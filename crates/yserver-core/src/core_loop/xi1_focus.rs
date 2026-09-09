//! XI 1.x per-device focus: SetDeviceFocus / GetDeviceFocus state and
//! DeviceFocusIn / DeviceFocusOut event generation.
//!
//! Ports Xorg's `DeviceFocusEvents` (dix/enterleave.c:1419-1542) over
//! yserver's window tree. The focus value is stored raw — `0` None,
//! `1` PointerRoot, `3` FollowKeyboard (XI.h), else a window xid — and
//! `FollowKeyboard` resolves through the core keyboard focus at event
//! generation and key-delivery time, mirroring `FollowKeyboardWin`
//! (dix/events.c:4924-4926).
//!
//! Focus reverts (Xi/exevents.c `DeleteDeviceFromAnyExtEvents`) fire
//! when the focus window becomes unviewable: unmap, destroy, and
//! client-disconnect teardown all funnel through
//! [`revert_unviewable_focus`] / [`revert_focus_for_dying_subtree`].

use yserver_protocol::x11::{ClientId, ResourceId};

use crate::{
    core_loop::fanout::fanout_event_to_clients,
    resources::{MapState, ROOT_WINDOW},
    server::{ServerState, Xi1DeviceFocus, Xi1FocusRoute},
};

/// Raw focus specials (XI.h / X.h).
pub const FOCUS_NONE: u32 = 0;
pub const FOCUS_POINTER_ROOT: u32 = 1;
pub const FOCUS_FOLLOW_KEYBOARD: u32 = 3;

/// RevertTo values (X.h `RevertToNone/PointerRoot/Parent` +
/// XI.h `RevertToFollowKeyboard`).
pub const REVERT_TO_NONE: u8 = 0;
pub const REVERT_TO_POINTER_ROOT: u8 = 1;
pub const REVERT_TO_PARENT: u8 = 2;
pub const REVERT_TO_FOLLOW_KEYBOARD: u8 = 3;

/// Focus-event detail codes beyond the crossing details 0..=4
/// (X.h NotifyPointer / NotifyPointerRoot / NotifyDetailNone).
const NOTIFY_ANCESTOR: u8 = 0;
const NOTIFY_VIRTUAL: u8 = 1;
const NOTIFY_INFERIOR: u8 = 2;
const NOTIFY_NONLINEAR: u8 = 3;
const NOTIFY_NONLINEAR_VIRTUAL: u8 = 4;
const NOTIFY_POINTER: u8 = 5;
const NOTIFY_POINTER_ROOT: u8 = 6;
const NOTIFY_DETAIL_NONE: u8 = 7;

/// Focus-event `mode` (X.h).
pub const NOTIFY_NORMAL: u8 = 0;
pub const NOTIFY_WHILE_GRABBED: u8 = 3;

/// A device focus target with specials resolved to delivery semantics.
/// `FollowKeyboard` is resolved away by [`resolve_focus`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Xi1FocusTarget {
    None,
    PointerRoot,
    Window(ResourceId),
}

/// Current focus record for `deviceid` (default PointerRoot).
#[must_use]
pub fn device_focus(state: &ServerState, deviceid: u16) -> Xi1DeviceFocus {
    state
        .xi1_device_focus
        .get(&deviceid)
        .copied()
        .unwrap_or_default()
}

/// Resolve a raw focus value to a delivery target. `FollowKeyboard`
/// follows the core keyboard focus (Xorg `FollowKeyboardWin` →
/// `inputInfo.keyboard->focus->win`), which carries its own
/// None/PointerRoot/window distinction in `state.core_focus`.
#[must_use]
pub fn resolve_focus(state: &ServerState, raw: u32) -> Xi1FocusTarget {
    match raw {
        FOCUS_NONE => Xi1FocusTarget::None,
        FOCUS_POINTER_ROOT => Xi1FocusTarget::PointerRoot,
        FOCUS_FOLLOW_KEYBOARD => match state.core_focus.raw {
            0 => Xi1FocusTarget::None,
            1 => Xi1FocusTarget::PointerRoot,
            w => Xi1FocusTarget::Window(ResourceId(w)),
        },
        w => Xi1FocusTarget::Window(ResourceId(w)),
    }
}

/// The focus-event delivery mode for `deviceid`: NotifyWhileGrabbed
/// when an XI1 device grab is active, NotifyNormal otherwise (Xorg
/// dix/events.c:4923).
#[must_use]
pub fn focus_event_mode(state: &ServerState, deviceid: u16) -> u8 {
    if state.xi1_active_grabs.contains_key(&deviceid) {
        NOTIFY_WHILE_GRABBED
    } else {
        NOTIFY_NORMAL
    }
}

/// Wrap-aware X11 timestamp ordering: true when `a` is strictly later
/// than `b`.
#[must_use]
pub fn time_after(a: u32, b: u32) -> bool {
    a != b && a.wrapping_sub(b) < 0x8000_0000
}

