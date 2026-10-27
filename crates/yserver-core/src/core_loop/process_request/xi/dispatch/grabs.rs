use crate::{
    backend::{Backend, OriginContext},
    core_loop::{
        fanout::fanout_event_to_clients,
        key_fanout::replay_frozen_key_to_focus,
        pointer_fanout::replay_frozen_pointer_event_to_state,
        process_request::{
            RequestOutcome, XI2_FIRST_ERROR, XI2_MAJOR_OPCODE, emit_x11_error_with_minor,
            grabs::{
                apply_allow_events, apply_xi2_allow_events_for_slave, deactivate_core_pointer_grab,
                deactivate_passive_pointer_grab_crossings, xi2_allow_mode_to_core,
            },
            xi::{
                XI1_ERROR_BAD_CLASS, XI1_ERROR_BAD_DEVICE, XI1_KEY_MIN, XI1_USE_X_KEYBOARD,
                dispatch::xi_write_reply, xi1_device_has_buttons, xi1_device_has_keys,
                xi1_device_valid, xi1_error, xi1_event_class_device, xi1_modifiers_valid,
                xi1_window_exists,
            },
        },
    },
    resources::ROOT_WINDOW,
    server::ServerState,
};
use log::debug;
use std::io;
use yserver_protocol::{
    x11,
    x11::{ClientByteOrder, ClientId, ResourceId, SequenceNumber},
};

