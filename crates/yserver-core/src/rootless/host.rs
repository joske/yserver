//! Host → core requests (spec §4.4, §6.4) and the typed server actions they
//! use instead of synthesized wire bodies.

use yserver_protocol::x11::{
    self, ClientByteOrder, ConfigureWindowRequest, ResourceId, SequenceNumber,
};

use super::{
    CoordinatePolicy, NetWmState,
    acks::{ConfigureSource, note_policy_fields},
    types::{GeometryFields, HostRequest, RootlessEvent, ToplevelId, ToplevelKind},
    wm::set_words,
};
use crate::{
    backend::Backend,
    resources::{ROOT_WINDOW, SERVER_OWNER},
    rootless::SizeHints,
    server::ServerState,
};

/// ConfigureWindow as the server (spec §4.4), through the client path's apply.
pub(crate) fn configure_window_as_server(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    request: ConfigureWindowRequest,
    source: ConfigureSource,
) {
    let _ = crate::core_loop::process_request::configure_window_apply(
        state,
        backend,
        None,
        SERVER_OWNER,
        SequenceNumber(0),
        request,
        source,
    );
}

/// A format-32 ClientMessage with event mask 0: X11 SendEvent delivers it to
/// the destination window's creator.
pub(crate) fn send_client_message_as_server(
    state: &mut ServerState,
    window: ResourceId,
    r#type: &str,
    data: [u32; 5],
) {
    let Ok(targets) = crate::core_loop::process_request::send_event_recipients(state, window, 0, 0)
    else {
        return;
    };
    let r#type = state.atoms.intern(r#type, false);
    let mut event = [0u8; 32];
    event[0] = 33 | 0x80;
    event[1] = 32;
    event[4..8].copy_from_slice(&window.0.to_le_bytes());
    event[8..12].copy_from_slice(&r#type.0.to_le_bytes());
    for (i, v) in data.iter().enumerate() {
        event[12 + 4 * i..16 + 4 * i].copy_from_slice(&v.to_le_bytes());
    }
    let _dropped = crate::core_loop::fanout::fanout_raw_event_to_clients(
        state,
        &targets,
        &event,
        ClientByteOrder::LittleEndian,
    );
}

/// KillClient of `window`'s owner (`misc.rs` KillClient, other-client branch).
pub(crate) fn kill_client_as_server(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    window: ResourceId,
) {
    let Some(owner) = state.resources.window(window).map(|w| w.owner) else {
        return;
    };
    if owner == SERVER_OWNER {
        return;
    }
    if state.zombie_clients.contains_key(&owner.0) {
        // On `Err` the zombie stays recorded; the server is shutting down.
        if crate::core_loop::process_disconnect::destroy_zombie_resources(state, backend, owner)
            .is_ok()
        {
            state.zombie_clients.remove(&owner.0);
        }
    } else if state.clients.contains_key(&owner.0) {
        crate::core_loop::process_disconnect::process_disconnect(state, backend, owner);
    }
}

/// ICCCM §4.1.2.3: min/max, then `base + k·inc` (base defaults to min, min to
/// base).
fn clamp_dimension(
    value: u32,
    min: Option<u32>,
    max: Option<u32>,
    base: Option<u32>,
    inc: Option<u32>,
) -> u32 {
    let (min, base) = (min.or(base), base.or(min));
    let mut v = value;
    if let Some(min) = min {
        v = v.max(min);
    }
    if let Some(max) = max.filter(|m| *m > 0) {
        v = v.min(max);
    }
    if let Some(inc) = inc.filter(|i| *i > 0) {
        let base = base.unwrap_or(0);
        if v > base {
            v = base + (v - base) / inc * inc;
        }
    }
    v.max(1)
}

fn clamp_size(hints: &SizeHints, width: u32, height: u32) -> (u32, u32) {
    let pick = |p: Option<(u32, u32)>, h: bool| p.map(|(w, ht)| if h { ht } else { w });
    (
        clamp_dimension(
            width,
            pick(hints.min, false),
            pick(hints.max, false),
            pick(hints.base, false),
            pick(hints.inc, false),
        ),
        clamp_dimension(
            height,
            pick(hints.min, true),
            pick(hints.max, true),
            pick(hints.base, true),
            pick(hints.inc, true),
        ),
    )
}

fn live_window(state: &ServerState, id: ToplevelId) -> Option<ResourceId> {
    let window = state.rootless.as_ref()?.live.get(&id).copied();
    if window.is_none() {
        log::debug!("rootless: host request for stale id {id:?} dropped");
    }
    window
}

fn configure(window: ResourceId) -> ConfigureWindowRequest {
    ConfigureWindowRequest {
        window,
        value_mask: 0,
        x: None,
        y: None,
        width: None,
        height: None,
        border_width: None,
        sibling: None,
        stack_mode: None,
    }
}

/// ICCCM §4.1.5: after a WM move without a resize, the client gets a
/// synthetic ConfigureNotify in root coordinates (XWin
/// `winmultiwindowwm.c:1294-1314`).
fn synthetic_configure_notify(state: &mut ServerState, window: ResourceId) {
    let Some(w) = state.resources.window(window) else {
        return;
    };
    let geometry = x11::Geometry {
        root: ROOT_WINDOW,
        x: w.x,
        y: w.y,
        width: w.width,
        height: w.height,
        border_width: w.border_width,
        depth: w.depth,
    };
    let override_redirect = w.override_redirect;
    let above = state.resources.configure_notify_above_sibling(window);
    let _dropped = crate::core_loop::fanout::emit_window_event_to_state(
        state,
        window,
        0x0002_0000, // StructureNotify
        |buf, seq, order| {
            let start = buf.len();
            x11::encode_configure_notify_event(
                buf,
                seq,
                order,
                window,
                window,
                above,
                geometry,
                override_redirect,
            );
            buf[start] |= 0x80;
        },
    );
}

fn net_wm_state_atoms(state: &mut ServerState, s: NetWmState) -> Vec<u32> {
    [
        (s.fullscreen, "_NET_WM_STATE_FULLSCREEN"),
        (s.maximized_vert, "_NET_WM_STATE_MAXIMIZED_VERT"),
        (s.maximized_horz, "_NET_WM_STATE_MAXIMIZED_HORZ"),
        (s.above, "_NET_WM_STATE_ABOVE"),
        (s.below, "_NET_WM_STATE_BELOW"),
        (s.hidden, "_NET_WM_STATE_HIDDEN"),
        (s.skip_taskbar, "_NET_WM_STATE_SKIP_TASKBAR"),
        (s.modal, "_NET_WM_STATE_MODAL"),
    ]
    .into_iter()
    .filter(|(on, _)| *on)
    .map(|(_, name)| state.atoms.intern(name, false).0)
    .collect()
}

/// Apply one host request on the core thread (spec §4.4). Requests naming an
/// id that is not live are dropped.
#[allow(clippy::too_many_lines)]
pub(super) fn apply(state: &mut ServerState, backend: &mut dyn Backend, request: HostRequest) {
    let Some(policy) = state.rootless.as_ref().map(super::Rootless::policy) else {
        return;
    };
    match request {
        HostRequest::Moved { id, x, y, serial } => {
            if policy == CoordinatePolicy::Opaque {
                log::debug!("rootless: Moved under the Opaque policy dropped");
                return;
            }
            let Some(window) = live_window(state, id) else {
                return;
            };
            let clamp =
                |v: i32| i16::try_from(v).unwrap_or(if v < 0 { i16::MIN } else { i16::MAX });
            let mut req = configure(window);
            req.value_mask = 0x3;
            (req.x, req.y) = (Some(clamp(x)), Some(clamp(y)));
            configure_window_as_server(state, backend, req, ConfigureSource::Host(serial));
            synthetic_configure_notify(state, window);
        }
        HostRequest::Resized {
            id,
            width,
            height,
            serial,
        } => {
            let Some(window) = live_window(state, id) else {
                return;
            };
            let Some(bw) = state
                .resources
                .window(window)
                .map(|w| u32::from(w.border_width))
            else {
                return;
            };
            let content = |v: u16| u32::from(v).saturating_sub(2 * bw).max(1);
            let (want_w, want_h) = (content(width), content(height));
            let hints = state
                .rootless
                .as_ref()
                .and_then(|r| r.toplevels.get(&window))
                .map(|t| t.info.size_hints)
                .unwrap_or_default();
            let (w, h) = clamp_size(&hints, want_w, want_h);
            let to_u16 = |v: u32| u16::try_from(v).unwrap_or(u16::MAX);
            let mut req = configure(window);
            req.value_mask = 0xc;
            (req.width, req.height) = (Some(to_u16(w)), Some(to_u16(h)));
            configure_window_as_server(state, backend, req, ConfigureSource::Host(serial));
            let mut clamped = GeometryFields::empty();
            if w != want_w {
                clamped |= GeometryFields::WIDTH;
            }
            if h != want_h {
                clamped |= GeometryFields::HEIGHT;
            }
            note_policy_fields(state, window, clamped);
        }
        HostRequest::Restacked { id, above, serial } => {
            let Some(window) = live_window(state, id) else {
                return;
            };
            let mut req = configure(window);
            match above {
                Some(above) => {
                    let Some(sibling) = live_window(state, above) else {
                        return;
                    };
                    req.value_mask = 0x60;
                    (req.sibling, req.stack_mode) = (Some(sibling), Some(0)); // Above
                }
                None => {
                    req.value_mask = 0x40;
                    req.stack_mode = Some(1); // Below
                }
            }
            configure_window_as_server(state, backend, req, ConfigureSource::Host(serial));
        }
        HostRequest::CloseRequested { id } => {
            let Some(window) = live_window(state, id) else {
                return;
            };
            let listed = state
                .rootless
                .as_ref()
                .and_then(|r| r.toplevels.get(&window))
                .is_some_and(|t| t.info.protocols.delete_window);
            // Decision 12: a close request never kills.
            if listed {
                let delete = state.atoms.intern("WM_DELETE_WINDOW", false).0;
                let time = state.timestamp_now();
                send_client_message_as_server(
                    state,
                    window,
                    "WM_PROTOCOLS",
                    [delete, time, 0, 0, 0],
                );
            }
        }
        HostRequest::ForceKill { id } => {
            if let Some(window) = live_window(state, id) {
                kill_client_as_server(state, backend, window);
            }
        }
        HostRequest::StateChanged { id, state: net } => {
            let Some(window) = live_window(state, id) else {
                return;
            };
            let atoms = net_wm_state_atoms(state, net);
            set_words(state, backend, window, "_NET_WM_STATE", "ATOM", &atoms);
            let managed = state
                .rootless
                .as_ref()
                .and_then(|r| r.toplevels.get(&window))
                .is_some_and(|t| t.kind == ToplevelKind::Managed);
            if managed {
                // ICCCM §4.1.3.1: Iconic 3, Normal 1; minimize keeps the window mapped (decision 3).
                let wm_state = if net.hidden { 3 } else { 1 };
                set_words(
                    state,
                    backend,
                    window,
                    "WM_STATE",
                    "WM_STATE",
                    &[wm_state, 0],
                );
            }
        }
        HostRequest::ScreenLayout { outputs } => {
            // D7: one host output until RL2's per-output RandR.
            let [output] = outputs.as_slice() else {
                log::warn!(
                    "rootless: ScreenLayout with {} outputs rejected (one until RL2)",
                    outputs.len()
                );
                return;
            };
            crate::core_loop::host_screen_resized(state, backend, output.width, output.height);
            let root = (state.randr.screen_width, state.randr.screen_height);
            if let Some(r) = state.rootless.as_mut() {
                r.pending.push(RootlessEvent::ScreenChanged { root });
            }
        }
        HostRequest::Activated { id } => super::seat::activate(state, backend, id),
        HostRequest::Deactivated => super::seat::deactivate(state, backend),
    }
}