/// Store a new focus for `deviceid` and emit the DeviceFocusIn/Out
/// transition events. The caller has already validated the request
/// (device, revert_to, window viewability) and the timestamp.
/// `raw_focus` keeps the FollowKeyboard sentinel; events are computed
/// over the resolved targets (Xorg dix/events.c:4924-4937).
pub fn set_device_focus(
    state: &mut ServerState,
    deviceid: u16,
    raw_focus: u32,
    revert_to: u8,
    time: u32,
) {
    let prev = device_focus(state, deviceid);
    let from = resolve_focus(state, prev.focus);
    let to = resolve_focus(state, raw_focus);
    let mode = focus_event_mode(state, deviceid);
    device_focus_events(state, deviceid, from, to, mode);
    state.xi1_device_focus.insert(
        deviceid,
        Xi1DeviceFocus {
            focus: raw_focus,
            revert_to,
            time,
        },
    );
}

/// Compute the [`Xi1FocusRoute`] + natural target for a keyboard
/// device event — the XI1 port of Xorg `DeliverFocusedEvent`
/// (dix/events.c:4202-4222). Returns `(natural_target, route)`.
#[must_use]
pub fn key_delivery_route(state: &ServerState, deviceid: u16) -> (ResourceId, Xi1FocusRoute) {
    let sprite = crate::core_loop::key_fanout::deepest_window_at_pointer(state);
    match resolve_focus(state, device_focus(state, deviceid).focus) {
        Xi1FocusTarget::None => (sprite, Xi1FocusRoute::Drop),
        Xi1FocusTarget::PointerRoot => (sprite, Xi1FocusRoute::Walk),
        Xi1FocusTarget::Window(f) => {
            if f == sprite || is_ancestor(state, f, sprite) {
                (sprite, Xi1FocusRoute::WalkUpTo(f))
            } else {
                (f, Xi1FocusRoute::WindowOnly(f))
            }
        }
    }
}

/// Revert any device focus whose window is no longer viewable. Hook
/// for UnmapWindow / UnmapSubwindows (after map-state updates land).
pub fn revert_unviewable_focus(state: &mut ServerState) {
    let devices: Vec<u16> = state.xi1_device_focus.keys().copied().collect();
    for dev in devices {
        let f = device_focus(state, dev);
        if let Xi1FocusTarget::Window(w) = resolve_raw_window_only(f.focus)
            && !window_viewable(state, w)
        {
            do_revert(state, dev, w, None);
        }
    }
}

/// Revert any device focus inside a window subtree that is about to be
/// destroyed. Must run while the tree is still intact (before resource
/// removal) so the RevertToParent walk can find the surviving
/// ancestor. Hook for DestroyWindow / DestroySubwindows / disconnect
/// teardown.
pub fn revert_focus_for_dying_subtree(state: &mut ServerState, dying_root: ResourceId) {
    if dying_root == ROOT_WINDOW {
        return;
    }
    let devices: Vec<u16> = state.xi1_device_focus.keys().copied().collect();
    for dev in devices {
        let f = device_focus(state, dev);
        if let Xi1FocusTarget::Window(w) = resolve_raw_window_only(f.focus)
            && in_subtree(state, w, dying_root)
        {
            do_revert(state, dev, w, Some(dying_root));
        }
    }
}

/// Raw → target without FollowKeyboard resolution: revert checks care
/// about the *stored* window, not what FollowKeyboard points at (a
/// FollowKeyboard focus never reverts — the core focus does).
fn resolve_raw_window_only(raw: u32) -> Xi1FocusTarget {
    match raw {
        FOCUS_NONE | FOCUS_FOLLOW_KEYBOARD => Xi1FocusTarget::None,
        FOCUS_POINTER_ROOT => Xi1FocusTarget::PointerRoot,
        w => Xi1FocusTarget::Window(ResourceId(w)),
    }
}

/// Apply the revert for device `dev` whose focus window `w` became
/// unviewable, per Xi/exevents.c `DeleteDeviceFromAnyExtEvents`
/// (lines 3044-3092). `dying_subtree` marks a subtree about to be
/// destroyed (its windows can't host the reverted focus).
fn do_revert(state: &mut ServerState, dev: u16, w: ResourceId, dying_subtree: Option<ResourceId>) {
    let f = device_focus(state, dev);
    let mode = focus_event_mode(state, dev);
    let from = Xi1FocusTarget::Window(w);
    let (to_target, new_raw, new_revert) = match f.revert_to {
        REVERT_TO_PARENT => {
            let parent = closest_viable_ancestor(state, w, dying_subtree);
            (
                Xi1FocusTarget::Window(parent),
                parent.0,
                REVERT_TO_NONE, // Xi/exevents.c:3069
            )
        }
        REVERT_TO_POINTER_ROOT => (Xi1FocusTarget::PointerRoot, FOCUS_POINTER_ROOT, f.revert_to),
        REVERT_TO_FOLLOW_KEYBOARD => (
            resolve_focus(state, FOCUS_FOLLOW_KEYBOARD),
            FOCUS_FOLLOW_KEYBOARD,
            f.revert_to,
        ),
        _ => (Xi1FocusTarget::None, FOCUS_NONE, f.revert_to),
    };
    device_focus_events(state, dev, from, to_target, mode);
    state.xi1_device_focus.insert(
        dev,
        Xi1DeviceFocus {
            focus: new_raw,
            revert_to: new_revert,
            time: f.time,
        },
    );
    log::debug!(
        "xi1_focus: device {dev} focus 0x{:x} reverted (revert_to={}) -> 0x{new_raw:x}",
        w.0,
        f.revert_to,
    );
}