// XI2 grab opcodes (51-55). yserver wires these into the
// existing core X11 grab state (`state.active_pointer_grab`,
// `state.button_grabs`,
// `state.active_keyboard_grab`, `state.key_grabs`) which the
// pointer/key fanout already honours. The XI2 mask + per-
// device routing isn't fully implemented — every grab is
// mapped to the requested master slot or exact slave-device grab
// slot according to the registered device role. That matches what
// GTK relies on in practice (its uses of XIGrabDevice are equivalent
// to XGrabPointer/XGrabKeyboard for the master devices).
//
// Pre-fix all five handlers were no-ops that just sent
// Success replies — GTK then thought it owned the device but
// events still went to the normal pointer-window, so popups
// dismissed on the first stray motion event and `gtk_window_
// present` looped re-mapping the popup at ~50 Hz (visible as
// brisk-menu's MapWindow remap storm pre-`MapWindow`
// no-op fix, and as subtle popup misbehavior elsewhere).
//
// Match Xorg `Xi/exevents.c::DeviceGrabDevice` /
// `ProcXIPassiveGrabDevice` for state-update intent. The
// status byte at reply offset 8 is always Success(0); a
// strict implementation would return AlreadyGrabbed(1) /
// NotViewable(3) / etc., but Xorg's permissive behaviour
// (set the grab and let later events sort it out) keeps GTK
// happy across pause/resume sleeps.
pub(super) fn handle_xi_grab_device(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    byte_order: ClientByteOrder,
) -> io::Result<RequestOutcome> {
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    {
        // XIGrabDevice reply: 32 bytes (header + status at offset
        // 8 + 23 pad). Wire body: window(4) + time(4) + cursor(4)
        // + deviceid(2) + mode(1) + paired_device_mode(1) +
        // owner_events(1) + pad(1) + mask_len(2) + ...
        // XIGrabDevice body: window(4) time(4) cursor(4)
        // deviceid(2) mode(1) paired(1) owner_events(1) pad(1)
        // mask_len(2) mask...
        let (grab_window, time, cursor, deviceid, owner_events) = if body.len() >= 17 {
            (
                u32::from_le_bytes([body[0], body[1], body[2], body[3]]),
                u32::from_le_bytes([body[4], body[5], body[6], body[7]]),
                u32::from_le_bytes([body[8], body[9], body[10], body[11]]),
                u16::from_le_bytes([body[12], body[13]]),
                body[16] != 0,
            )
        } else {
            (0, 0, 0, 0, false)
        };
        // The grab's XI2 event-type mask follows mask_len at body[18..20].
        // Keep the first two words represented by ActivePointerGrab's
        // u64 mask; these cover all pointer event types currently routed.
        let grab_xi2_mask = body
            .get(18..20)
            .map_or(0, |length| {
                usize::from(u16::from_le_bytes([length[0], length[1]]))
            })
            .saturating_mul(4)
            .min(8)
            .checked_add(20)
            .and_then(|end| body.get(20..end))
            .map_or(0, |mask_bytes| {
                let mut mask = [0u8; 8];
                mask[..mask_bytes.len()].copy_from_slice(mask_bytes);
                u64::from_le_bytes(mask)
            });
        let role = match state.xi_devices.role(deviceid) {
            Some(role) => role,
            None => {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    XI2_FIRST_ERROR,
                    u32::from(deviceid),
                    51,
                    XI2_MAJOR_OPCODE,
                );
            }
        };
        let is_keyboard = matches!(
            role,
            crate::xinput::XiDeviceRole::MasterKeyboard
                | crate::xinput::XiDeviceRole::SlaveKeyboard
        );
        let is_master = matches!(
            role,
            crate::xinput::XiDeviceRole::MasterKeyboard
                | crate::xinput::XiDeviceRole::MasterPointer
        );
        // Xorg `dix/events.c:5240` (GrabDevice): a device already
        // grabbed by ANOTHER client → AlreadyGrabbed(1); do NOT
        // overwrite the grab. Same-client re-grab still replaces
        // (Xorg SameClient path, unchanged below). #94 Cinnamon
        // click-swallow: muffin `XIGrabDevice`-stole the master
        // pointer from Steam mid-click (both device=2), so the
        // ButtonRelease was delivered to muffin and Steam's click
        // never completed (cinnamon.xtrace conn 026 vs 105). The
        // pointer slot covers implicit / passive-activated / explicit
        // grabs alike — exactly Xorg's single `deviceGrab.grab`.
        let grab_status: u8 = if is_keyboard && is_master {
            u8::from(
                state
                    .active_keyboard_grab
                    .is_some_and(|g| g.owner != client_id),
            )
        } else if is_keyboard {
            u8::from(
                state
                    .xi2_keyboard_grabs
                    .get(&deviceid)
                    .is_some_and(|g| g.owner != client_id),
            )
        } else if is_master {
            u8::from(
                state
                    .active_pointer_grab
                    .is_some_and(|grab| grab.owner != client_id),
            )
        } else {
            u8::from(
                state
                    .xi2_pointer_grabs
                    .get(&deviceid)
                    .is_some_and(|grab| grab.owner != client_id),
            )
        };
        if grab_status == 0 {
            // Capture the previous grab window (if any, this client's)
            // BEFORE we overwrite. Synthesised crossings need to send
            // Leave on the previous grab window when transitioning to
            // a different one. GTK3 popups grab the input-shadow
            // window first, then re-grab the visible popup — without
            // the Leave on the shadow window GTK3's menu-tracking
            // never engages on the visible popup.
            let prev_pointer_grab_window: Option<ResourceId> = if is_keyboard {
                None
            } else if is_master {
                state
                    .active_pointer_grab
                    .filter(|g| g.owner == client_id)
                    .map(|g| g.grab_window)
            } else {
                state
                    .xi2_pointer_grabs
                    .get(&deviceid)
                    .filter(|g| g.owner == client_id)
                    .map(|g| g.grab_window)
            };
            let prev_keyboard_grab_window: Option<ResourceId> = if is_keyboard && is_master {
                state
                    .active_keyboard_grab
                    .filter(|g| g.owner == client_id)
                    .map(|g| g.grab_window)
            } else {
                state
                    .xi2_keyboard_grabs
                    .get(&deviceid)
                    .filter(|g| g.owner == client_id)
                    .map(|g| g.grab_window)
            };
            if is_keyboard {
                let grab = crate::server::ActiveKeyboardGrab {
                    owner: client_id,
                    grab_window: ResourceId(grab_window),
                    source: crate::server::ActiveKeyboardGrabSource::Explicit,
                    owner_events,
                    via_xi2: true,
                    xi2_mask: u32::try_from(grab_xi2_mask & u64::from(u32::MAX))
                        .expect("first XI2 mask word fits CARD32"),
                };
                if is_master {
                    state.active_keyboard_grab = Some(grab);
                } else {
                    state.xi2_keyboard_grabs.insert(deviceid, grab);
                    let _ = state.detach_xi2_slave(deviceid);
                }
            } else {
                let grab = crate::server::ActivePointerGrab {
                    owner: client_id,
                    grab_window: ResourceId(grab_window),
                    event_mask: 0xFFFF, // core mask is unused for XI2 grabs
                    cursor: ResourceId(cursor),
                    time,
                    owner_events,
                    via_xi2: true,
                    implicit: false,
                    passive: false,
                    xi2_mask: grab_xi2_mask,
                };
                if is_master {
                    state.set_pointer_grab(grab);
                } else {
                    state.xi2_pointer_grabs.insert(deviceid, grab);
                    let _ = state.detach_xi2_slave(deviceid);
                }
            }
            // Core↔XI bridge — Xorg `ActivateKeyboardGrab` /
            // `ActivatePointerGrab` end with `CheckGrabForSyncs`
            // (dix/events.c:1424): a SYNCHRONOUS grab freezes this
            // device, an ASYNCHRONOUS grab THAWS it — including a
            // freeze left behind by a sync passive-grab activation —
            // and `ComputeFreezes` replays the withheld queues. The
            // core GrabKeyboard/GrabPointer handlers already do this;
            // pre-fix the XI2 path skipped it, so muffin's alt-tab
            // (sync passive keybinding grab → ASYNC XIGrabDevice
            // pushModal → XIUngrabDevice) left the keyboard
            // FrozenNoEvent forever: one alt-tab killed all key input
            // (the XIAllowEvents muffin sends after the ungrab is
            // correctly a no-op — its grab is already gone).
            // Wire: body[14]=grab_mode, body[15]=paired_device_mode;
            // XI2 GrabModeSync=0 / GrabModeAsync=1.
            let grab_mode = body.get(14).copied().unwrap_or(1);
            let paired_mode = body.get(15).copied().unwrap_or(1);
            crate::core_loop::pointer_fanout::xi1_check_grab_for_syncs(
                state,
                deviceid,
                client_id,
                grab_mode == 0,
                paired_mode == 0,
            );
            if grab_mode != 0 {
                // The withheld sync-passive replay candidate was already
                // delivered to the grab owner; an async grab forecloses
                // Replay (Xorg drops the stored event on thaw).
                state.xi1_frozen.entry(deviceid).or_default().stored = None;
            }
            debug!(
                "client {} #{} XIGrabDevice window=0x{:x} deviceid={} cursor=0x{:x} -> Success",
                client_id.0, sequence.0, grab_window, deviceid, cursor
            );
            // Synthesised XI2 crossings on grab activation. Matches
            // Xorg `Xi/exevents.c::ActivateKeyboardGrab` /
            // `ActivatePointerGrab` (which call `DoEnterLeaveEvents`
            // with `NotifyGrab`). Without these GTK3 popup state
            // machines never engage their hover/click tracking — the
            // menu is visible but items don't highlight or activate
            // on click. Captured in `mate-xorg.xtrace` as the pair
            // `XI_Leave(mode=Grab, detail=Nonlinear)` on the previous
            // grab window followed by `XI_Enter(mode=Grab,
            // detail=Nonlinear)` on the new grab window; keyboard
            // grabs additionally get `XI_FocusIn(mode=Grab,
            // detail=Nonlinear)`. NotifyGrab = 1, NotifyNonlinear = 3.
            let pointer_xy = backend
                .query_pointer(origin)
                .ok()
                .map(|p| (p.win_x, p.win_y))
                .unwrap_or((0, 0));
            let (root_x, root_y) = pointer_xy;
            let server_time = state.timestamp_now();
            // Emit one XI2 crossing (Enter/Leave/FocusIn/FocusOut) on a
            // single window with an explicit detail code, mode
            // NotifyGrab. The keyboard-focus path drives this directly;
            // the pointer path drives it from a crossing chain so the
            // detail codes — and the `from == to` no-op — match Xorg's
            // `DoEnterLeaveEvents`.
            let emit_xi_crossing = |state: &mut ServerState,
                                    evtype: u16,
                                    target_window: ResourceId,
                                    detail: u8| {
                let (origin_x, origin_y) = state.resources.window_absolute_position(target_window);
                let event_x =
                    i16::try_from(i32::from(root_x).saturating_sub(origin_x)).unwrap_or(i16::MAX);
                let event_y =
                    i16::try_from(i32::from(root_y).saturating_sub(origin_y)).unwrap_or(i16::MAX);
                let focus = !matches!(evtype, 9 | 10) && state.crossing_has_focus(target_window);
                let _dropped = fanout_event_to_clients(state, &[client_id], |out, seq, order| {
                    x11::encode_xi2_crossing_event(
                        out,
                        order,
                        seq,
                        XI2_MAJOR_OPCODE,
                        evtype,
                        deviceid,
                        server_time,
                        ROOT_WINDOW,
                        target_window,
                        root_x,
                        root_y,
                        event_x,
                        event_y,
                        0,
                        1, // mode = NotifyGrab
                        detail,
                        deviceid,
                        focus,
                    );
                });
            };
            if is_keyboard {
                // Keyboard grab → focus transitions (Xorg
                // `ActivateKeyboardGrab` → `DoFocusEvents`). Unchanged.
                if let Some(prev) = prev_keyboard_grab_window
                    && prev != ResourceId(grab_window)
                {
                    emit_xi_crossing(state, 10, prev, 3); // XI_FocusOut
                }
                emit_xi_crossing(state, 9, ResourceId(grab_window), 3); // XI_FocusIn
            } else {
                // Pointer grab → Xorg `ActivatePointerGrab` →
                // `DoEnterLeaveEvents(from, grab_window, NotifyGrab)`
                // (dix/events.c). `from` is the previous grab window
                // when replacing an active grab, else the window the
                // pointer currently sits in (the sprite, resolved from
                // the live position). `compute_crossing_chain`
                // early-returns when `from == grab_window`, so grabbing
                // the window already under the pointer emits nothing —
                // matching Xorg. The pre-fix code emitted an
                // unconditional `XI_Enter(NotifyGrab)` on the grab
                // window, which desynced cinnamon's XEmbed-systray
                // click-forward whenever muffin grabbed its own
                // full-screen stage (pamac tray unclickable, 2026-06-22).
                let from = prev_pointer_grab_window.unwrap_or_else(|| {
                    state
                        .root_pointer_target_at(root_x, root_y)
                        .map_or(ResourceId(grab_window), |h| h.0)
                });
                let chain =
                    crate::crossings::implicit_grab_crossings(state, from, ResourceId(grab_window));
                for e in chain {
                    let evtype: u16 = match e.kind {
                        crate::crossings::CrossingKind::Enter => 7,
                        crate::crossings::CrossingKind::Leave => 8,
                    };
                    emit_xi_crossing(state, evtype, e.window, e.detail);
                }
            }
        } // end: mutate grab state + emit crossings only when granted
        // XIGrabDevice reply carries the grab status at offset 8
        // (0 = Success, 1 = AlreadyGrabbed — Xorg dix/events.c:5240).
        let mut reply = x11::fixed_reply(byte_order, sequence, 0, 0);
        let mut grab_reply_body = [0u8; 24];
        grab_reply_body[0] = grab_status;
        reply.extend_from_slice(&grab_reply_body);
        buf.extend_from_slice(&reply);
    }
    xi_write_reply(state, client_id, buf)
}

