//! Local-coordinate pointer input, pointer scope and activation (spec §4.4,
//! §6.2).

use yserver_protocol::x11::{ResourceId, SequenceNumber};

use super::{
    host::send_client_message_as_server,
    types::{RootlessInput, ToplevelId, ToplevelKind},
    wm::set_words,
};
use crate::{
    backend::Backend,
    core_loop::{HostInputEvent, InputOrigin, handle_host_input},
    host_x11::HostKeyEvent,
    resources::{ROOT_WINDOW, SERVER_OWNER},
    server::ServerState,
};

/// Which top-level the hit test may find (spec §4.4): the native system
/// decides which window is under the pointer.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum PointerScope {
    /// No host input yet: the plain X hit test.
    #[default]
    Anywhere,
    /// Over no X window: everything hits the root.
    Nowhere,
    /// Over this top-level's native window: only its subtree.
    Within(ResourceId),
}

/// Linux button codes for X buttons 1-10, the inverse of the XBackend map
/// (`portable/pointer.rs` `process_pointer_button`).
const BUTTON_CODES: [u16; 10] = [
    0x110, 0x112, 0x111, 0x180, 0x181, 0x182, 0x183, 0x113, 0x114, 0x115,
];

fn live(state: &ServerState, id: ToplevelId) -> Option<ResourceId> {
    let window = state.rootless.as_ref()?.live.get(&id).copied();
    if window.is_none() {
        log::debug!("rootless: input for stale id {id:?} dropped");
    }
    window
}

fn set_scope(state: &mut ServerState, scope: PointerScope) {
    if let Some(r) = state.rootless.as_mut() {
        r.scope = scope;
    }
}

fn motion(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: InputOrigin,
    (x, y): (i32, i32),
    time: u32,
) {
    let (px, py) = (
        i32::from(state.pointer_root.0),
        i32::from(state.pointer_root.1),
    );
    handle_host_input(
        state,
        backend,
        HostInputEvent::PointerMotion {
            origin,
            x,
            y,
            time,
            relative: false,
            dx: x - px,
            dy: y - py,
            motion_delta: None,
        },
    );
}

/// Convert host input to root-space input (spec §4.4) and forward it.
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn map_input(state: &mut ServerState, backend: &mut dyn Backend, input: RootlessInput) {
    if state.rootless.is_none() {
        return;
    }
    match input {
        RootlessInput::PointerEnter {
            origin,
            toplevel,
            local,
            time,
        }
        | RootlessInput::PointerMotionLocal {
            origin,
            toplevel,
            local,
            time,
        } => {
            let Some(window) = live(state, toplevel) else {
                return;
            };
            let bw = state
                .resources
                .window(window)
                .map_or(0, |w| i32::from(w.border_width));
            let (cx, cy) = state.resources.window_absolute_position(window);
            // root = content origin − (bw, bw) + ⌊local⌋: local (0,0) is the outer corner.
            let root = (
                cx - bw + local.0.floor() as i32,
                cy - bw + local.1.floor() as i32,
            );
            set_scope(state, PointerScope::Within(window));
            motion(state, backend, origin, root, time);
        }
        RootlessInput::PointerLeave { origin, time } => {
            set_scope(state, PointerScope::Nowhere);
            for (i, code) in BUTTON_CODES.iter().enumerate() {
                if state.buttons_down & (1 << i) != 0 {
                    handle_host_input(
                        state,
                        backend,
                        HostInputEvent::PointerButton {
                            origin,
                            button: *code,
                            pressed: false,
                            time,
                        },
                    );
                }
            }
            // A motion in place: the hit test now returns the root, so the
            // crossing code delivers LeaveNotify.
            let at = (
                i32::from(state.pointer_root.0),
                i32::from(state.pointer_root.1),
            );
            motion(state, backend, origin, at, time);
        }
        RootlessInput::PointerRelative {
            origin,
            delta,
            unaccelerated,
            time,
        } => {
            let Some(r) = state.rootless.as_mut().filter(|r| r.lock.held()) else {
                log::debug!("rootless: relative motion without a lock dropped");
                return;
            };
            let [across, down, raw_across, raw_down] = &mut r.lock.residue;
            let (dx, dy) = (
                super::lock::whole(across, delta.0),
                super::lock::whole(down, delta.1),
            );
            let (raw_dx, raw_dy) = (
                super::lock::whole(raw_across, unaccelerated.0),
                super::lock::whole(raw_down, unaccelerated.1),
            );
            let x = i32::from(state.pointer_root.0) + dx;
            let y = i32::from(state.pointer_root.1) + dy;
            handle_host_input(
                state,
                backend,
                HostInputEvent::PointerMotion {
                    origin,
                    x,
                    y,
                    time,
                    relative: true,
                    dx: raw_dx,
                    dy: raw_dy,
                    motion_delta: Some([delta.0, delta.1]),
                },
            );
        }
    }
}