/// First ancestor of `w` that is viewable and outside `dying_subtree`
/// (Xorg's `while (!parent->realized)` walk, Xi/exevents.c:3060-3065).
/// Falls back to the root window.
fn closest_viable_ancestor(
    state: &ServerState,
    w: ResourceId,
    dying_subtree: Option<ResourceId>,
) -> ResourceId {
    let mut current = w;
    for _ in 0..256 {
        let Some(parent) = state.resources.window(current).map(|win| win.parent) else {
            return ROOT_WINDOW;
        };
        if parent == current {
            return ROOT_WINDOW;
        }
        let dying = dying_subtree.is_some_and(|root| in_subtree(state, parent, root));
        if !dying && (parent == ROOT_WINDOW || window_viewable(state, parent)) {
            return parent;
        }
        current = parent;
    }
    ROOT_WINDOW
}

/// Effective viewability: the window and every ancestor must be
/// mapped. `resources.unmap_window` only flips the unmapped window's
/// own state, so the ancestor walk is load-bearing for "ancestor
/// unmapped ⇒ focus window unviewable".
fn window_viewable(state: &ServerState, w: ResourceId) -> bool {
    let Some(win) = state.resources.window(w) else {
        return false;
    };
    if win.map_state != MapState::Viewable {
        return false;
    }
    let mut current = w;
    for _ in 0..256 {
        let Some(win) = state.resources.window(current) else {
            return false;
        };
        if win.map_state == MapState::Unmapped {
            return false;
        }
        if win.parent == current {
            return true;
        }
        current = win.parent;
    }
    false
}

/// True when `root` is `w` or one of its ancestors.
fn in_subtree(state: &ServerState, w: ResourceId, root: ResourceId) -> bool {
    let mut current = w;
    for _ in 0..256 {
        if current == root {
            return true;
        }
        match state.resources.window(current).map(|win| win.parent) {
            Some(parent) if parent != current => current = parent,
            _ => return false,
        }
    }
    false
}

/// True when `a` is a strict ancestor of `b` (Xorg `IsParent`).
pub(crate) fn is_ancestor(state: &ServerState, a: ResourceId, b: ResourceId) -> bool {
    if a == b {
        return false;
    }
    let mut current = b;
    for _ in 0..256 {
        match state.resources.window(current).map(|win| win.parent) {
            Some(parent) if parent != current => {
                if parent == a {
                    return true;
                }
                current = parent;
            }
            _ => return false,
        }
    }
    false
}