#[allow(clippy::needless_return)]
pub(super) fn handle_xi_ungrab_device(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    // XIUngrabDevice body: time(4) + deviceid(2) + pad(2).
    let deviceid = if body.len() >= 6 {
        u16::from_le_bytes([body[4], body[5]])
    } else {
        0
    };
    let role = match state.xi_devices.role(deviceid) {
        Some(role) => role,
        None => {
            return emit_x11_error_with_minor(
                state,
                client_id,
                sequence,
                XI2_FIRST_ERROR,
                u32::from(deviceid),
                52,
                XI2_MAJOR_OPCODE,
            );
        }
    };
    let is_keyboard = matches!(
        role,
        crate::xinput::XiDeviceRole::MasterKeyboard | crate::xinput::XiDeviceRole::SlaveKeyboard
    );
    let is_master = matches!(
        role,
        crate::xinput::XiDeviceRole::MasterKeyboard | crate::xinput::XiDeviceRole::MasterPointer
    );
    deactivate_xi2_device_grab(
        state,
        backend,
        origin,
        client_id,
        deviceid,
        is_keyboard,
        is_master,
    );
    debug!(
        "client {} #{} XIUngrabDevice deviceid={}",
        client_id.0, sequence.0, deviceid
    );
    return Ok(RequestOutcome::Handled);
}

/// Deactivate `owner`'s XI2 grab on `deviceid`, if it holds one, and send the
/// owner the `NotifyUngrab` events (Xorg `DeactivatePointerGrab` /
/// `DeactivateKeyboardGrab`). XIUngrabDevice and window destroy.
pub(in crate::core_loop::process_request) fn deactivate_xi2_device_grab(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    owner: ClientId,
    deviceid: u16,
    is_keyboard: bool,
    is_master: bool,
) {
    // Capture grab_window BEFORE clearing — we synthesize the
    // matching XI2 crossing with `NotifyUngrab` on the way out
    // so the client's grab state machine pairs cleanly with
    // its earlier NotifyGrab. Matches Xorg
    // `Xi/exevents.c::DeactivateKeyboardGrab` /
    // `DeactivatePointerGrab`.
    let grab_window_for_event: Option<ResourceId> = if is_keyboard && is_master {
        state
            .active_keyboard_grab
            .filter(|g| g.owner == owner)
            .map(|g| g.grab_window)
    } else if is_keyboard {
        state
            .xi2_keyboard_grabs
            .get(&deviceid)
            .filter(|g| g.owner == owner)
            .map(|g| g.grab_window)
    } else if is_master {
        state
            .active_pointer_grab
            .filter(|grab| grab.owner == owner)
            .map(|grab| grab.grab_window)
    } else {
        state
            .xi2_pointer_grabs
            .get(&deviceid)
            .filter(|grab| grab.owner == owner)
            .map(|grab| grab.grab_window)
    };
    if grab_window_for_event.is_some() {
        if is_keyboard && is_master {
            state.active_keyboard_grab = None;
        } else if is_keyboard {
            state.xi2_keyboard_grabs.remove(&deviceid);
        } else if is_master {
            state.clear_pointer_grab();
        } else {
            state.xi2_pointer_grabs.remove(&deviceid);
        }
        state.xi1_frozen.entry(deviceid).or_default().stored = None;
        crate::core_loop::pointer_fanout::xi1_core_grab_bridge_release(state, deviceid, owner);
        if !is_master {
            state.reattach_xi2_slave(deviceid);
        }
        if !is_keyboard {
            // Clear any grab-cursor sprite override, as
            // `deactivate_core_pointer_grab` does — a core
            // XGrabPointer(cursor) torn down via XIUngrabDevice
            // must not strand the grab cursor on the sprite.
            let _ = backend.set_grab_cursor(None, None);
        }
    }
    if let Some(grab_window) = grab_window_for_event {
        let pointer_xy = backend
            .query_pointer(origin)
            .ok()
            .map(|p| (p.win_x, p.win_y))
            .unwrap_or((0, 0));
        let (root_x, root_y) = pointer_xy;
        let server_time = state.timestamp_now();
        let emit_xi_crossing =
            |state: &mut ServerState, evtype: u16, target_window: ResourceId, detail: u8| {
                let (origin_x, origin_y) = state.resources.window_absolute_position(target_window);
                let event_x =
                    i16::try_from(i32::from(root_x).saturating_sub(origin_x)).unwrap_or(i16::MAX);
                let event_y =
                    i16::try_from(i32::from(root_y).saturating_sub(origin_y)).unwrap_or(i16::MAX);
                let focus = !matches!(evtype, 9 | 10) && state.crossing_has_focus(target_window);
                let _dropped = fanout_event_to_clients(state, &[owner], |out, seq, order| {
                    x11::encode_xi2_crossing_event(
                        out,
                        order,
                        seq,
                        XI2_MAJOR_OPCODE,
                        evtype,
                        deviceid,
                        server_time,
                        ROOT_WINDOW,
                        target_window,
                        root_x,
                        root_y,
                        event_x,
                        event_y,
                        0,
                        2, // mode = NotifyUngrab
                        detail,
                        deviceid,
                        focus,
                    );
                });
            };
        if is_keyboard {
            // Keyboard ungrab → focus restore. Unchanged.
            emit_xi_crossing(state, 10, grab_window, 3); // XI_FocusOut
        } else {
            // Pointer ungrab → Xorg `DeactivatePointerGrab` →
            // `DoEnterLeaveEvents(grab_window, sprite, NotifyUngrab)`
            // (dix/events.c). Symmetric with the activation
            // chain: `from == to` (sprite still inside the grab
            // window) emits nothing.
            let to = state
                .root_pointer_target_at(root_x, root_y)
                .map_or(grab_window, |h| h.0);
            let chain = crate::crossings::implicit_grab_crossings(state, grab_window, to);
            for e in chain {
                let evtype: u16 = match e.kind {
                    crate::crossings::CrossingKind::Enter => 7,
                    crate::crossings::CrossingKind::Leave => 8,
                };
                emit_xi_crossing(state, evtype, e.window, e.detail);
            }
        }
    }
}

#[allow(clippy::needless_return)]
pub(super) fn handle_xi_allow_events(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    // XIAllowEvents body (per X11/extensions/XI2proto.h
    // `xXIAllowEventsReq`): time(4) + deviceid(2) + mode(1) +
    // pad(1). XI 2.2 extends with touchid(4) + grab_window(4)
    // for touch grabs; we don't implement touch, so the
    // extension fields are ignored.
    //
    // mutter/muffin/cinnamon install click-to-focus passive
    // button grabs (sync) and keybinding key grabs (sync); on each
    // press they examine it then call XIAllowEvents to thaw/replay.
    // The mode (XIAsyncDevice/XISyncDevice/XIReplayDevice/…) +
    // deviceid map onto a core AllowSome mode — see
    // `xi2_allow_mode_to_core` — and run through the shared
    // `apply_allow_events` below.
    let (deviceid, mode) = if body.len() >= 8 {
        (u16::from_le_bytes([body[4], body[5]]), body[6])
    } else {
        (0, 0)
    };
    let role = match state.xi_devices.role(deviceid) {
        Some(role) => role,
        None => {
            return emit_x11_error_with_minor(
                state,
                client_id,
                sequence,
                XI2_FIRST_ERROR,
                u32::from(deviceid),
                53,
                XI2_MAJOR_OPCODE,
            );
        }
    };
    let is_keyboard = matches!(
        role,
        crate::xinput::XiDeviceRole::MasterKeyboard | crate::xinput::XiDeviceRole::SlaveKeyboard
    );
    let is_master = matches!(
        role,
        crate::xinput::XiDeviceRole::MasterKeyboard | crate::xinput::XiDeviceRole::MasterPointer
    );
    debug!(
        "client {} #{} XIAllowEvents deviceid={} mode={}",
        client_id.0, sequence.0, deviceid, mode
    );
    let time = if body.len() >= 4 {
        u32::from_le_bytes([body[0], body[1], body[2], body[3]])
    } else {
        0
    };
    // Map the XI2 mode + target deviceid onto the equivalent core
    // AllowSome mode and run the SHARED `apply_allow_events`. This
    // used to be a partial reimplementation that diverged from the
    // core path and caused recurring freezes (desktop rubber-band =
    // Async/Replay thaw gap; Cinnamon input freeze = XISyncDevice
    // treated as a no-op). Touch modes (XIAcceptTouch/XIRejectTouch)
    // are unsupported → no-op.
    if is_master {
        if let Some(core_mode) = xi2_allow_mode_to_core(mode, is_keyboard) {
            return apply_allow_events(state, backend, client_id, sequence, core_mode, time);
        }
    } else {
        return apply_xi2_allow_events_for_slave(
            state,
            backend,
            client_id,
            sequence,
            deviceid,
            is_keyboard,
            mode,
            time,
        );
    }
    return Ok(RequestOutcome::Handled);
}