/// SetInputFocus as the server: viewable check, then the core apply.
pub(crate) fn set_input_focus_as_server(
    state: &mut ServerState,
    window: ResourceId,
    revert_to: u8,
) {
    if crate::core_loop::process_request::core_focus_window_error(state, window).is_some() {
        return;
    }
    crate::core_loop::process_request::apply_core_input_focus(
        state,
        SERVER_OWNER,
        SequenceNumber(0),
        window,
        revert_to,
        0,
    );
}

/// `Activated` (spec §6.2): focus unless `WM_HINTS.input` is False, then
/// `WM_TAKE_FOCUS` if listed (ICCCM §4.1.7), then `_NET_ACTIVE_WINDOW`.
pub(super) fn activate(state: &mut ServerState, backend: &mut dyn Backend, id: ToplevelId) {
    let Some(window) = live(state, id) else {
        return;
    };
    let Some(t) = state
        .rootless
        .as_ref()
        .and_then(|r| r.toplevels.get(&window))
    else {
        return;
    };
    if t.kind != ToplevelKind::Managed {
        log::debug!("rootless: popups are never activated");
        return;
    }
    let (input, take_focus) = (t.info.input_hint, t.info.protocols.take_focus);
    if let Some(r) = state.rootless.as_mut() {
        r.active = Some(window);
    }
    if input != Some(false) {
        set_input_focus_as_server(state, window, 1); // RevertToPointerRoot
    }
    if take_focus {
        let atom = state.atoms.intern("WM_TAKE_FOCUS", false).0;
        // ICCCM §4.2.8: never CurrentTime.
        let time = state.timestamp_now().max(1);
        send_client_message_as_server(state, window, "WM_PROTOCOLS", [atom, time, 0, 0, 0]);
    }
    set_words(
        state,
        backend,
        ROOT_WINDOW,
        "_NET_ACTIVE_WINDOW",
        "WINDOW",
        &[window.0],
    );
}

/// The held keys, each with the origin that pressed it (the per-device
/// records), then any key only the master holds (`NestedHost`).
fn held_keys(state: &ServerState) -> Vec<(u8, InputOrigin)> {
    let mut held: Vec<(u8, InputOrigin)> = state
        .key_down_by_device
        .values()
        .chain(state.unpublished_keyboard_keys_down.values())
        .flat_map(|keys| keys.iter().map(|(k, o)| (*k, *o)))
        .collect();
    held.sort_unstable_by_key(|(k, _)| *k);
    for keycode in 8..=255u8 {
        let down = state.keys_down[usize::from(keycode / 8)] & (1 << (keycode % 8)) != 0;
        if down && !held.iter().any(|(k, _)| *k == keycode) {
            held.push((keycode, InputOrigin::NestedHost));
        }
    }
    held
}

/// `Deactivated`: release every pressed key on the device that pressed it
/// (XWin `winKeybdReleaseKeys`), focus None, `_NET_ACTIVE_WINDOW = None`.
pub(super) fn deactivate(state: &mut ServerState, backend: &mut dyn Backend) {
    if let Some(r) = state.rootless.as_mut() {
        r.active = None;
    }
    let time = state.timestamp_now();
    let (rx, ry) = state.pointer_root;
    for (keycode, origin) in held_keys(state) {
        handle_host_input(
            state,
            backend,
            HostInputEvent::Key(HostKeyEvent {
                origin,
                pressed: false,
                keycode,
                time,
                root_x: rx,
                root_y: ry,
                event_x: rx,
                event_y: ry,
                state: 0,
            }),
        );
    }
    set_input_focus_as_server(state, ResourceId(0), 0);
    set_words(
        state,
        backend,
        ROOT_WINDOW,
        "_NET_ACTIVE_WINDOW",
        "WINDOW",
        &[0],
    );
}