/// Port of Xorg `DeviceFocusEvents` (dix/enterleave.c:1419-1542),
/// single screen. Emits the DeviceFocusOut/DeviceFocusIn sequence for
/// moving device `deviceid`'s focus from `from` to `to`.
pub fn device_focus_events(
    state: &mut ServerState,
    deviceid: u16,
    from: Xi1FocusTarget,
    to: Xi1FocusTarget,
    mode: u8,
) {
    use Xi1FocusTarget::{None as FNone, PointerRoot, Window};
    if from == to {
        return;
    }
    let sprite = crate::core_loop::key_fanout::deepest_window_at_pointer(state);
    let root = ROOT_WINDOW;
    // Details for the root-window events when from/to is a special.
    let out_detail = if from == FNone {
        NOTIFY_DETAIL_NONE
    } else {
        NOTIFY_POINTER_ROOT
    };
    let in_detail = if to == FNone {
        NOTIFY_DETAIL_NONE
    } else {
        NOTIFY_POINTER_ROOT
    };

    match (from, to) {
        (_, FNone | PointerRoot) => {
            match from {
                FNone | PointerRoot => {
                    if from == PointerRoot {
                        emit_device_focus(state, deviceid, false, mode, NOTIFY_POINTER, sprite);
                        focus_out_chain(state, deviceid, sprite, Some(root), mode, NOTIFY_POINTER);
                    }
                    emit_device_focus(state, deviceid, false, mode, out_detail, root);
                }
                Window(f) => {
                    if is_ancestor(state, f, sprite) {
                        emit_device_focus(state, deviceid, false, mode, NOTIFY_POINTER, sprite);
                        focus_out_chain(state, deviceid, sprite, Some(f), mode, NOTIFY_POINTER);
                    }
                    emit_device_focus(state, deviceid, false, mode, NOTIFY_NONLINEAR, f);
                    // "next call catches the root too" (enterleave.c:1461).
                    focus_out_chain(state, deviceid, f, None, mode, NOTIFY_NONLINEAR_VIRTUAL);
                }
            }
            emit_device_focus(state, deviceid, true, mode, in_detail, root);
            if to == PointerRoot {
                focus_in_chain(state, deviceid, root, sprite, mode, NOTIFY_POINTER);
                emit_device_focus(state, deviceid, true, mode, NOTIFY_POINTER, sprite);
            }
        }
        (FNone | PointerRoot, Window(t)) => {
            if from == PointerRoot {
                emit_device_focus(state, deviceid, false, mode, NOTIFY_POINTER, sprite);
                focus_out_chain(state, deviceid, sprite, Some(root), mode, NOTIFY_POINTER);
            }
            emit_device_focus(state, deviceid, false, mode, out_detail, root);
            if t != root {
                focus_in_chain(state, deviceid, root, t, mode, NOTIFY_NONLINEAR_VIRTUAL);
            }
            emit_device_focus(state, deviceid, true, mode, NOTIFY_NONLINEAR, t);
            if is_ancestor(state, t, sprite) {
                focus_in_chain(state, deviceid, t, sprite, mode, NOTIFY_POINTER);
            }
        }
        (Window(f), Window(t)) => {
            if is_ancestor(state, t, f) {
                emit_device_focus(state, deviceid, false, mode, NOTIFY_ANCESTOR, f);
                focus_out_chain(state, deviceid, f, Some(t), mode, NOTIFY_VIRTUAL);
                emit_device_focus(state, deviceid, true, mode, NOTIFY_INFERIOR, t);
                if is_ancestor(state, t, sprite)
                    && sprite != f
                    && !is_ancestor(state, f, sprite)
                    && !is_ancestor(state, sprite, f)
                {
                    focus_in_chain(state, deviceid, t, sprite, mode, NOTIFY_POINTER);
                }
            } else if is_ancestor(state, f, t) {
                if is_ancestor(state, f, sprite)
                    && sprite != f
                    && !is_ancestor(state, t, sprite)
                    && !is_ancestor(state, sprite, t)
                {
                    emit_device_focus(state, deviceid, false, mode, NOTIFY_POINTER, sprite);
                    focus_out_chain(state, deviceid, sprite, Some(f), mode, NOTIFY_POINTER);
                }
                emit_device_focus(state, deviceid, false, mode, NOTIFY_INFERIOR, f);
                focus_in_chain(state, deviceid, f, t, mode, NOTIFY_VIRTUAL);
                emit_device_focus(state, deviceid, true, mode, NOTIFY_ANCESTOR, t);
            } else {
                let common = common_ancestor(state, f, t);
                if is_ancestor(state, f, sprite) {
                    focus_out_chain(state, deviceid, sprite, Some(f), mode, NOTIFY_POINTER);
                }
                emit_device_focus(state, deviceid, false, mode, NOTIFY_NONLINEAR, f);
                if f != root {
                    focus_out_chain(
                        state,
                        deviceid,
                        f,
                        Some(common),
                        mode,
                        NOTIFY_NONLINEAR_VIRTUAL,
                    );
                }
                if t != root {
                    focus_in_chain(state, deviceid, common, t, mode, NOTIFY_NONLINEAR_VIRTUAL);
                }
                emit_device_focus(state, deviceid, true, mode, NOTIFY_NONLINEAR, t);
                if is_ancestor(state, t, sprite) {
                    focus_in_chain(state, deviceid, t, sprite, mode, NOTIFY_POINTER);
                }
            }
        }
    }
}

/// DeviceFocusOut on each window from `child`'s parent up to (not
/// including) `ancestor`; `None` walks through and including the root
/// (Xorg `DeviceFocusOutEvents`, enterleave.c:843-853 with
/// `ancestor == NullWindow`).
fn focus_out_chain(
    state: &mut ServerState,
    deviceid: u16,
    child: ResourceId,
    ancestor: Option<ResourceId>,
    mode: u8,
    detail: u8,
) {
    let mut current = child;
    for _ in 0..256 {
        let Some(parent) = state.resources.window(current).map(|w| w.parent) else {
            return;
        };
        if Some(parent) == ancestor {
            return;
        }
        if parent == current {
            return; // walked past the root
        }
        emit_device_focus(state, deviceid, false, mode, detail, parent);
        current = parent;
    }
}

/// DeviceFocusIn on each window strictly between `ancestor` and
/// `child`, top-down (Xorg `DeviceFocusInEvents`, recursive,
/// enterleave.c:860-870).
fn focus_in_chain(
    state: &mut ServerState,
    deviceid: u16,
    ancestor: ResourceId,
    child: ResourceId,
    mode: u8,
    detail: u8,
) {
    let mut chain: Vec<ResourceId> = Vec::new();
    let mut current = child;
    for _ in 0..256 {
        let Some(parent) = state.resources.window(current).map(|w| w.parent) else {
            break;
        };
        if parent == ancestor || parent == current {
            break;
        }
        chain.push(parent);
        current = parent;
    }
    for w in chain.into_iter().rev() {
        emit_device_focus(state, deviceid, true, mode, detail, w);
    }
}

/// Lowest common ancestor of `a` and `b` (Xorg `CommonAncestor`);
/// falls back to the root window.
fn common_ancestor(state: &ServerState, a: ResourceId, b: ResourceId) -> ResourceId {
    let mut a_chain: Vec<ResourceId> = Vec::new();
    let mut current = a;
    for _ in 0..256 {
        a_chain.push(current);
        match state.resources.window(current).map(|w| w.parent) {
            Some(parent) if parent != current => current = parent,
            _ => break,
        }
    }
    current = b;
    for _ in 0..256 {
        if a_chain.contains(&current) && current != a && current != b {
            return current;
        }
        match state.resources.window(current).map(|w| w.parent) {
            Some(parent) if parent != current => current = parent,
            _ => break,
        }
    }
    ROOT_WINDOW
}