/// `XIAllDevices` (0) or `XIAllMasterDevices` (1): Xorg
/// `ProcXIPassiveGrabDevice` / `ProcXIPassiveUngrabDevice`
/// (Xi/xipassivegrab.c) take these without a device lookup.
fn is_xi2_device_wildcard(deviceid: u16) -> bool {
    deviceid <= 1
}

/// The device a passive grab is stored under. `XIAllMasterDevices` becomes
/// the master of the grab's class, which the grab matching already treats
/// as "any device attached to it"; `XIAllDevices` stays 0, matched the same
/// way. Grab and ungrab map alike, so an ungrab finds its grab.
fn passive_grab_device(deviceid: u16, grab_type: u8) -> u16 {
    match (deviceid, grab_type) {
        (1, 0) => crate::xinput::DEVICEID_MASTER_POINTER,
        (1, 1) => crate::xinput::DEVICEID_MASTER_KEYBOARD,
        _ => deviceid,
    }
}

pub(super) fn handle_xi_passive_grab_device(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    byte_order: ClientByteOrder,
) -> io::Result<RequestOutcome> {
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    {
        // XIPassiveGrabDevice body (per X11/extensions/XI2proto.h
        // `xXIPassiveGrabDeviceReq`): time(4) + grab_window(4) +
        // cursor(4) + detail(4) + deviceid(2) + num_modifiers(2)
        // + mask_len(2) + grab_type(1) + grab_mode(1) +
        // paired_device_mode(1) + owner_events(1) + pad1(2) +
        // mask(mask_len*4) + modifiers(num_modifiers*4).
        //
        // GrabType: Button=0, Keycode=1, Enter=2, FocusIn=3,
        // TouchBegin=4. Button and Keycode map onto core X11
        // PassiveButtonGrab / KeyGrab; the other three are no-ops
        // for now (yserver doesn't synthesise core X11 Enter/
        // FocusIn passive grabs).
        let (
            grab_window,
            detail,
            deviceid,
            num_modifiers,
            mask_len,
            grab_type,
            grab_mode,
            paired_device_mode,
            owner_events,
        ) = if body.len() >= 25 {
            (
                u32::from_le_bytes([body[4], body[5], body[6], body[7]]),
                u32::from_le_bytes([body[12], body[13], body[14], body[15]]),
                u16::from_le_bytes([body[16], body[17]]),
                u16::from_le_bytes([body[18], body[19]]),
                u16::from_le_bytes([body[20], body[21]]) as usize,
                body[22],
                body[23],
                body[24],
                body.get(25).copied().unwrap_or(0) != 0,
            )
        } else {
            (0, 0, 0, 0, 0, 0xff, 1, 1, false)
        };
        if matches!(grab_type, 0 | 1) && !is_xi2_device_wildcard(deviceid) {
            let role = state.xi_devices.role(deviceid);
            let compatible = match grab_type {
                0 => matches!(
                    role,
                    Some(
                        crate::xinput::XiDeviceRole::MasterPointer
                            | crate::xinput::XiDeviceRole::SlavePointer
                    )
                ),
                1 => matches!(
                    role,
                    Some(
                        crate::xinput::XiDeviceRole::MasterKeyboard
                            | crate::xinput::XiDeviceRole::SlaveKeyboard
                    )
                ),
                _ => true,
            };
            if !compatible {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    XI2_FIRST_ERROR,
                    u32::from(deviceid),
                    54,
                    XI2_MAJOR_OPCODE,
                );
            }
        }
        let deviceid = passive_grab_device(deviceid, grab_type);
        // Modifiers tail starts after the header (28) and the mask
        // (mask_len * 4 bytes).
        let mods_start = 28 + mask_len * 4;
        // First mask word: event types 0..=31.
        let grab_xi2_mask = if mask_len > 0 {
            body.get(28..32)
                .map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        } else {
            0
        };
        let mut modifier_masks: Vec<u16> = Vec::with_capacity(num_modifiers.max(1).into());
        if num_modifiers == 0 {
            // "no modifiers" — single grab with modifier-mask 0.
            modifier_masks.push(0);
        } else {
            for i in 0..usize::from(num_modifiers) {
                let off = mods_start + i * 4;
                if let Some(slice) = body.get(off..off + 4) {
                    let raw = u32::from_le_bytes(slice.try_into().unwrap());
                    // XI2 `Any` = bit 31; map to core X11 AnyModifier (0x8000).
                    #[allow(clippy::cast_possible_truncation)]
                    let m = if raw & 0x8000_0000 != 0 {
                        0x8000u16
                    } else {
                        (raw & 0xFFFF) as u16
                    };
                    modifier_masks.push(m);
                }
            }
        }
        match grab_type {
            0 => {
                // Button. detail = button number (0 == AnyButton).
                let button = u8::try_from(detail).unwrap_or(0);
                for modifiers in &modifier_masks {
                    state.button_grabs.retain(|g| {
                        !(g.owner == client_id
                            && g.via_xi2
                            && g.device_id == deviceid
                            && g.grab_window.0 == grab_window
                            && g.button == button
                            && g.modifiers == *modifiers)
                    });
                    state.button_grabs.push(crate::server::PassiveButtonGrab {
                        device_id: deviceid,
                        owner: client_id,
                        grab_window: ResourceId(grab_window),
                        button,
                        modifiers: *modifiers,
                        owner_events,
                        event_mask: 0xFFFF_FFFF,
                        pointer_mode: grab_mode,
                        keyboard_mode: 1,
                        confine_to: ResourceId(0),
                        via_xi2: true,
                    });
                }
            }
            1 => {
                // Keycode. detail = keycode (0 == AnyKey).
                let keycode = u8::try_from(detail).unwrap_or(0);
                for modifiers in &modifier_masks {
                    state.key_grabs.retain(|g| {
                        !(g.owner == client_id
                            && g.via_xi2
                            && g.device_id == deviceid
                            && g.grab_window.0 == grab_window
                            && g.keycode == keycode
                            && g.modifiers == *modifiers)
                    });
                    state.key_grabs.push(crate::server::KeyGrab {
                        device_id: deviceid,
                        owner: client_id,
                        grab_window: ResourceId(grab_window),
                        keycode,
                        modifiers: *modifiers,
                        owner_events,
                        pointer_mode: paired_device_mode,
                        keyboard_mode: grab_mode,
                        via_xi2: true,
                        xi2_mask: grab_xi2_mask,
                    });
                }
            }
            _ => {
                // Enter/FocusIn/TouchBegin — yserver has no
                // matching machinery yet; record the request in
                // the debug log so a real repro shows up.
                debug!(
                    "client {} #{} XIPassiveGrabDevice unsupported grab_type={} \
                         (Enter/FocusIn/TouchBegin)",
                    client_id.0, sequence.0, grab_type
                );
            }
        }
        debug!(
            "client {} #{} XIPassiveGrabDevice window=0x{:x} detail=0x{:x} deviceid={} \
                 grab_type={} num_modifiers={} -> Success",
            client_id.0, sequence.0, grab_window, detail, deviceid, grab_type, num_modifiers,
        );
        // Reply: num_modifiers(2) + pad(22). With 0 failed
        // modifiers the client treats every requested combination
        // as grabbed.
        let mut reply = x11::fixed_reply(byte_order, sequence, 0, 0);
        reply.extend_from_slice(&[0u8; 24]);
        buf.extend_from_slice(&reply);
    }
    xi_write_reply(state, client_id, buf)
}

#[allow(clippy::needless_return)]
pub(super) fn handle_xi_passive_ungrab_device(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    // XIPassiveUngrabDevice body: grab_window(4) + detail(4)
    // + deviceid(2) + num_modifiers(2) + grab_type(1) +
    // pad(3) + modifiers.
    let (grab_window, detail, device_id, num_modifiers, grab_type) = if body.len() >= 13 {
        (
            u32::from_le_bytes([body[0], body[1], body[2], body[3]]),
            u32::from_le_bytes([body[4], body[5], body[6], body[7]]),
            u16::from_le_bytes([body[8], body[9]]),
            u16::from_le_bytes([body[10], body[11]]),
            body[12],
        )
    } else {
        (0, 0, 0, 0, 0xff)
    };
    if matches!(grab_type, 0 | 1) && !is_xi2_device_wildcard(device_id) {
        let role = state.xi_devices.role(device_id);
        let compatible = match grab_type {
            0 => matches!(
                role,
                Some(
                    crate::xinput::XiDeviceRole::MasterPointer
                        | crate::xinput::XiDeviceRole::SlavePointer
                )
            ),
            1 => matches!(
                role,
                Some(
                    crate::xinput::XiDeviceRole::MasterKeyboard
                        | crate::xinput::XiDeviceRole::SlaveKeyboard
                )
            ),
            _ => true,
        };
        if !compatible {
            return emit_x11_error_with_minor(
                state,
                client_id,
                sequence,
                XI2_FIRST_ERROR,
                u32::from(device_id),
                55,
                XI2_MAJOR_OPCODE,
            );
        }
    }
    let device_id = passive_grab_device(device_id, grab_type);
    let mods_start = 16;
    let mut modifier_masks: Vec<u16> = Vec::with_capacity(num_modifiers.max(1).into());
    if num_modifiers == 0 {
        modifier_masks.push(0);
    } else {
        for i in 0..usize::from(num_modifiers) {
            let off = mods_start + i * 4;
            if let Some(slice) = body.get(off..off + 4) {
                let raw = u32::from_le_bytes(slice.try_into().unwrap());
                #[allow(clippy::cast_possible_truncation)]
                let m = if raw & 0x8000_0000 != 0 {
                    0x8000u16
                } else {
                    (raw & 0xFFFF) as u16
                };
                modifier_masks.push(m);
            }
        }
    }
    match grab_type {
        0 => {
            let button = u8::try_from(detail).unwrap_or(0);
            state.button_grabs.retain(|g| {
                !(g.owner == client_id
                    && g.via_xi2
                    && g.device_id == device_id
                    && g.grab_window.0 == grab_window
                    && (g.button == button || button == 0)
                    && modifier_masks
                        .iter()
                        .any(|m| g.modifiers == *m || *m == 0x8000))
            });
        }
        1 => {
            let keycode = u8::try_from(detail).unwrap_or(0);
            state.key_grabs.retain(|g| {
                !(g.owner == client_id
                    && g.via_xi2
                    && g.device_id == device_id
                    && g.grab_window.0 == grab_window
                    && (g.keycode == keycode || keycode == 0)
                    && modifier_masks
                        .iter()
                        .any(|m| g.modifiers == *m || *m == 0x8000))
            });
        }
        _ => {}
    }
    debug!(
        "client {} #{} XIPassiveUngrabDevice window=0x{:x} detail=0x{:x} grab_type={}",
        client_id.0, sequence.0, grab_window, detail, grab_type
    );
    return Ok(RequestOutcome::Handled);
}

// GrabDevice: { grabWindow, time, event_count, this_device_mode,
// other_devices_mode, ownerEvents, deviceid } + classes. Classes
// must name the grabbed device (XTS XGrabDevice-4/-13).
pub(super) fn handle_xi1_grab_device(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    byte_order: ClientByteOrder,
    minor: u8,
) -> io::Result<RequestOutcome> {
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    {
        if body.len() < 16 {
            return Ok(RequestOutcome::Handled);
        }
        let dev = u16::from(body[13]);
        if !xi1_device_valid(&state.xi_devices, dev) {
            return xi1_error(
                state,
                client_id,
                sequence,
                XI1_ERROR_BAD_DEVICE,
                u32::from(dev),
                minor,
            );
        }
        let win = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
        if !xi1_window_exists(state, win) {
            return xi1_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_WINDOW,
                win,
                minor,
            );
        }
        let this_mode = body[10];
        let other_mode = body[11];
        let owner_events = body[12];
        if this_mode > 1 {
            return xi1_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                u32::from(this_mode),
                minor,
            );
        }
        if other_mode > 1 {
            return xi1_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                u32::from(other_mode),
                minor,
            );
        }
        if owner_events > 1 {
            return xi1_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                u32::from(owner_events),
                minor,
            );
        }
        let count = usize::from(u16::from_le_bytes([body[8], body[9]]));
        for i in 0..count {
            let off = 16 + i * 4;
            if off + 4 > body.len() {
                break;
            }
            let class =
                u32::from_le_bytes([body[off], body[off + 1], body[off + 2], body[off + 3]]);
            let class_dev = xi1_event_class_device(class);
            if !xi1_device_valid(&state.xi_devices, class_dev) || class_dev != dev {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    XI1_ERROR_BAD_CLASS,
                    class,
                    minor,
                );
            }
        }
        // Establish the active device grab. Status values per X.h:
        // GrabSuccess 0, AlreadyGrabbed 1, GrabInvalidTime 2,
        // GrabNotViewable 3.
        let time = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
        let now = state
            .timestamp_now()
            .max(state.xi1_last_input_time)
            .max(state.xi1_last_grab_time);
        let viewable = state
            .resources
            .window(ResourceId(win))
            .is_some_and(|w| w.map_state == crate::resources::MapState::Viewable);
        let status: u8 = if state
            .xi1_active_grabs
            .get(&dev)
            .is_some_and(|g| g.owner != client_id)
        {
            1 // AlreadyGrabbed
        } else if !viewable {
            3 // GrabNotViewable
        } else if time != 0 && (time < state.xi1_last_grab_time || time > now) {
            2 // GrabInvalidTime
        } else {
            state.xi1_active_grabs.insert(
                dev,
                crate::server::Xi1ActiveGrab {
                    owner: client_id,
                    deviceid: dev,
                    grab_window: ResourceId(win),
                    owner_events: owner_events != 0,
                    this_mode,
                    other_mode,
                    passive_detail: None,
                },
            );
            state.xi1_last_grab_time = if time == 0 { now } else { time };
            // CheckGrabForSyncs: a sync this_mode freezes the
            // device ONCE (FrozenNoEvent — no stored event, so
            // Replay is a no-op on it); a sync other_mode holds
            // the paired device on this grab's behalf.
            crate::core_loop::pointer_fanout::xi1_check_grab_for_syncs(
                state,
                dev,
                client_id,
                this_mode == 0,
                other_mode == 0,
            );
            0 // GrabSuccess
        };
        debug!(
            "client {} #{} XI1 GrabDevice device={dev} window=0x{win:x} status={status}",
            client_id.0, sequence.0
        );
        let mut reply = x11::fixed_reply(byte_order, sequence, 0, 0);
        reply.push(status);
        reply.extend_from_slice(&[0u8; 23]);
        buf.extend_from_slice(&reply);
    }
    xi_write_reply(state, client_id, buf)
}