/// Deliver one DeviceFocusIn/Out to the clients that selected the
/// matching `XEventClass` on `window` via SelectExtensionEvent. Focus
/// events do not propagate up the tree (Xorg `DeliverEventsToWindow`
/// with `DeviceFocusChangeMask` on the single window).
///
/// Every DeviceFocusIn is followed by a DeviceStateNotify snapshot to
/// the same window (Xorg `DeviceFocusEvent` tail,
/// dix/enterleave.c:835-836) — gated by its own event-class selection,
/// NOT by the focus selection, so it fires even with zero focus
/// subscribers (XTS XI/Miscellaneous selects only DeviceStateNotify).
pub(crate) fn emit_device_focus(
    state: &mut ServerState,
    deviceid: u16,
    focus_in: bool,
    mode: u8,
    detail: u8,
    window: ResourceId,
) {
    let offset = if focus_in {
        crate::xinput::XI_DEVICE_FOCUS_IN_OFFSET
    } else {
        crate::xinput::XI_DEVICE_FOCUS_OUT_OFFSET
    };
    let event_type = crate::server::XI_FIRST_EVENT + offset;
    let class = (u32::from(deviceid) << 8) | u32::from(event_type);
    let targets: Vec<ClientId> = state
        .clients
        .iter()
        .filter(|(_, c)| {
            c.xi1_window_event_classes
                .get(&window)
                .is_some_and(|set| set.contains(&class))
        })
        .map(|(id, _)| ClientId(*id))
        .collect();
    if !targets.is_empty() {
        let time = state.timestamp_now();
        log::debug!(
            "xi1_focus: DeviceFocus{} window=0x{:x} detail={detail} mode={mode} -> {} client(s)",
            if focus_in { "In" } else { "Out" },
            window.0,
            targets.len(),
        );
        let _dropped = fanout_event_to_clients(state, &targets, |buf, seq, order| {
            crate::xinput::encode_xi1_device_focus_event(
                buf, order, event_type, detail, seq, time, window.0, mode, deviceid,
            );
        });
    }
    if focus_in {
        crate::core_loop::xi1_state_notify::deliver_state_notify(state, deviceid, window);
    }
}

/// Emit XI 1.x `DeviceMappingNotify` to every client that selected the
/// event class for `deviceid` *other than* `originator`. Class encoding:
/// `(deviceid << 8) | type` where
/// `type = XI_FIRST_EVENT + XI_DEVICE_MAPPING_NOTIFY_OFFSET`.
/// Selection lives in `client.xi1_event_classes` (server-wide, not
/// window-scoped — mapping is a per-device property, not a window
/// property, so Xorg `Xi/exevents.c::SendMappingNotify` sends it to
/// every client that picked the class on any window).
///
/// `originator` is the client that sent the request that triggered the
/// notify. The caller is expected to splice the event into `originator`'s
/// reply buffer (event-after-reply on the wire), so we don't fanout to
/// it here — doing so would either duplicate the event or land it
/// before the reply, which xts5 SetDeviceButtonMapping rejects with
/// `wanted REPLY got EVENT`.
///
/// `request_kind` is the mapping flavour (0=Modifier, 1=Keyboard,
/// 2=Pointer); `first_keycode`/`count` are non-zero only for the
/// Keyboard variant.
pub(crate) fn emit_device_mapping_notify(
    state: &mut ServerState,
    originator: ClientId,
    deviceid: u16,
    request_kind: u8,
    first_keycode: u8,
    count: u8,
) {
    let event_type = crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_MAPPING_NOTIFY_OFFSET;
    let class = (u32::from(deviceid) << 8) | u32::from(event_type);
    let targets: Vec<ClientId> = state
        .clients
        .iter()
        .filter(|(id, _)| **id != originator.0)
        .filter(|(_, c)| c.xi1_event_classes.contains(&class))
        .map(|(id, _)| ClientId(*id))
        .collect();
    if targets.is_empty() {
        return;
    }
    let time = state.timestamp_now();
    log::debug!(
        "xi1_mapping_notify: device={deviceid} request_kind={request_kind} \
         first_keycode={first_keycode} count={count} -> {} client(s)",
        targets.len(),
    );
    #[allow(clippy::cast_possible_truncation)]
    let device_byte = deviceid as u8;
    let _dropped = fanout_event_to_clients(state, &targets, |buf, seq, order| {
        crate::xinput::encode_xi1_device_mapping_notify(
            buf,
            order,
            event_type,
            device_byte,
            seq,
            time,
            request_kind,
            first_keycode,
            count,
        );
    });
}

/// True if `client` has selected the XI 1.x `DeviceMappingNotify` class
/// for `deviceid`. Use in handlers that need to know whether to splice
/// the event after the reply for the originator client.
pub(crate) fn xi1_client_wants_device_mapping_notify(
    state: &ServerState,
    client_id: ClientId,
    deviceid: u16,
) -> bool {
    let event_type = crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_MAPPING_NOTIFY_OFFSET;
    let class = (u32::from(deviceid) << 8) | u32::from(event_type);
    state
        .clients
        .get(&client_id.0)
        .is_some_and(|c| c.xi1_event_classes.contains(&class))
}

/// True if `client` has selected the XI 1.x `ChangeDeviceNotify` class
/// for `deviceid`.
pub(crate) fn xi1_client_wants_change_device_notify(
    state: &ServerState,
    client_id: ClientId,
    deviceid: u16,
) -> bool {
    let event_type = crate::server::XI_FIRST_EVENT + crate::xinput::XI_CHANGE_DEVICE_NOTIFY_OFFSET;
    let class = (u32::from(deviceid) << 8) | u32::from(event_type);
    state
        .clients
        .get(&client_id.0)
        .is_some_and(|c| c.xi1_event_classes.contains(&class))
}

/// Fanout `ChangeDeviceNotify` to every selector other than `originator`
/// (the caller splices the event into the originator's reply buffer).
/// `request_kind` is 0 (NewPointer) or 1 (NewKeyboard) per XInput.h.
pub(crate) fn emit_change_device_notify(
    state: &mut ServerState,
    originator: ClientId,
    deviceid: u16,
    request_kind: u8,
) {
    let event_type = crate::server::XI_FIRST_EVENT + crate::xinput::XI_CHANGE_DEVICE_NOTIFY_OFFSET;
    let class = (u32::from(deviceid) << 8) | u32::from(event_type);
    let targets: Vec<ClientId> = state
        .clients
        .iter()
        .filter(|(id, _)| **id != originator.0)
        .filter(|(_, c)| c.xi1_event_classes.contains(&class))
        .map(|(id, _)| ClientId(*id))
        .collect();
    if targets.is_empty() {
        return;
    }
    let time = state.timestamp_now();
    #[allow(clippy::cast_possible_truncation)]
    let device_byte = deviceid as u8;
    let _dropped = fanout_event_to_clients(state, &targets, |buf, seq, order| {
        crate::xinput::encode_xi1_change_device_notify(
            buf,
            order,
            event_type,
            device_byte,
            seq,
            time,
            request_kind,
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::ClientState;
    use std::{
        collections::{HashMap, HashSet, VecDeque},
        io::Read,
        os::unix::net::UnixStream,
        sync::{Arc, Mutex, atomic::AtomicU16},
    };
    use yserver_protocol::x11::{ClientByteOrder, ClientId, CreateWindowRequest};

    const DEV: u16 = crate::xinput::DEVICEID_SLAVE_KEYBOARD;

    // Duplicated from pointer_fanout.rs::tests (shared test_fixtures
    // module is a tracked follow-up).
    fn install_client(state: &mut ServerState, id: u32) -> UnixStream {
        let (a, b) = UnixStream::pair().unwrap();
        state.clients.insert(
            id,
            ClientState {
                writer: Arc::new(Mutex::new(crate::transport::Transport::Unix(a))),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0,
                resource_id_mask: u32::MAX,
                event_masks: HashMap::new(),
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
                xi1_event_classes: HashSet::new(),
                xi1_window_event_classes: HashMap::new(),
                outbound: VecDeque::new(),
                watching_writable: false,
                focused_window: ROOT_WINDOW,
                reader_control: None,
                is_local: true,
                fd_passing: true,
            },
        );
        b
    }

    fn read_all_available(peer: &mut UnixStream) -> Vec<u8> {
        peer.set_nonblocking(true).expect("set_nonblocking");
        let mut out = Vec::new();
        let mut buf = [0u8; 512];
        loop {
            match peer.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(err) => panic!("read failed: {err}"),
            }
        }
        peer.set_nonblocking(false).expect("unset_nonblocking");
        out
    }

    fn make_window(state: &mut ServerState, id: u32, parent: ResourceId) -> ResourceId {
        let rid = ResourceId(id);
        state.resources.create_window(
            ClientId(1),
            CreateWindowRequest {
                depth: 24,
                window: rid,
                parent,
                // Keep all test windows away from (0,0) so the cached
                // pointer (0,0) resolves the sprite window to the root.
                x: 50,
                y: 50,
                width: 30,
                height: 30,
                border_width: 0,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
        let _ = state.resources.map_window(rid);
        rid
    }

    /// Subscribe client 1 to DeviceFocusIn + DeviceFocusOut on `window`.
    fn select_focus_classes(state: &mut ServerState, window: ResourceId) {
        let first = crate::server::XI_FIRST_EVENT;
        let classes = state
            .clients
            .get_mut(&1)
            .unwrap()
            .xi1_window_event_classes
            .entry(window)
            .or_default();
        classes.insert(
            (u32::from(DEV) << 8) | u32::from(first + crate::xinput::XI_DEVICE_FOCUS_IN_OFFSET),
        );
        classes.insert(
            (u32::from(DEV) << 8) | u32::from(first + crate::xinput::XI_DEVICE_FOCUS_OUT_OFFSET),
        );
    }

    /// Parsed (is_focus_in, detail, window, mode, deviceid) from the
    /// 32-byte deviceFocus events in `bytes`.
    fn parse_focus_events(bytes: &[u8]) -> Vec<(bool, u8, u32, u8, u8)> {
        let first = crate::server::XI_FIRST_EVENT;
        let fi = first + crate::xinput::XI_DEVICE_FOCUS_IN_OFFSET;
        let fo = first + crate::xinput::XI_DEVICE_FOCUS_OUT_OFFSET;
        bytes
            .chunks_exact(32)
            .filter(|ev| ev[0] == fi || ev[0] == fo)
            .map(|ev| {
                (
                    ev[0] == fi,
                    ev[1],
                    u32::from_le_bytes([ev[8], ev[9], ev[10], ev[11]]),
                    ev[12],
                    ev[13],
                )
            })
            .collect()
    }

    #[test]
    fn default_focus_is_pointer_root() {
        let state = ServerState::new();
        let f = device_focus(&state, DEV);
        assert_eq!(f.focus, FOCUS_POINTER_ROOT);
        assert_eq!(f.revert_to, REVERT_TO_NONE);
    }

    #[test]
    fn sibling_focus_change_emits_nonlinear_pair() {
        // XTS XSetDeviceFocus assertion 12 shape: base with two
        // children; focus ch1 → ch2 must emit exactly FocusOut(ch1,
        // NotifyNonlinear) then FocusIn(ch2, NotifyNonlinear); the
        // common ancestor (base) gets nothing.
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, 1);
        let base = make_window(&mut state, 0x0030_0001, ROOT_WINDOW);
        let ch1 = make_window(&mut state, 0x0030_0002, base);
        let ch2 = make_window(&mut state, 0x0030_0003, base);

        set_device_focus(&mut state, DEV, ch1.0, REVERT_TO_NONE, 100);
        for w in [base, ch1, ch2] {
            select_focus_classes(&mut state, w);
        }
        let _ = read_all_available(&mut peer);

        set_device_focus(&mut state, DEV, ch2.0, REVERT_TO_NONE, 101);
        let events = parse_focus_events(&read_all_available(&mut peer));
        assert_eq!(
            events,
            vec![
                (false, 3, ch1.0, NOTIFY_NORMAL, DEV as u8), // Out Nonlinear
                (true, 3, ch2.0, NOTIFY_NORMAL, DEV as u8),  // In Nonlinear
            ],
        );
        assert_eq!(device_focus(&state, DEV).focus, ch2.0);
    }

    #[test]
    fn unmap_reverts_to_parent_with_ancestor_inferior_pair() {
        // XTS XSetDeviceFocus assertion 6: focus child, RevertToParent;
        // unmap child → FocusOut(child, NotifyAncestor) +
        // FocusIn(base, NotifyInferior); revert_to becomes
        // RevertToNone; focus is the parent.
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, 1);
        let base = make_window(&mut state, 0x0030_0011, ROOT_WINDOW);
        let child = make_window(&mut state, 0x0030_0012, base);

        set_device_focus(&mut state, DEV, child.0, REVERT_TO_PARENT, 100);
        select_focus_classes(&mut state, base);
        select_focus_classes(&mut state, child);
        let _ = read_all_available(&mut peer);

        let _ = state.resources.unmap_window(child);
        revert_unviewable_focus(&mut state);

        let events = parse_focus_events(&read_all_available(&mut peer));
        assert_eq!(
            events,
            vec![
                (false, 0, child.0, NOTIFY_NORMAL, DEV as u8), // Out Ancestor
                (true, 2, base.0, NOTIFY_NORMAL, DEV as u8),   // In Inferior
            ],
        );
        let f = device_focus(&state, DEV);
        assert_eq!(f.focus, base.0);
        assert_eq!(f.revert_to, REVERT_TO_NONE);
    }

    #[test]
    fn unmap_reverts_to_pointer_root_with_full_chain() {
        // XTS XSetDeviceFocus assertion 7: focus = child of base,
        // RevertToPointerRoot, pointer parked on the root. Unmap →
        // Out(child, Nonlinear), Out(base, NonlinearVirtual),
        // Out(root, NonlinearVirtual), In(root, PointerRoot),
        // In(root, Pointer).
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, 1);
        let base = make_window(&mut state, 0x0030_0021, ROOT_WINDOW);
        let child = make_window(&mut state, 0x0030_0022, base);

        set_device_focus(&mut state, DEV, child.0, REVERT_TO_POINTER_ROOT, 100);
        for w in [base, child, ROOT_WINDOW] {
            select_focus_classes(&mut state, w);
        }
        let _ = read_all_available(&mut peer);

        let _ = state.resources.unmap_window(child);
        revert_unviewable_focus(&mut state);

        let events = parse_focus_events(&read_all_available(&mut peer));
        assert_eq!(
            events,
            vec![
                (false, 3, child.0, NOTIFY_NORMAL, DEV as u8), // Out Nonlinear
                (false, 4, base.0, NOTIFY_NORMAL, DEV as u8),  // Out NonlinearVirtual
                (false, 4, ROOT_WINDOW.0, NOTIFY_NORMAL, DEV as u8), // Out NonlinearVirtual
                (true, 6, ROOT_WINDOW.0, NOTIFY_NORMAL, DEV as u8), // In PointerRoot
                (true, 5, ROOT_WINDOW.0, NOTIFY_NORMAL, DEV as u8), // In Pointer
            ],
        );
        let f = device_focus(&state, DEV);
        assert_eq!(f.focus, FOCUS_POINTER_ROOT);
        assert_eq!(f.revert_to, REVERT_TO_POINTER_ROOT);
    }

    #[test]
    fn unmap_reverts_to_none_with_detail_none_focus_in() {
        // XTS XSetDeviceFocus assertion 9: RevertToNone → the chain of
        // FocusOut events then a single FocusIn(root, NotifyDetailNone).
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, 1);
        let base = make_window(&mut state, 0x0030_0031, ROOT_WINDOW);
        let child = make_window(&mut state, 0x0030_0032, base);

        set_device_focus(&mut state, DEV, child.0, REVERT_TO_NONE, 100);
        for w in [base, child, ROOT_WINDOW] {
            select_focus_classes(&mut state, w);
        }
        let _ = read_all_available(&mut peer);

        let _ = state.resources.unmap_window(child);
        revert_unviewable_focus(&mut state);

        let events = parse_focus_events(&read_all_available(&mut peer));
        assert_eq!(
            events,
            vec![
                (false, 3, child.0, NOTIFY_NORMAL, DEV as u8),
                (false, 4, base.0, NOTIFY_NORMAL, DEV as u8),
                (false, 4, ROOT_WINDOW.0, NOTIFY_NORMAL, DEV as u8),
                (true, 7, ROOT_WINDOW.0, NOTIFY_NORMAL, DEV as u8), // In DetailNone
            ],
        );
        assert_eq!(device_focus(&state, DEV).focus, FOCUS_NONE);
    }

    #[test]
    fn key_delivery_route_follows_device_focus() {
        let mut state = ServerState::new();
        let _peer = install_client(&mut state, 1);
        let win = make_window(&mut state, 0x0030_0041, ROOT_WINDOW);

        // Default PointerRoot → plain walk from the sprite window.
        assert_eq!(
            key_delivery_route(&state, DEV),
            (ROOT_WINDOW, Xi1FocusRoute::Walk),
        );

        // Focus None → selection delivery dropped.
        set_device_focus(&mut state, DEV, FOCUS_NONE, REVERT_TO_NONE, 100);
        assert_eq!(
            key_delivery_route(&state, DEV),
            (ROOT_WINDOW, Xi1FocusRoute::Drop),
        );

        // Focus a window the pointer is NOT inside → deliver relative
        // to the focus window only.
        set_device_focus(&mut state, DEV, win.0, REVERT_TO_NONE, 101);
        assert_eq!(
            key_delivery_route(&state, DEV),
            (win, Xi1FocusRoute::WindowOnly(win)),
        );

        // Pointer inside the focus subtree → walk bounded at focus.
        state.pointer_root = (60, 60);
        assert_eq!(
            key_delivery_route(&state, DEV),
            (win, Xi1FocusRoute::WalkUpTo(win)),
        );

        // FollowKeyboard with core focus unset (root) → PointerRoot →
        // plain walk from the sprite window (which is `win` here).
        set_device_focus(&mut state, DEV, FOCUS_FOLLOW_KEYBOARD, REVERT_TO_NONE, 102);
        assert_eq!(key_delivery_route(&state, DEV), (win, Xi1FocusRoute::Walk),);
        // ...and with a core focus set, it follows it.
        state.pointer_root = (0, 0);
        state.core_focus.raw = win.0;
        for c in state.clients.values_mut() {
            c.focused_window = win;
        }
        assert_eq!(
            key_delivery_route(&state, DEV),
            (win, Xi1FocusRoute::WindowOnly(win)),
        );
        // GetDeviceFocus must still report the sentinel, unresolved.
        assert_eq!(device_focus(&state, DEV).focus, FOCUS_FOLLOW_KEYBOARD);
    }

    #[test]
    fn destroy_subtree_reverts_focus_before_teardown() {
        // Disconnect/destroy teardown shape: focus on a child of the
        // dying subtree with RevertToParent must land on the closest
        // surviving ancestor, not a dying window.
        let mut state = ServerState::new();
        let _peer = install_client(&mut state, 1);
        let keep = make_window(&mut state, 0x0030_0051, ROOT_WINDOW);
        let dying = make_window(&mut state, 0x0030_0052, keep);
        let inner = make_window(&mut state, 0x0030_0053, dying);

        set_device_focus(&mut state, DEV, inner.0, REVERT_TO_PARENT, 100);
        revert_focus_for_dying_subtree(&mut state, dying);

        let f = device_focus(&state, DEV);
        assert_eq!(f.focus, keep.0, "focus must skip the dying subtree");
        assert_eq!(f.revert_to, REVERT_TO_NONE);
    }

    #[test]
    fn wrap_aware_time_ordering() {
        assert!(time_after(100, 99));
        assert!(!time_after(99, 100));
        assert!(!time_after(100, 100));
        // Wraparound: 5 is "after" u32::MAX - 5.
        assert!(time_after(5, u32::MAX - 5));
    }
}