// UngrabDevice (void): { time, deviceid }. Body: time at
// bytes 0..4, deviceid at byte 4 — `xUngrabDeviceReq`.
pub(super) fn handle_xi1_ungrab_device(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    minor: u8,
) -> io::Result<RequestOutcome> {
    {
        let time = u32::from_le_bytes([
            *body.first().unwrap_or(&0),
            *body.get(1).unwrap_or(&0),
            *body.get(2).unwrap_or(&0),
            *body.get(3).unwrap_or(&0),
        ]);
        let dev = u16::from(*body.get(4).unwrap_or(&0));
        if !xi1_device_valid(&state.xi_devices, dev) {
            return xi1_error(
                state,
                client_id,
                sequence,
                XI1_ERROR_BAD_DEVICE,
                u32::from(dev),
                minor,
            );
        }
        // Xorg `Xi/ungrdev.c::ProcXUngrabDevice` requires the
        // request's time to be in `[grabTime, currentTime]`
        // (CurrentTime = 0 passes both checks). Out-of-range
        // timestamps silently skip the release — xts5
        // XUngrabDevice-2 asserts this.
        let now = state.timestamp_now();
        let time_ok = time == 0 || (time >= state.xi1_last_grab_time && time <= now);
        if time_ok
            && state
                .xi1_active_grabs
                .get(&dev)
                .is_some_and(|g| g.owner == client_id)
        {
            crate::core_loop::pointer_fanout::xi1_deactivate_device_grab(state, dev);
        }
        debug!(
            "client {} #{} XI1 UngrabDevice device={dev}",
            client_id.0, sequence.0
        );
    }
    xi_write_reply(state, client_id, Vec::new())
}

// GrabDeviceKey (void): { grabWindow, event_count, modifiers,
// modifier_device, grabbed_device, key, this_device_mode,
// other_devices_mode, ownerEvents } + classes. (The XTS prose
// for XGrabDeviceKey-19/-20/-21 says "BadValue", but the test
// error traps accept only the XI extension BadDevice/BadClass
// codes — verified empirically; Xorg's dixLookupDevice agrees.)
pub(super) fn handle_xi1_grab_device_key(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    minor: u8,
) -> io::Result<RequestOutcome> {
    {
        if body.len() < 16 {
            return Ok(RequestOutcome::Handled);
        }
        let mods = u16::from_le_bytes([body[6], body[7]]);
        let mod_dev = u16::from(body[8]);
        let dev = u16::from(body[9]);
        let key = body[10];
        let this_mode = body[11];
        let other_mode = body[12];
        let owner_events = body[13];
        if !xi1_device_valid(&state.xi_devices, dev) {
            return xi1_error(
                state,
                client_id,
                sequence,
                XI1_ERROR_BAD_DEVICE,
                u32::from(dev),
                minor,
            );
        }
        if mod_dev != XI1_USE_X_KEYBOARD && !xi1_device_valid(&state.xi_devices, mod_dev) {
            return xi1_error(
                state,
                client_id,
                sequence,
                XI1_ERROR_BAD_DEVICE,
                u32::from(mod_dev),
                minor,
            );
        }
        if mod_dev != XI1_USE_X_KEYBOARD && !xi1_device_has_keys(&state.xi_devices, mod_dev) {
            return xi1_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, minor);
        }
        // key: AnyKey (0) or within the advertised keycode range.
        if key != 0 && key < XI1_KEY_MIN {
            return xi1_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                u32::from(key),
                minor,
            );
        }
        if !xi1_modifiers_valid(mods) {
            return xi1_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                u32::from(mods),
                minor,
            );
        }
        if this_mode > 1 || other_mode > 1 || owner_events > 1 {
            return xi1_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                u32::from(this_mode.max(other_mode).max(owner_events)),
                minor,
            );
        }
        let win = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
        if !xi1_window_exists(state, win) {
            return xi1_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_WINDOW,
                win,
                minor,
            );
        }
        let count = usize::from(u16::from_le_bytes([body[4], body[5]]));
        for i in 0..count {
            let off = 16 + i * 4;
            if off + 4 > body.len() {
                break;
            }
            let class =
                u32::from_le_bytes([body[off], body[off + 1], body[off + 2], body[off + 3]]);
            if !xi1_device_valid(&state.xi_devices, xi1_event_class_device(class)) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    XI1_ERROR_BAD_CLASS,
                    class,
                    minor,
                );
            }
        }
        // Conflicting grab by another client → BadAccess (XTS
        // XGrabDeviceKey-16).
        if state.xi1_passive_grabs.iter().any(|g| {
            g.is_key
                && g.owner != client_id
                && g.deviceid == dev
                && g.grab_window == ResourceId(win)
                && (g.detail == key || g.detail == 0 || key == 0)
                && (g.modifiers == mods || g.modifiers == 0x8000 || mods == 0x8000)
        }) {
            return xi1_error(state, client_id, sequence, x11::error::BAD_ACCESS, 0, minor);
        }
        state.xi1_passive_grabs.retain(|g| {
            !(g.is_key
                && g.owner == client_id
                && g.deviceid == dev
                && g.grab_window == ResourceId(win)
                && g.detail == key
                && g.modifiers == mods)
        });
        state.xi1_passive_grabs.push(crate::server::Xi1PassiveGrab {
            owner: client_id,
            deviceid: dev,
            grab_window: ResourceId(win),
            detail: key,
            modifiers: mods,
            owner_events: owner_events != 0,
            this_mode,
            other_mode,
            is_key: true,
        });
        debug!(
            "client {} #{} XI1 GrabDeviceKey device={dev}",
            client_id.0, sequence.0
        );
    }
    xi_write_reply(state, client_id, Vec::new())
}

// UngrabDeviceKey (void): { grabWindow, modifiers,
// modifier_device, key, grabbed_device }.
pub(super) fn handle_xi1_ungrab_device_key(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    minor: u8,
) -> io::Result<RequestOutcome> {
    {
        if body.len() < 9 {
            return Ok(RequestOutcome::Handled);
        }
        let mods = u16::from_le_bytes([body[4], body[5]]);
        let mod_dev = u16::from(body[6]);
        let key = body[7];
        let dev = u16::from(body[8]);
        if !xi1_device_valid(&state.xi_devices, dev) {
            return xi1_error(
                state,
                client_id,
                sequence,
                XI1_ERROR_BAD_DEVICE,
                u32::from(dev),
                minor,
            );
        }
        if mod_dev != XI1_USE_X_KEYBOARD && !xi1_device_valid(&state.xi_devices, mod_dev) {
            return xi1_error(
                state,
                client_id,
                sequence,
                XI1_ERROR_BAD_DEVICE,
                u32::from(mod_dev),
                minor,
            );
        }
        if !xi1_device_has_keys(&state.xi_devices, dev)
            || (mod_dev != XI1_USE_X_KEYBOARD && !xi1_device_has_keys(&state.xi_devices, mod_dev))
        {
            return xi1_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, minor);
        }
        if key != 0 && key < XI1_KEY_MIN {
            return xi1_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                u32::from(key),
                minor,
            );
        }
        if !xi1_modifiers_valid(mods) {
            return xi1_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                u32::from(mods),
                minor,
            );
        }
        let win = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
        if !xi1_window_exists(state, win) {
            return xi1_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_WINDOW,
                win,
                minor,
            );
        }
        state.xi1_passive_grabs.retain(|g| {
            !(g.is_key
                && g.owner == client_id
                && g.deviceid == dev
                && g.grab_window == ResourceId(win)
                && (key == 0 || g.detail == key)
                && (mods == 0x8000 || g.modifiers == mods))
        });
        debug!(
            "client {} #{} XI1 UngrabDeviceKey device={dev}",
            client_id.0, sequence.0
        );
    }
    xi_write_reply(state, client_id, Vec::new())
}

// GrabDeviceButton (void): { grabWindow, grabbed_device,
// modifier_device, event_count, modifiers, this_device_mode,
// other_devices_mode, button, ownerEvents } + classes.
pub(super) fn handle_xi1_grab_device_button(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    minor: u8,
) -> io::Result<RequestOutcome> {
    {
        if body.len() < 16 {
            return Ok(RequestOutcome::Handled);
        }
        let dev = u16::from(body[4]);
        let mod_dev = u16::from(body[5]);
        let mods = u16::from_le_bytes([body[8], body[9]]);
        let this_mode = body[10];
        let other_mode = body[11];
        let owner_events = body[13];
        if !xi1_device_valid(&state.xi_devices, dev) {
            return xi1_error(
                state,
                client_id,
                sequence,
                XI1_ERROR_BAD_DEVICE,
                u32::from(dev),
                minor,
            );
        }
        if mod_dev != XI1_USE_X_KEYBOARD && !xi1_device_valid(&state.xi_devices, mod_dev) {
            return xi1_error(
                state,
                client_id,
                sequence,
                XI1_ERROR_BAD_DEVICE,
                u32::from(mod_dev),
                minor,
            );
        }
        if mod_dev != XI1_USE_X_KEYBOARD && !xi1_device_has_keys(&state.xi_devices, mod_dev) {
            return xi1_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, minor);
        }
        if !xi1_modifiers_valid(mods) {
            return xi1_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                u32::from(mods),
                minor,
            );
        }
        if this_mode > 1 || other_mode > 1 || owner_events > 1 {
            return xi1_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                u32::from(this_mode.max(other_mode).max(owner_events)),
                minor,
            );
        }
        let win = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
        if !xi1_window_exists(state, win) {
            return xi1_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_WINDOW,
                win,
                minor,
            );
        }
        let button = body[12];
        // Validate the trailing XEventClass array (Xorg
        // `Xi/grabdevb.c::ProcXGrabDeviceButton` →
        // `CreateMaskFromList` → `BadClass`). Each class's high
        // byte names a device id; any unknown deviceid is
        // BadClass, including the `0xFFFFFFFF` sentinel xts5
        // GrabDeviceButton-21 builds. event_count lives at
        // body[6..8] per `xGrabDeviceButtonReq`.
        let event_count = usize::from(u16::from_le_bytes([body[6], body[7]]));
        for i in 0..event_count {
            let off = 16 + i * 4;
            let Some(slice) = body.get(off..off + 4) else {
                break;
            };
            let class = u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]);
            if !xi1_device_valid(&state.xi_devices, xi1_event_class_device(class)) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    XI1_ERROR_BAD_CLASS,
                    class,
                    minor,
                );
            }
        }
        if state.xi1_passive_grabs.iter().any(|g| {
            !g.is_key
                && g.owner != client_id
                && g.deviceid == dev
                && g.grab_window == ResourceId(win)
                && (g.detail == button || g.detail == 0 || button == 0)
                && (g.modifiers == mods || g.modifiers == 0x8000 || mods == 0x8000)
        }) {
            return xi1_error(state, client_id, sequence, x11::error::BAD_ACCESS, 0, minor);
        }
        state.xi1_passive_grabs.retain(|g| {
            !(!g.is_key
                && g.owner == client_id
                && g.deviceid == dev
                && g.grab_window == ResourceId(win)
                && g.detail == button
                && g.modifiers == mods)
        });
        state.xi1_passive_grabs.push(crate::server::Xi1PassiveGrab {
            owner: client_id,
            deviceid: dev,
            grab_window: ResourceId(win),
            detail: button,
            modifiers: mods,
            owner_events: owner_events != 0,
            this_mode,
            other_mode,
            is_key: false,
        });
        debug!(
            "client {} #{} XI1 GrabDeviceButton device={dev}",
            client_id.0, sequence.0
        );
    }
    xi_write_reply(state, client_id, Vec::new())
}

// UngrabDeviceButton (void): { grabWindow, modifiers,
// modifier_device, button, grabbed_device }.
pub(super) fn handle_xi1_ungrab_device_button(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    minor: u8,
) -> io::Result<RequestOutcome> {
    {
        if body.len() < 9 {
            return Ok(RequestOutcome::Handled);
        }
        let mods = u16::from_le_bytes([body[4], body[5]]);
        let mod_dev = u16::from(body[6]);
        let dev = u16::from(body[8]);
        if !xi1_device_valid(&state.xi_devices, dev) {
            return xi1_error(
                state,
                client_id,
                sequence,
                XI1_ERROR_BAD_DEVICE,
                u32::from(dev),
                minor,
            );
        }
        if mod_dev != XI1_USE_X_KEYBOARD && !xi1_device_valid(&state.xi_devices, mod_dev) {
            return xi1_error(
                state,
                client_id,
                sequence,
                XI1_ERROR_BAD_DEVICE,
                u32::from(mod_dev),
                minor,
            );
        }
        if !xi1_device_has_buttons(&state.xi_devices, dev)
            || (mod_dev != XI1_USE_X_KEYBOARD && !xi1_device_has_keys(&state.xi_devices, mod_dev))
        {
            return xi1_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, minor);
        }
        if !xi1_modifiers_valid(mods) {
            return xi1_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                u32::from(mods),
                minor,
            );
        }
        let win = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
        if !xi1_window_exists(state, win) {
            return xi1_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_WINDOW,
                win,
                minor,
            );
        }
        let button = body[7];
        state.xi1_passive_grabs.retain(|g| {
            !(!g.is_key
                && g.owner == client_id
                && g.deviceid == dev
                && g.grab_window == ResourceId(win)
                && (button == 0 || g.detail == button)
                && (mods == 0x8000 || g.modifiers == mods))
        });
        debug!(
            "client {} #{} XI1 UngrabDeviceButton device={dev}",
            client_id.0, sequence.0
        );
    }
    xi_write_reply(state, client_id, Vec::new())
}

// AllowDeviceEvents (void): { time, mode, deviceid }. Modes:
// AsyncThisDevice(0)..SyncAll(5).
pub(super) fn handle_xi1_allow_device_events(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    minor: u8,
) -> io::Result<RequestOutcome> {
    {
        let mode = *body.get(4).unwrap_or(&0);
        let dev = u16::from(*body.get(5).unwrap_or(&0));
        if !xi1_device_valid(&state.xi_devices, dev) {
            return xi1_error(
                state,
                client_id,
                sequence,
                XI1_ERROR_BAD_DEVICE,
                u32::from(dev),
                minor,
            );
        }
        if mode > 5 {
            return xi1_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                u32::from(mode),
                minor,
            );
        }
        // Port of Xorg `AllowSome` (dix/events.c:1823-1936) over
        // the two-device model. Mode → newState mapping per
        // Xi/allowev.c: AsyncThisDevice(0)→THAWED,
        // SyncThisDevice(1)→FREEZE_NEXT_EVENT,
        // ReplayThisDevice(2)→NOT_GRABBED,
        // AsyncOtherDevices(3)→THAW_OTHERS,
        // AsyncAll(4)→THAWED_BOTH, SyncAll(5)→FREEZE_BOTH_NEXT_EVENT.
        use crate::{
            core_loop::pointer_fanout::{
                xi1_compute_freezes, xi1_deactivate_device_grab, xi1_device_grab_owner,
                xi1_other_input_device, xi1_route_device_event,
            },
            server::Xi1SyncState,
        };
        let time = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
        let other_dev = xi1_other_input_device(state, dev);
        let sync_state = |state: &ServerState, d: u16| {
            state
                .xi1_frozen
                .get(&d)
                .map_or(Xi1SyncState::Thawed, |f| f.state)
        };
        let sync_other =
            |state: &ServerState, d: u16| state.xi1_frozen.get(&d).and_then(|f| f.other);
        // thisGrabbed / otherGrabbed / thisSynced / othersFrozen
        // (dix/events.c:1830-1851). `sync.other` stores the owning
        // client, and deactivation clears it, so `== client` means
        // "held on behalf of a live grab of this client".
        let this_grabbed = xi1_device_grab_owner(state, dev) == Some(client_id);
        let other_grabbed =
            other_dev.is_some_and(|other| xi1_device_grab_owner(state, other) == Some(client_id));
        let this_synced = sync_other(state, dev) == Some(client_id) && other_grabbed;
        let others_frozen = other_dev.is_some_and(|other| {
            other_grabbed && sync_state(state, other) >= Xi1SyncState::FrozenNoEvent
        });
        let this_frozen_by_grab =
            this_grabbed && sync_state(state, dev) >= Xi1SyncState::FrozenNoEvent;
        debug!(
            "client {} #{} XI1 AllowDeviceEvents device={dev} mode={mode} \
                 this_grabbed={this_grabbed} this_synced={this_synced} \
                 others_frozen={others_frozen}",
            client_id.0, sequence.0,
        );
        // Gate 1: only act when this device is frozen by the
        // client (directly or on another grab's behalf).
        if !(this_frozen_by_grab || this_synced) {
            return Ok(RequestOutcome::Handled);
        }
        // Gate 2: time validation — later than now or earlier than
        // the client's last grab time → no effect.
        let now = state
            .timestamp_now()
            .max(state.xi1_last_input_time)
            .max(state.xi1_last_grab_time);
        if time != 0
            && (crate::core_loop::xi1_focus::time_after(time, now)
                || crate::core_loop::xi1_focus::time_after(state.xi1_last_grab_time, time))
        {
            return Ok(RequestOutcome::Handled);
        }
        match mode {
            // AsyncThisDevice → THAWED.
            0 => {
                if this_grabbed && let Some(f) = state.xi1_frozen.get_mut(&dev) {
                    f.state = Xi1SyncState::Thawed;
                }
                if this_synced && let Some(f) = state.xi1_frozen.get_mut(&dev) {
                    f.other = None;
                }
                let xid_map = backend.xid_map().clone();
                xi1_compute_freezes(state, backend, &xid_map);
            }
            // SyncThisDevice → FREEZE_NEXT_EVENT.
            1 => {
                if this_grabbed {
                    state.xi1_frozen.entry(dev).or_default().state = Xi1SyncState::FreezeNextEvent;
                    if this_synced && let Some(f) = state.xi1_frozen.get_mut(&dev) {
                        f.other = None;
                    }
                    let xid_map = backend.xid_map().clone();
                    xi1_compute_freezes(state, backend, &xid_map);
                }
            }
            // ReplayThisDevice → NOT_GRABBED: only for a grab
            // frozen WITH a stored event; release the grab and
            // reprocess that event as if the grab never took it
            // (passive grabs get a fresh look at it).
            2 => {
                if this_grabbed && sync_state(state, dev) == Xi1SyncState::FrozenWithEvent {
                    if this_synced && let Some(f) = state.xi1_frozen.get_mut(&dev) {
                        f.other = None;
                    }
                    let stored = state.xi1_frozen.get_mut(&dev).and_then(|f| f.stored.take());
                    // Reprocessing masks out passive grabs at or
                    // above the released grab's window (Xorg
                    // syncEvents.replayWin → CheckDeviceGrabs).
                    let replay_floor = state.xi1_active_grabs.get(&dev).map(|g| g.grab_window);
                    if state.xi1_active_grabs.contains_key(&dev) {
                        xi1_deactivate_device_grab(state, dev);
                    } else if state.active_pointer_grab.is_some_and(|grab| grab.passive) {
                        // Xorg NOT_GRABBED → DeactivateGrab before the
                        // replay, passive flavor (mirrors
                        // apply_allow_events' passive NOT_GRABBED
                        // deactivation).
                        deactivate_passive_pointer_grab_crossings(state);
                    } else if state
                        .active_pointer_grab
                        .is_some_and(|g| g.owner == client_id)
                    {
                        // Explicit (or implicit) core grab: full
                        // deactivation (mirrors apply_allow_events'
                        // deactivate_core_pointer_grab call).
                        deactivate_core_pointer_grab(state, backend, client_id);
                    }
                    // Thaw dev after any deactivation. Arm A already
                    // thaws internally; re-thawing an already-thawed
                    // device is a no-op, and the no-core-grab case
                    // (previous behavior) simply thaws here.
                    if let Some(f) = state.xi1_frozen.get_mut(&dev) {
                        f.state = Xi1SyncState::Thawed;
                    }
                    if let Some(stored) = stored {
                        match stored {
                            crate::server::QueuedInputEvent::Xi1Routed(mut q) => {
                                q.replay_floor = replay_floor;
                                let _ = xi1_route_device_event(state, q, true);
                            }
                            crate::server::QueuedInputEvent::HostPointer(event) => {
                                let xid_map = backend.xid_map().clone();
                                let _ = replay_frozen_pointer_event_to_state(
                                    state, backend, &xid_map, event,
                                );
                            }
                            crate::server::QueuedInputEvent::HostKey(event) => {
                                let _ = replay_frozen_key_to_focus(state, event);
                            }
                            crate::server::QueuedInputEvent::HostKeyTransition(
                                event,
                                master_transition_accepted,
                            ) => {
                                let _ = crate::core_loop::key_fanout::replay_frozen_key_to_focus_after_transition(
                                    state,
                                    event,
                                    master_transition_accepted,
                                );
                            }
                            // Only a device event activates a grab, so
                            // the stored slot never holds a raw event;
                            // processing one is plain master delivery.
                            crate::server::QueuedInputEvent::RawKey(event) => {
                                let _ = crate::core_loop::key_fanout::deliver_raw_key_master(
                                    state, event,
                                );
                            }
                        }
                    }
                    let xid_map = backend.xid_map().clone();
                    xi1_compute_freezes(state, backend, &xid_map);
                }
            }
            // AsyncOtherDevices → THAW_OTHERS: thaw every OTHER
            // device held by this client (this device untouched).
            3 => {
                if others_frozen && let Some(other) = other_dev {
                    if xi1_device_grab_owner(state, other) == Some(client_id)
                        && let Some(f) = state.xi1_frozen.get_mut(&other)
                    {
                        f.state = Xi1SyncState::Thawed;
                    }
                    if sync_other(state, other) == Some(client_id)
                        && let Some(f) = state.xi1_frozen.get_mut(&other)
                    {
                        f.other = None;
                    }
                    let xid_map = backend.xid_map().clone();
                    xi1_compute_freezes(state, backend, &xid_map);
                }
            }
            // AsyncAll → THAWED_BOTH / SyncAll → FREEZE_BOTH_NEXT_EVENT:
            // both REQUIRE the other devices to be frozen by the
            // client (XTS XAllowDeviceEvents-16/-18); they act on
            // every device the client grabbed or held.
            4 | 5 => {
                if others_frozen {
                    let new_state = if mode == 4 {
                        Xi1SyncState::Thawed
                    } else {
                        Xi1SyncState::FreezeBothNextEvent
                    };
                    for d in std::iter::once(dev).chain(other_dev) {
                        if xi1_device_grab_owner(state, d) == Some(client_id) {
                            state.xi1_frozen.entry(d).or_default().state = new_state;
                        }
                        if sync_other(state, d) == Some(client_id)
                            && let Some(f) = state.xi1_frozen.get_mut(&d)
                        {
                            f.other = None;
                        }
                    }
                    let xid_map = backend.xid_map().clone();
                    xi1_compute_freezes(state, backend, &xid_map);
                }
            }
            _ => unreachable!("mode validated above"),
        }
    }
    xi_write_reply(state, client_id, Vec::new())
}
