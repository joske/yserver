use super::*;

pub(super) fn handle_allow_events(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    // Core AllowEvents modes (X11 spec):
    //   0 AsyncPointer  1 SyncPointer   2 ReplayPointer
    //   3 AsyncKeyboard 4 SyncKeyboard  5 ReplayKeyboard
    //   6 AsyncBoth     7 SyncBoth
    let mode = header.data;
    if mode > 7 {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_VALUE,
            u32::from(mode),
            35,
        );
    }
    let time = body
        .get(0..4)
        .map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    apply_allow_events(state, backend, client_id, sequence, mode, time)
}

/// Map an XI2 `XIAllowEvents` mode + registry-classified target onto the
/// equivalent core `AllowSome` mode consumed by [`apply_allow_events`]. The
/// paired modes act on the OTHER device. Touch modes (`XIAcceptTouch`=6 /
/// `XIRejectTouch`=7) and anything unknown are unsupported → `None` (no-op).
/// XI2 mode constants per `X11/extensions/XI2.h`: AsyncDevice=0,
/// SyncDevice=1, ReplayDevice=2, AsyncPairedDevice=3, AsyncPair=4,
/// SyncPair=5.
pub(super) fn xi2_allow_mode_to_core(xi2_mode: u8, kbd: bool) -> Option<u8> {
    // Core modes: 0 AsyncPointer 1 SyncPointer 2 ReplayPointer
    //             3 AsyncKeyboard 4 SyncKeyboard 5 ReplayKeyboard
    //             6 AsyncBoth 7 SyncBoth
    Some(match xi2_mode {
        0 => u8::from(kbd) * 3,     // XIAsyncDevice  → Async{Pointer,Keyboard}
        1 => 1 + u8::from(kbd) * 3, // XISyncDevice   → Sync{Pointer,Keyboard}
        2 => 2 + u8::from(kbd) * 3, // XIReplayDevice → Replay{Pointer,Keyboard}
        3 => u8::from(!kbd) * 3,    // XIAsyncPairedDevice → async the PAIRED device
        4 => 6,                     // XIAsyncPair → AsyncBoth
        5 => 7,                     // XISyncPair  → SyncBoth
        _ => return None,           // XIAcceptTouch / XIRejectTouch / unknown
    })
}

/// XI2 AllowEvents for a slave device uses the exact device's freeze state;
/// paired modes use the opposite master returned by the registry attachment.
pub(super) fn apply_xi2_allow_events_for_slave(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
    sequence: SequenceNumber,
    device_id: u16,
    is_keyboard: bool,
    xi_mode: u8,
    time: u32,
) -> io::Result<RequestOutcome> {
    use crate::server::Xi1SyncState;

    if xi_mode > 5 {
        return Ok(RequestOutcome::Handled);
    }
    let paired_id = crate::core_loop::pointer_fanout::xi1_other_input_device(state, device_id);
    let target_id = if xi_mode == 3 {
        let Some(paired_id) = paired_id else {
            return Ok(RequestOutcome::Handled);
        };
        paired_id
    } else {
        device_id
    };
    let target_owner = crate::core_loop::pointer_fanout::xi1_device_grab_owner(state, target_id);
    let target_state = state
        .xi1_frozen
        .get(&target_id)
        .map_or(Xi1SyncState::Thawed, |freeze| freeze.state);
    let target_other = state
        .xi1_frozen
        .get(&target_id)
        .and_then(|freeze| freeze.other);
    let is_frozen_by_client = target_owner == Some(client_id)
        && target_state >= Xi1SyncState::FrozenNoEvent
        || target_other == Some(client_id);
    if !is_frozen_by_client {
        return Ok(RequestOutcome::Handled);
    }
    let now = state
        .timestamp_now()
        .max(state.xi1_last_input_time)
        .max(if is_keyboard {
            state.last_keyboard_grab_time
        } else {
            state.last_pointer_grab_time
        });
    if time != 0 && crate::core_loop::xi1_focus::time_after(time, now) {
        return Ok(RequestOutcome::Handled);
    }

    let mut devices = Vec::with_capacity(2);
    match xi_mode {
        0..=2 => devices.push(device_id),
        3 => devices.push(target_id),
        4 | 5 => {
            devices.push(device_id);
            if let Some(paired_id) = paired_id {
                devices.push(paired_id);
            }
        }
        _ => return Ok(RequestOutcome::Handled),
    }
    let new_state = match xi_mode {
        0 | 3 | 4 => Some(Xi1SyncState::Thawed),
        1 | 5 => Some(Xi1SyncState::FreezeNextEvent),
        2 => Some(Xi1SyncState::Thawed),
        _ => None,
    };
    for id in &devices {
        if let Some(new_state) = new_state {
            let freeze = state.xi1_frozen.entry(*id).or_default();
            freeze.state = new_state;
            if freeze.other == Some(client_id) {
                freeze.other = None;
            }
            if matches!(xi_mode, 0 | 3 | 4) {
                freeze.stored = None;
            }
        }
    }

    if xi_mode == 2 {
        let stored = state
            .xi1_frozen
            .get_mut(&device_id)
            .and_then(|freeze| freeze.stored.take());
        if let Some(crate::server::QueuedInputEvent::HostPointer(_)) = stored {
            if let Some(grab) = state.xi2_pointer_grabs.get(&device_id)
                && grab.owner == client_id
            {
                state.xi2_pointer_grabs.remove(&device_id);
                state.reattach_xi2_slave(device_id);
            }
            if let Some(crate::server::QueuedInputEvent::HostPointer(event)) = stored {
                let xid_map = backend.xid_map().clone();
                let _dropped =
                    crate::core_loop::pointer_fanout::replay_frozen_pointer_event_to_state(
                        state, backend, &xid_map, event,
                    );
            }
        } else if let Some(crate::server::QueuedInputEvent::HostKeyTransition(
            event,
            master_transition_accepted,
        )) = stored
        {
            if state
                .xi2_keyboard_grabs
                .get(&device_id)
                .is_some_and(|grab| grab.owner == client_id)
            {
                state.xi2_keyboard_grabs.remove(&device_id);
                state.reattach_xi2_slave(device_id);
            }
            let _dropped =
                crate::core_loop::key_fanout::replay_frozen_key_to_focus_after_transition(
                    state,
                    event,
                    master_transition_accepted,
                );
        } else if let Some(crate::server::QueuedInputEvent::HostKey(event)) = stored {
            if state
                .xi2_keyboard_grabs
                .get(&device_id)
                .is_some_and(|grab| grab.owner == client_id)
            {
                state.xi2_keyboard_grabs.remove(&device_id);
                state.reattach_xi2_slave(device_id);
            }
            let _dropped = crate::core_loop::key_fanout::replay_frozen_key_to_focus(state, event);
        }
    }
    let xid_map = backend.xid_map().clone();
    crate::core_loop::pointer_fanout::xi1_compute_freezes(state, backend, &xid_map);
    debug!(
        "client {} #{} XIAllowEvents slave device={} mode={}",
        client_id.0, sequence.0, device_id, xi_mode
    );
    Ok(RequestOutcome::Handled)
}

/// Shared `AllowSome` implementation (Xorg `dix/events.c:1823`), driven by
/// BOTH core `AllowEvents` (mode straight from the request) AND XI2
/// `XIAllowEvents` (mode mapped from the XI2 mode + deviceid; see
/// [`xi2_allow_mode_to_core`]). Routing the XI2 path through here — rather
/// than the old partial reimplementation in the XI2 handler — kills a
/// recurring class of divergence bugs where the XI2 copy mishandled modes
/// the core path got right (the desktop rubber-band = Async/Replay thaw
/// gap; the Cinnamon input freeze = `XISyncDevice`/mode-1 treated as a
/// no-op). `mode` is a CORE AllowEvents mode (0..=7); `time` is the
/// request timestamp.
pub(super) fn apply_allow_events(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
    sequence: SequenceNumber,
    mode: u8,
    time: u32,
) -> io::Result<RequestOutcome> {
    // Xorg AllowSome: the request is a no-op when its time is LATER
    // than the current time or EARLIER than the client's grab time
    // (XAllowEvents-12/13). Pointer modes gate on the pointer's grab
    // time, keyboard and *Both modes on the keyboard's.
    let grab_time = if matches!(mode, 0..=2) {
        state.last_pointer_grab_time
    } else {
        state.last_keyboard_grab_time
    };
    let now = state
        .timestamp_now()
        .max(state.xi1_last_input_time)
        .max(grab_time);
    if time != 0
        && (crate::core_loop::xi1_focus::time_after(time, now)
            || crate::core_loop::xi1_focus::time_after(grab_time, time))
    {
        debug!(
            "client {} #{} AllowEvents ignored (time {time} outside [{grab_time}, {now}])",
            client_id.0, sequence.0
        );
        return Ok(RequestOutcome::Handled);
    }
    debug!(
        "client {} #{} AllowEvents mode={} frozen_pointer={} frozen_keyboard={}",
        client_id.0,
        sequence.0,
        mode,
        state
            .xi1_frozen
            .get(&crate::xinput::DEVICEID_MASTER_POINTER)
            .is_some_and(|f| f.stored.is_some()),
        state
            .xi1_frozen
            .get(&crate::xinput::DEVICEID_MASTER_KEYBOARD)
            .is_some_and(|f| f.stored.is_some()),
    );

    // Port of Xorg AllowSome (dix/events.c:1823). `thisDev` is the
    // pointer for the pointer modes and the keyboard for the keyboard
    // and *Both modes; the request is a no-op unless this client's
    // grab holds the device frozen, or the device is synced on this
    // client's behalf.
    use crate::server::Xi1SyncState;
    let dev_this = if matches!(mode, 0..=2) {
        crate::xinput::DEVICEID_MASTER_POINTER
    } else {
        crate::xinput::DEVICEID_MASTER_KEYBOARD
    };
    let grab_owner_of = |state: &ServerState, dev: u16| -> Option<ClientId> {
        crate::core_loop::pointer_fanout::xi1_device_grab_owner(state, dev)
    };
    let this_grabbed = grab_owner_of(state, dev_this) == Some(client_id);
    let this_state = state
        .xi1_frozen
        .get(&dev_this)
        .map_or(Xi1SyncState::Thawed, |f| f.state);
    let this_synced = state.xi1_frozen.get(&dev_this).and_then(|f| f.other) == Some(client_id);
    if !((this_grabbed && this_state >= Xi1SyncState::FrozenNoEvent) || this_synced) {
        debug!(
            "client {} #{} AllowEvents no-op (not frozen by this client: grabbed={this_grabbed} state={this_state:?} synced={this_synced})",
            client_id.0, sequence.0
        );
        return Ok(RequestOutcome::Handled);
    }

    let pointer_replay = mode == 2;
    let keyboard_replay = mode == 5;

    // State transitions per mode (Xorg AllowSome switch). The queued
    // events play AFTER the transition so the freeze gate at fanout
    // entry sees the new state: Async → events flow; Sync →
    // FreezeNextEvent lets them flow until the next key/button trips
    // the re-freeze (FreezeThisEventIfNeededForSyncGrab).
    let set_state = |state: &mut ServerState, dev: u16, st: Xi1SyncState| {
        let f = state.xi1_frozen.entry(dev).or_default();
        f.state = st;
        if f.other == Some(client_id) {
            f.other = None;
        }
    };
    let dev_ptr = crate::xinput::DEVICEID_MASTER_POINTER;
    let dev_kbd = crate::xinput::DEVICEID_MASTER_KEYBOARD;
    // Xorg AllowSome `othersFrozen`: the *Both modes (AsyncBoth /
    // SyncBoth) only act when ANOTHER device this client grabbed is
    // itself frozen (dix/events.c:1872, 1886 — `if (othersFrozen)`).
    // Without this gate a client could thaw/re-freeze its own device
    // via *Both even when no paired device is frozen, leaving later
    // grabs in the wrong sync state. In the two-device model "others"
    // relative to dev_this (keyboard, for *Both) is the pointer.
    let others_frozen = {
        let other = if dev_this == dev_kbd {
            dev_ptr
        } else {
            dev_kbd
        };
        grab_owner_of(state, other) == Some(client_id)
            && state
                .xi1_frozen
                .get(&other)
                .is_some_and(|f| f.state >= Xi1SyncState::FrozenNoEvent)
    };
    match mode {
        0 | 2 => set_state(state, dev_ptr, Xi1SyncState::Thawed),
        1 => set_state(state, dev_ptr, Xi1SyncState::FreezeNextEvent),
        3 | 5 => set_state(state, dev_kbd, Xi1SyncState::Thawed),
        4 => set_state(state, dev_kbd, Xi1SyncState::FreezeNextEvent),
        6 if others_frozen => {
            // AsyncBoth thaws every device this client's grabs froze.
            for dev in [dev_ptr, dev_kbd] {
                if grab_owner_of(state, dev) == Some(client_id)
                    || state.xi1_frozen.get(&dev).and_then(|f| f.other) == Some(client_id)
                {
                    set_state(state, dev, Xi1SyncState::Thawed);
                }
            }
        }
        7 if others_frozen => {
            // SyncBoth arms FreezeBothNextEvent on the client's
            // grabbed devices.
            for dev in [dev_ptr, dev_kbd] {
                if grab_owner_of(state, dev) == Some(client_id)
                    || state.xi1_frozen.get(&dev).and_then(|f| f.other) == Some(client_id)
                {
                    set_state(state, dev, Xi1SyncState::FreezeBothNextEvent);
                }
            }
        }
        // *Both with no paired device frozen → no-op (Xorg).
        6 | 7 => {}
        _ => {}
    }

    // Withheld pointer events. The activating press of a sync passive
    // grab was already delivered to the grab owner — only Replay
    // re-delivers it (to the natural target, grab bypassed); the
    // async/sync releases just drop it. The QUEUE (events that arrived
    // during the freeze, delivered to nobody) plays through the normal
    // pipeline on every pointer-side release — Xorg
    // PlayReleasedEvents; with the grab still active they route to
    // the grab owner.
    let pointer_side = matches!(mode, 0 | 1 | 2 | 6 | 7);
    let frozen_pointer = if pointer_side {
        state
            .xi1_frozen
            .get_mut(&dev_ptr)
            .and_then(|f| f.stored.take())
    } else {
        None
    };
    // NOT_GRABBED (Replay) deactivates a frozen-with-event grab on
    // this device — passive AND explicit (Xorg dix/events.c:1898 calls
    // DeactivateGrab regardless of grab kind). Async/Sync keep the
    // grab until the natural button release. Without the explicit
    // branch a GrabPointer(GrabModeSync) replays the frozen event but
    // stays active → user-visible stuck grab.
    let pointer_has_event = frozen_pointer.is_some();
    if pointer_replay && state.active_pointer_grab.is_some_and(|grab| grab.passive) {
        // Xorg `DeactivatePointerGrab` → `DoEnterLeaveEvents(grab
        // window → sprite, NotifyUngrab)` (dix/events.c:1711): mirror
        // the activation chain emitted in pointer_fanout so the client's
        // crossing state rebalances. The subsequent replay (below)
        // re-delivers the press to the natural target, matching Xorg's
        // ungrab-crossings-then-replayed-press order. Shared with the XI1
        // ReplayThisDevice path.
        deactivate_passive_pointer_grab_crossings(state);
    } else if pointer_replay
        && pointer_has_event
        && state
            .active_pointer_grab
            .is_some_and(|g| g.owner == client_id)
    {
        deactivate_core_pointer_grab(state, backend, client_id);
    }

    let keyboard_side = matches!(mode, 3..=7);
    let frozen_keyboard = if keyboard_side {
        state
            .xi1_frozen
            .get_mut(&dev_kbd)
            .and_then(|f| f.stored.take())
    } else {
        None
    };
    // The activating core key was already delivered to the grab owner. Its
    // parallel XI1 form can be pending from the device enqueue path; Sync
    // must not replay it and consume the allowance intended for the next
    // physical key event.
    if mode == 4
        && let Some(
            crate::server::QueuedInputEvent::HostKey(event)
            | crate::server::QueuedInputEvent::HostKeyTransition(event, _),
        ) = frozen_keyboard.as_ref()
    {
        let press_evcode =
            crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_KEY_PRESS_OFFSET;
        if let Some(index) = state.sync_pending.iter().position(|pending| {
            pending.device == dev_kbd
                && matches!(&pending.event, crate::server::QueuedInputEvent::Xi1Routed(q)
                    if q.evcode == press_evcode && q.detail == event.keycode && q.time == event.time)
        }) {
            state.sync_pending.remove(index);
        }
    }
    // GH #59: a synchronous passive key grab's activating press is
    // recorded in BOTH the unified activating slot (`Xi1Freeze::stored`,
    // taken above) AND the XI1 pending queue. Xorg's ComputeFreezes replays the stored
    // activating event only for Replay*, never for Sync*
    // (dix/events.c:1320-1370). On SyncKeyboard, drop that duplicate
    // queued XI1 DeviceKeyPress so the closing `xi1_compute_freezes`
    // doesn't replay the grab's own already-delivered press — that
    // replay would trip FreezeNextEvent → FrozenWithEvent, consuming the
    // SyncKeyboard allowance meant for the next *physical* event and
    // withholding the terminating release (sxhkd's dead keyboard).
    if keyboard_replay
        && state
            .active_keyboard_grab
            .is_some_and(|g| g.owner == client_id)
    {
        let is_passive = matches!(
            state.active_keyboard_grab.map(|g| g.source),
            Some(crate::server::ActiveKeyboardGrabSource::PassiveKey { .. })
        );
        let kbd_has_event = frozen_keyboard.is_some()
            || state
                .sync_pending
                .iter()
                .any(|pending| pending.device == dev_kbd);
        if is_passive {
            state.active_keyboard_grab = None;
        } else if kbd_has_event {
            // Explicit GrabKeyboard(GrabModeSync): deactivate on replay
            // too (Xorg NOT_GRABBED → DeactivateGrab), emitting the
            // FocusOut(NotifyUngrab) chain. Else the grab stays stuck.
            deactivate_core_keyboard_grab(state, client_id);
        }
    }

    if pointer_replay
        && let Some(crate::server::QueuedInputEvent::HostPointer(event)) = frozen_pointer
    {
        // Snapshot the xid_map so we can release the immutable
        // borrow on `backend` before pointer_event_fanout_to_state
        // mutates `state`. The map is small (a few hundred entries)
        // and AllowEvents-replay is rare, so the clone is cheap
        // compared to the wire writes the fanout produces.
        let xid_map = backend.xid_map().clone();
        // Deliver core and XI2 device events to the natural target on
        // replay, but do not regenerate XI_RawButtonPress: raw events
        // describe physical input and were delivered before the grab froze.
        // The XI2 freeze-filter at pointer_fanout.rs:309-316 (cinnamon
        // aea9b0f) restricts XI2 press to the grab owner ONLY
        // during sync-passive-grab freeze; the natural target
        // never received XI2 during the freeze. Trace evidence
        // from MATE on silence (2026-05-28): marco activates
        // passive grab on the panel, AllowEvents fires, mate-panel
        // (only XI2 mask, no core mask) saw the queued release
        // but no press → menu clicks dead.
        let _dropped = replay_frozen_pointer_event_to_state(state, backend, &xid_map, event);
    }
    if keyboard_replay
        && let Some(crate::server::QueuedInputEvent::HostKeyTransition(
            event,
            master_transition_accepted,
        )) = frozen_keyboard
    {
        let _dropped = crate::core_loop::key_fanout::replay_frozen_key_to_focus_after_transition(
            state,
            event,
            master_transition_accepted,
        );
    } else if keyboard_replay
        && let Some(crate::server::QueuedInputEvent::HostKey(event)) = frozen_keyboard
    {
        let _dropped = replay_frozen_key_to_focus(state, event);
    }
    // ComputeFreezes: replay the per-device XI1 queues + withheld
    // core keys now that the sync states changed (Xorg has ONE
    // AllowSome for both protocols).
    let xid_map = backend.xid_map().clone();
    crate::core_loop::pointer_fanout::xi1_compute_freezes(state, backend, &xid_map);
    Ok(RequestOutcome::Handled)
}

/// Core pointer-grab event mask: ButtonPress..KeymapState — X.h
/// `PointerGrabMask`. Requests selecting bits outside this set get
/// `BadValue` (Xorg `ProcGrabPointer` / `ProcGrabButton` /
/// `ProcChangeActivePointerGrab`).
const POINTER_GRAB_MASK: u32 = 0x7FFC;
/// X.h `AnyModifier` — wire encoding 1<<15.
const ANY_MODIFIER: u16 = 0x8000;
/// X.h `AllModifiersMask` — Shift..Mod5, the 8 real modifier bits.
/// `modifiers` values outside this (other than `AnyModifier`) get
/// `BadValue` (Xorg `CheckGrabValues`).
const ALL_MODIFIERS_MASK: u16 = 0x00FF;

pub(super) fn handle_grab_pointer(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    let mut status: u8 = 0;
    // Logged with the grab result so an input trace shows whether the
    // client asked for pointer confinement at all — SDL's relative-mouse
    // mode passes its own window as confine_to (#99).
    let mut logged_grab_window = ResourceId(0);
    let mut logged_confine_to = ResourceId(0);
    if body.len() >= 20 {
        // GrabPointer wire shape: header opcode/data/length, then
        // window(4) event-mask(2) pointer-mode(1) keyboard-mode(1)
        // confine-to(4) cursor(4) time(4). The `owner_events` BOOL
        // lives in the request header's `data` byte (header.data),
        // not in the body.
        let owner_events = header.data != 0;
        let grab_window = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
        let event_mask = u16::from_le_bytes([body[4], body[5]]);
        let confine_to = ResourceId(u32::from_le_bytes([body[8], body[9], body[10], body[11]]));
        let cursor = ResourceId(u32::from_le_bytes([body[12], body[13], body[14], body[15]]));
        let time = u32::from_le_bytes([body[16], body[17], body[18], body[19]]);
        logged_grab_window = grab_window;
        logged_confine_to = confine_to;
        // Validation, in Xorg's order (ProcGrabPointer → GrabDevice):
        // eventMask → confine_to lookup → keyboard/pointer mode →
        // owner_events → grab_window lookup → cursor lookup.
        if u32::from(event_mask) & !POINTER_GRAB_MASK != 0 {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                u32::from(event_mask),
                26,
            );
        }
        if confine_to.0 != 0 && state.resources.window(confine_to).is_none() {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_WINDOW,
                confine_to.0,
                26,
            );
        }
        if body[7] > 1 {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                u32::from(body[7]),
                26,
            );
        }
        if body[6] > 1 {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                u32::from(body[6]),
                26,
            );
        }
        if header.data > 1 {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                u32::from(header.data),
                26,
            );
        }
        if state.resources.window(grab_window).is_none() {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_WINDOW,
                grab_window.0,
                26,
            );
        }
        if cursor.0 != 0 && !state.resources.cursor_exists(cursor) {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_CURSOR,
                cursor.0,
                26,
            );
        }
        // Grab status, in Xorg GrabDevice's check order (X.h values:
        // GrabSuccess 0, AlreadyGrabbed 1, GrabInvalidTime 2,
        // GrabNotViewable 3, GrabFrozen 4). Failure leaves all grab
        // state untouched.
        let now = state
            .timestamp_now()
            .max(state.xi1_last_input_time)
            .max(state.last_pointer_grab_time);
        let viewable = state
            .resources
            .window(grab_window)
            .is_some_and(|w| w.map_state == crate::resources::MapState::Viewable);
        let confine_viewable = confine_to.0 == 0
            || state
                .resources
                .window(confine_to)
                .is_some_and(|w| w.map_state == crate::resources::MapState::Viewable);
        let grabbed_by_other = state
            .active_pointer_grab
            .is_some_and(|grab| grab.owner != client_id);
        // Frozen on behalf of another client's grab on the paired
        // device (Xorg: sync.frozen && sync.other && !SameClient).
        let frozen_by_other = state
            .xi1_frozen
            .get(&crate::xinput::DEVICEID_MASTER_POINTER)
            .is_some_and(|f| f.frozen() && f.other.is_some_and(|c| c != client_id));
        if grabbed_by_other {
            status = 1; // AlreadyGrabbed
        } else if !viewable || !confine_viewable {
            status = 3; // GrabNotViewable
        } else if time != 0
            && (crate::core_loop::xi1_focus::time_after(state.last_pointer_grab_time, time)
                || crate::core_loop::xi1_focus::time_after(time, now))
        {
            status = 2; // GrabInvalidTime
        } else if frozen_by_other {
            status = 4; // GrabFrozen
        } else {
            let prev_grab_window = state
                .active_pointer_grab
                .filter(|g| g.owner == client_id)
                .map(|g| g.grab_window);
            state.set_pointer_grab(crate::server::ActivePointerGrab {
                owner: client_id,
                grab_window,
                event_mask,
                cursor,
                time,
                owner_events,
                via_xi2: false,
                implicit: false,
                passive: false,
                xi2_mask: 0,
            });
            state.last_pointer_grab_time = if time == 0 { now } else { time };
            // Core↔XI bridge (Xorg has ONE deviceGrab per device): a core
            // sync grab freezes the device's XI1 event stream too — XTS
            // XAllowDeviceEvents-3 freezes via XGrabPointer(GrabModeSync)
            // and thaws via XAllowDeviceEvents. body[6]=pointer_mode,
            // body[7]=keyboard_mode (0 = GrabModeSync).
            crate::core_loop::pointer_fanout::xi1_check_grab_for_syncs(
                state,
                crate::xinput::DEVICEID_MASTER_POINTER,
                client_id,
                body[6] == 0,
                body[7] == 0,
            );
            // Xorg ActivatePointerGrab: DoEnterLeaveEvents(oldWin →
            // grab window, NotifyGrab), where oldWin is the prior
            // grab's window when re-grabbing, else the pointer window.
            // marco's title-bar popup and GTK3 menus key off these.
            let from_win = prev_grab_window
                .unwrap_or_else(|| crate::core_loop::key_fanout::deepest_window_at_pointer(state));
            emit_core_pointer_grab_chain(state, from_win, grab_window, 1);
            // ConfineCursorToWindow: record the confinement and pull
            // the pointer inside the confine window if it is outside.
            state.pointer_confine_to = confine_to;
            confine_pointer_now(state, backend);
            // Xorg ActivatePointerGrab installs the grab's cursor on the
            // displayed sprite for the grab's duration (ImageMagick
            // `import` grabs with a crosshair — #90). `cursor == None`
            // (xid 0) means "no override"; per-window cursors show
            // through. A re-grab by the same client with a new cursor
            // replaces the override here. Resolved to a host cursor
            // handle the same way XIChangeCursor does.
            if cursor.0 == 0 {
                let _ = backend.set_grab_cursor(None, None);
            } else if let Some(host) = state.resources.cursor_host_xid(cursor) {
                let _ = backend.set_grab_cursor(None, Some(host));
            }
        }
    }
    debug!(
        "client {} #{} GrabPointer status={status} window={:#x} confine_to={:#x}",
        client_id.0, sequence.0, logged_grab_window.0, logged_confine_to.0
    );
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let byte_order = client.byte_order;
    let mut buf: Vec<u8> = Vec::with_capacity(32);
    x11::write_grab_reply(&mut buf, byte_order, sequence, status)?;
    Ok(write_to_client(client, client_id, &buf))
}

pub(super) fn handle_ungrab_pointer(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    // Xorg ProcUngrabPointer: deactivate only when the request time is
    // neither LATER than the current time nor EARLIER than the
    // last-grab time, AND the active grab belongs to this client.
    let time = body
        .get(0..4)
        .map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let now = state
        .timestamp_now()
        .max(state.xi1_last_input_time)
        .max(state.last_pointer_grab_time);
    if time != 0
        && (crate::core_loop::xi1_focus::time_after(time, now)
            || crate::core_loop::xi1_focus::time_after(state.last_pointer_grab_time, time))
    {
        debug!(
            "client {} #{} UngrabPointer ignored (time {time} outside [{}, {now}])",
            client_id.0, sequence.0, state.last_pointer_grab_time
        );
        return Ok(RequestOutcome::Handled);
    }
    let held_by_client = state
        .active_pointer_grab
        .is_some_and(|grab| grab.owner == client_id);
    if !held_by_client {
        debug!(
            "client {} #{} UngrabPointer ignored (no grab held by client)",
            client_id.0, sequence.0
        );
        return Ok(RequestOutcome::Handled);
    }
    deactivate_core_pointer_grab(state, backend, client_id);
    debug!("client {} #{} UngrabPointer", client_id.0, sequence.0);
    Ok(RequestOutcome::Handled)
}

/// Tear down the active core pointer grab held by `client_id` —
/// shared by UngrabPointer and the unmap/destroy deactivation path
/// (Xorg `DeactivateGrab` reached from `DeleteWindowFromAnyEvents`).
/// Passive-grab NOT_GRABBED (Replay) teardown: clear the active passive
/// pointer grab and emit the `NotifyUngrab` crossing chain (grab window
/// → sprite), mirroring the activation chain from pointer_fanout. Does
/// NOT release any core↔XI grab bridge — the Replay path deliberately
/// omits `xi1_core_grab_bridge_release` (Xorg re-checks device grabs on
/// the replayed event). Shared by `apply_allow_events` and the XI1
/// ReplayThisDevice path.
pub(super) fn deactivate_passive_pointer_grab_crossings(state: &mut ServerState) {
    let prev_grab = state.active_pointer_grab.filter(|grab| grab.passive);
    let prev_grab_window = prev_grab.map(|grab| grab.grab_window);
    state.clear_pointer_grab();
    state.pointer_confine_to = ResourceId(0);
    // Xorg DeactivatePointerGrab clears every `sync.other` held on the
    // dying grab's behalf: a keyboard frozen by the button grab's sync
    // keyboard mode thaws with it, or it stays frozen with no grab left
    // to AllowEvents it.
    // A core passive grab freezes the master pointer and holds its
    // paired master keyboard (`xi1_check_grab_for_syncs`).
    if let Some(grab) = prev_grab
        && let Some(paired) = crate::core_loop::pointer_fanout::xi1_other_input_device(
            state,
            crate::xinput::DEVICEID_MASTER_POINTER,
        )
        && let Some(kbd) = state.xi1_frozen.get_mut(&paired)
        && kbd.other == Some(grab.owner)
    {
        kbd.other = None;
    }
    if let Some(prev) = prev_grab_window {
        let to_win = crate::core_loop::key_fanout::deepest_window_at_pointer(state);
        emit_core_pointer_grab_chain(state, prev, to_win, 2); // NotifyUngrab
    }
}

pub(super) fn deactivate_core_pointer_grab(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
) {
    state.pointer_confine_to = ResourceId(0);
    let prev_grab_window = state
        .active_pointer_grab
        .filter(|g| g.owner == client_id)
        .map(|g| g.grab_window);
    state.clear_pointer_grab();
    state
        .xi1_frozen
        .entry(crate::xinput::DEVICEID_MASTER_POINTER)
        .or_default()
        .stored = None;
    // Xorg DeactivatePointerGrab reverts the sprite from the grab
    // cursor back to the per-window/default cursor. Clearing an
    // override that was never set is a cheap no-op.
    let _ = backend.set_grab_cursor(None, None);
    // Core↔XI bridge: release any XI1-side hold the core grab placed.
    crate::core_loop::pointer_fanout::xi1_core_grab_bridge_release(
        state,
        crate::xinput::DEVICEID_MASTER_POINTER,
        client_id,
    );
    if let Some(prev) = prev_grab_window {
        // Xorg DeactivatePointerGrab: DoEnterLeaveEvents(grab window
        // → pointer window, NotifyUngrab).
        let to_win = crate::core_loop::key_fanout::deepest_window_at_pointer(state);
        emit_core_pointer_grab_chain(state, prev, to_win, 2);
    }
}

/// Pull the pointer inside the current confinement rectangle when it
/// sits outside — Xorg `ConfineCursorToWindow` → `CheckPhysLimits`
/// (the warp generates motion/crossing events through the normal
/// input path). Used at grab activation and when the confine window
/// moves/resizes.
pub(crate) fn confine_pointer_now(state: &mut ServerState, backend: &mut dyn Backend) {
    let win = state.pointer_confine_to;
    if win.0 == 0 {
        return;
    }
    let Some(w) = state.resources.window(win) else {
        return;
    };
    if w.map_state != crate::resources::MapState::Viewable {
        return;
    }
    let (x0, y0) = state.resources.window_absolute_position(win);
    let (x1, y1) = (x0 + i32::from(w.width), y0 + i32::from(w.height));
    let (px, py) = state.pointer_root;
    let cx = i32::from(px).clamp(x0, (x1 - 1).max(x0));
    let cy = i32::from(py).clamp(y0, (y1 - 1).max(y0));
    if cx != i32::from(px) || cy != i32::from(py) {
        debug!(
            "confine_pointer_now: warping ({px},{py}) -> ({cx},{cy}) inside 0x{:x}",
            win.0
        );
        backend.warp_pointer_root(state, cx, cy);
    }
}

/// Full Enter/Leave chain for pointer-grab activation/deactivation —
/// Xorg `ActivatePointerGrab`/`DeactivatePointerGrab` →
/// `DoEnterLeaveEvents(sprite.win ↔ grab window, NotifyGrab/Ungrab)`.
/// Events flow through the normal per-window mask filter (EnterWindow
/// 0x10 / LeaveWindow 0x20) to every selecting client, then the same chain
/// in XI2 form (`DeviceEnterLeaveEvents`), from the master pointer.
pub(crate) fn emit_core_pointer_grab_chain(
    state: &mut ServerState,
    from_win: ResourceId,
    to_win: ResourceId,
    mode: u8,
) {
    if from_win == to_win {
        return;
    }
    let chain = crate::crossings::normal_mode_crossings(state, from_win, to_win);
    let (root_x, root_y) = state.pointer_root;
    let server_time = state.timestamp_now();
    for e in &chain {
        let (mask, enter) = match e.kind {
            crate::crossings::CrossingKind::Enter => (0x10u32, true),
            crate::crossings::CrossingKind::Leave => (0x20u32, false),
        };
        let (ox, oy) = state.resources.window_absolute_position(e.window);
        let event_x = i16::try_from(i32::from(root_x) - ox).unwrap_or(i16::MAX);
        let event_y = i16::try_from(i32::from(root_y) - oy).unwrap_or(i16::MAX);
        let focus = state.crossing_has_focus(e.window);
        let _dropped = emit_window_event_to_state(state, e.window, mask, |buf, seq, order| {
            let crossing = yserver_protocol::x11::CrossingEvent {
                sequence: seq,
                time: server_time,
                root: ROOT_WINDOW,
                event: e.window,
                child: e.child,
                root_x,
                root_y,
                event_x,
                event_y,
                state: 0,
                detail: e.detail,
                mode,
                focus,
            };
            if enter {
                x11::encode_enter_notify_event(buf, order, crossing);
            } else {
                x11::encode_leave_notify_event(buf, order, crossing);
            }
        });
    }
    for e in chain {
        let evtype: u16 = match e.kind {
            crate::crossings::CrossingKind::Enter => 7,
            crate::crossings::CrossingKind::Leave => 8,
        };
        // Xorg sends these before the grab is installed and after it is
        // gone, so the window's selections get them, whatever the grab.
        let targets =
            crate::core_loop::pointer_fanout::xi2_master_selectors(state, e.window, evtype);
        if targets.is_empty() {
            continue;
        }
        let (ox, oy) = state.resources.window_absolute_position(e.window);
        let event_x = i16::try_from(i32::from(root_x) - ox).unwrap_or(i16::MAX);
        let event_y = i16::try_from(i32::from(root_y) - oy).unwrap_or(i16::MAX);
        let focus = state.crossing_has_focus(e.window);
        let _dropped = fanout_event_to_clients(state, &targets, |buf, seq, order| {
            x11::encode_xi2_crossing_event(
                buf,
                order,
                seq,
                XI2_MAJOR_OPCODE,
                evtype,
                2,
                server_time,
                ROOT_WINDOW,
                e.window,
                root_x,
                root_y,
                event_x,
                event_y,
                0,
                mode,
                e.detail,
                2,
                focus,
            );
        });
    }
}

pub(super) fn handle_grab_button(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    if body.len() >= 20 {
        let button = body[16];
        let grab_window = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
        let event_mask = u32::from(u16::from_le_bytes([body[4], body[5]]));
        let pointer_mode = body[6];
        let keyboard_mode = body[7];
        let confine_to = ResourceId(u32::from_le_bytes([body[8], body[9], body[10], body[11]]));
        let cursor = ResourceId(u32::from_le_bytes([body[12], body[13], body[14], body[15]]));
        let owner_events = header.data != 0;
        let modifiers = u16::from_le_bytes([body[18], body[19]]);
        // Validation, in Xorg's ProcGrabButton order: pointer_mode →
        // keyboard_mode → modifiers → owner_events → event_mask →
        // grab_window → confine_to → cursor → conflicting-grab
        // BadAccess (AddPassiveGrabToList).
        if pointer_mode > 1 {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                u32::from(pointer_mode),
                28,
            );
        }
        if keyboard_mode > 1 {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                u32::from(keyboard_mode),
                28,
            );
        }
        if modifiers != ANY_MODIFIER && modifiers & !ALL_MODIFIERS_MASK != 0 {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                u32::from(modifiers),
                28,
            );
        }
        if header.data > 1 {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                u32::from(header.data),
                28,
            );
        }
        if event_mask & !POINTER_GRAB_MASK != 0 {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                event_mask,
                28,
            );
        }
        if state.resources.window(grab_window).is_none() {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_WINDOW,
                grab_window.0,
                28,
            );
        }
        if confine_to.0 != 0 && state.resources.window(confine_to).is_none() {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_WINDOW,
                confine_to.0,
                28,
            );
        }
        if cursor.0 != 0 && !state.resources.cursor_exists(cursor) {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_CURSOR,
                cursor.0,
                28,
            );
        }
        // A passive grab that would override another client's grab on
        // the same window is a BadAccess (Xorg GrabMatchesSecond via
        // AddPassiveGrabToList): button details overlap when either
        // side is AnyButton(0) or they are equal; modifiers likewise
        // with AnyModifier. Core grabs only conflict with core grabs
        // (grabtype mismatch never matches).
        let conflicts = state.button_grabs.iter().any(|g| {
            g.grab_window == grab_window
                && g.owner != client_id
                && !g.via_xi2
                && (g.button == button || g.button == 0 || button == 0)
                && (g.modifiers == modifiers
                    || g.modifiers == ANY_MODIFIER
                    || modifiers == ANY_MODIFIER)
        });
        if conflicts {
            return emit_x11_error(state, client_id, sequence, x11::error::BAD_ACCESS, 0, 28);
        }
        state.button_grabs.retain(|g| {
            !(g.owner == client_id
                && g.grab_window == grab_window
                && g.button == button
                && g.modifiers == modifiers)
        });
        state.button_grabs.push(crate::server::PassiveButtonGrab {
            device_id: 0,
            owner: client_id,
            grab_window,
            button,
            modifiers,
            owner_events,
            event_mask,
            pointer_mode,
            keyboard_mode,
            confine_to,
            via_xi2: false,
        });
        debug!(
            "client {} GrabButton window=0x{:x} button={} modifiers=0x{:x}",
            client_id.0, grab_window.0, button, modifiers
        );
    }
    debug!("client {} #{} GrabButton", client_id.0, sequence.0);
    Ok(RequestOutcome::Handled)
}

pub(super) fn handle_ungrab_button(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    if body.len() >= 6 {
        let button = header.data;
        let grab_window = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
        let modifiers = u16::from_le_bytes([body[4], body[5]]);
        // Xorg ProcUngrabButton: modifiers BadValue → window BadWindow.
        if modifiers != ANY_MODIFIER && modifiers & !ALL_MODIFIERS_MASK != 0 {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                u32::from(modifiers),
                29,
            );
        }
        if state.resources.window(grab_window).is_none() {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_WINDOW,
                grab_window.0,
                29,
            );
        }
        state.button_grabs.retain(|g| {
            !(g.owner == client_id
                && g.grab_window == grab_window
                && (g.button == button || button == 0)
                && (g.modifiers == modifiers || modifiers == 0x8000))
        });
        debug!(
            "client {} UngrabButton window=0x{:x} button={} modifiers=0x{:x}",
            client_id.0, grab_window.0, button, modifiers
        );
    }
    debug!("client {} #{} UngrabButton", client_id.0, sequence.0);
    Ok(RequestOutcome::Handled)
}

pub(super) fn handle_change_active_pointer_grab(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    if body.len() >= 12 {
        let cursor = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
        let time = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
        let event_mask = u16::from_le_bytes([body[8], body[9]]);
        // Xorg ProcChangeActivePointerGrab: eventMask BadValue →
        // cursor BadCursor; both fire even when no grab is active.
        if u32::from(event_mask) & !POINTER_GRAB_MASK != 0 {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                u32::from(event_mask),
                30,
            );
        }
        if cursor.0 != 0 && !state.resources.cursor_exists(cursor) {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_CURSOR,
                cursor.0,
                30,
            );
        }
        // Xorg ProcChangeActivePointerGrab: no-op when the request
        // time is LATER than current or EARLIER than the grab time.
        let now = state
            .timestamp_now()
            .max(state.xi1_last_input_time)
            .max(state.last_pointer_grab_time);
        let time_ok = time == 0
            || (!crate::core_loop::xi1_focus::time_after(state.last_pointer_grab_time, time)
                && !crate::core_loop::xi1_focus::time_after(time, now));
        if time_ok
            && let Some(g) = state.active_pointer_grab.as_mut()
            && g.owner == client_id
        {
            g.event_mask = event_mask;
            g.cursor = cursor;
        }
    }
    debug!(
        "client {} #{} ChangeActivePointerGrab",
        client_id.0, sequence.0
    );
    Ok(RequestOutcome::Handled)
}

pub(super) fn handle_grab_keyboard(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    let mut status: u8 = 0;
    if body.len() >= 12 {
        let grab_window = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
        // Validation per Xorg GrabDevice: keyboard_mode (body[9]) →
        // pointer_mode (body[8]) → owner_events (header data byte) →
        // grab_window lookup.
        if body[9] > 1 {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                u32::from(body[9]),
                31,
            );
        }
        if body[8] > 1 {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                u32::from(body[8]),
                31,
            );
        }
        if header.data > 1 {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                u32::from(header.data),
                31,
            );
        }
        if state.resources.window(grab_window).is_none() {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_WINDOW,
                grab_window.0,
                31,
            );
        }
        // Grab status, mirroring handle_grab_pointer (Xorg GrabDevice
        // check order). Failure leaves all grab state untouched.
        let time = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
        let now = state
            .timestamp_now()
            .max(state.xi1_last_input_time)
            .max(state.last_keyboard_grab_time);
        let viewable = state
            .resources
            .window(grab_window)
            .is_some_and(|w| w.map_state == crate::resources::MapState::Viewable);
        let grabbed_by_other = state
            .active_keyboard_grab
            .is_some_and(|g| g.owner != client_id);
        let frozen_by_other = state
            .xi1_frozen
            .get(&crate::xinput::DEVICEID_MASTER_KEYBOARD)
            .is_some_and(|f| f.frozen() && f.other.is_some_and(|c| c != client_id));
        if grabbed_by_other {
            status = 1; // AlreadyGrabbed
        } else if !viewable {
            status = 3; // GrabNotViewable
        } else if time != 0
            && (crate::core_loop::xi1_focus::time_after(state.last_keyboard_grab_time, time)
                || crate::core_loop::xi1_focus::time_after(time, now))
        {
            status = 2; // GrabInvalidTime
        } else if frozen_by_other {
            status = 4; // GrabFrozen
        } else {
            let prev_grab_window = state
                .active_keyboard_grab
                .filter(|g| g.owner == client_id)
                .map(|g| g.grab_window);
            state.active_keyboard_grab = Some(crate::server::ActiveKeyboardGrab {
                owner: client_id,
                grab_window,
                source: crate::server::ActiveKeyboardGrabSource::Explicit,
                owner_events: header.data != 0,
                via_xi2: false,
                xi2_mask: 0,
            });
            state.last_keyboard_grab_time = if time == 0 { now } else { time };
            // Core↔XI bridge — see handle_grab_pointer. GrabKeyboard wire:
            // window(4) time(4) pointer_mode(1) keyboard_mode(1); this
            // device = keyboard, other = pointer.
            crate::core_loop::pointer_fanout::xi1_check_grab_for_syncs(
                state,
                crate::xinput::DEVICEID_MASTER_KEYBOARD,
                client_id,
                body[9] == 0,
                body[8] == 0,
            );
            // Xorg ActivateKeyboardGrab: DoFocusEvents(oldWin →
            // grab_window, NotifyGrab), where oldWin is the prior
            // grab's window if one was active, else the focus.
            let from_raw = prev_grab_window.map_or(state.core_focus.raw, |w| w.0);
            emit_core_focus_transition(state, from_raw, grab_window.0, 1);
        }
    }
    debug!(
        "client {} #{} GrabKeyboard status={status}",
        client_id.0, sequence.0
    );
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let byte_order = client.byte_order;
    let mut buf: Vec<u8> = Vec::with_capacity(32);
    x11::write_grab_reply(&mut buf, byte_order, sequence, status)?;
    Ok(write_to_client(client, client_id, &buf))
}

pub(super) fn handle_ungrab_keyboard(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    // Time validation — see handle_ungrab_pointer (Xorg
    // ProcUngrabKeyboard has the identical CompareTimeStamps gate).
    let time = body
        .get(0..4)
        .map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let now = state
        .timestamp_now()
        .max(state.xi1_last_input_time)
        .max(state.last_keyboard_grab_time);
    if time != 0
        && (crate::core_loop::xi1_focus::time_after(time, now)
            || crate::core_loop::xi1_focus::time_after(state.last_keyboard_grab_time, time))
    {
        debug!(
            "client {} #{} UngrabKeyboard ignored (time {time} outside [{}, {now}])",
            client_id.0, sequence.0, state.last_keyboard_grab_time
        );
        return Ok(RequestOutcome::Handled);
    }
    deactivate_core_keyboard_grab(state, client_id);
    debug!("client {} #{} UngrabKeyboard", client_id.0, sequence.0);
    Ok(RequestOutcome::Handled)
}

/// Tear down the active core keyboard grab held by `client_id` —
/// shared by UngrabKeyboard and the unmap/destroy deactivation path
/// (see [`deactivate_core_pointer_grab`]).
fn deactivate_core_keyboard_grab(state: &mut ServerState, client_id: ClientId) {
    let prev_grab_window = state
        .active_keyboard_grab
        .filter(|g| g.owner == client_id)
        .map(|g| g.grab_window);
    if state
        .active_keyboard_grab
        .is_some_and(|g| g.owner == client_id)
    {
        state.active_keyboard_grab = None;
        state
            .xi1_frozen
            .entry(crate::xinput::DEVICEID_MASTER_KEYBOARD)
            .or_default()
            .stored = None;
        // Core↔XI bridge: release any XI1-side hold the grab placed.
        crate::core_loop::pointer_fanout::xi1_core_grab_bridge_release(
            state,
            crate::xinput::DEVICEID_MASTER_KEYBOARD,
            client_id,
        );
    }
    if let Some(prev) = prev_grab_window {
        // Xorg DeactivateKeyboardGrab: DoFocusEvents(grab_window →
        // focus, NotifyUngrab).
        emit_core_focus_transition(state, prev.0, state.core_focus.raw, 2);
    }
}

/// Xorg `DeleteWindowFromAnyEvents` grab leg: an active core grab
/// whose grab window stopped being viewable (unmap of it or an
/// ancestor, or destroy) deactivates. Call after the map-state /
/// resource changes have landed.
pub(super) fn release_core_grabs_for_unviewable(
    state: &mut ServerState,
    backend: &mut dyn Backend,
) {
    let viewable = |state: &ServerState, w: ResourceId| {
        state
            .resources
            .window(w)
            .is_some_and(|win| win.map_state == crate::resources::MapState::Viewable)
    };
    if let Some(grab) = state.active_pointer_grab
        && (!viewable(state, grab.grab_window)
            || (state.pointer_confine_to.0 != 0 && !viewable(state, state.pointer_confine_to)))
    {
        deactivate_core_pointer_grab(state, backend, grab.owner);
    }
    if let Some(g) = state.active_keyboard_grab
        && !viewable(state, g.grab_window)
    {
        deactivate_core_keyboard_grab(state, g.owner);
    }
}

pub(super) fn handle_grab_key(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    if let Some(req) = x11::parse_grab_key(body, header.data != 0) {
        let grab_window = ResourceId(req.grab_window);
        // Validation per Xorg ProcGrabKey: CheckGrabValues (modes,
        // owner_events, modifiers) → keycode range → grab_window
        // lookup → conflicting-grab BadAccess.
        if req.keyboard_mode > 1 {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                u32::from(req.keyboard_mode),
                33,
            );
        }
        if req.pointer_mode > 1 {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                u32::from(req.pointer_mode),
                33,
            );
        }
        if header.data > 1 {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                u32::from(header.data),
                33,
            );
        }
        if req.modifiers != ANY_MODIFIER && req.modifiers & !ALL_MODIFIERS_MASK != 0 {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                u32::from(req.modifiers),
                33,
            );
        }
        // Keycode must be within the advertised [min_keycode,
        // max_keycode] range (8..=255 — the setup reply's values) or
        // AnyKey(0).
        if req.keycode != 0 && req.keycode < 8 {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                u32::from(req.keycode),
                33,
            );
        }
        if state.resources.window(grab_window).is_none() {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_WINDOW,
                grab_window.0,
                33,
            );
        }
        // BadAccess when another client already holds an overlapping
        // passive key grab on this window — see the GrabButton
        // conflict check for the overlap rule.
        let conflicts = state.key_grabs.iter().any(|g| {
            g.grab_window == grab_window
                && g.owner != client_id
                && !g.via_xi2
                && (g.keycode == req.keycode || g.keycode == 0 || req.keycode == 0)
                && (g.modifiers == req.modifiers
                    || g.modifiers == ANY_MODIFIER
                    || req.modifiers == ANY_MODIFIER)
        });
        if conflicts {
            return emit_x11_error(state, client_id, sequence, x11::error::BAD_ACCESS, 0, 33);
        }
        state.key_grabs.retain(|g| {
            !(g.owner == client_id
                && g.grab_window == grab_window
                && g.keycode == req.keycode
                && g.modifiers == req.modifiers)
        });
        state.key_grabs.push(crate::server::KeyGrab {
            device_id: 0,
            owner: client_id,
            grab_window,
            keycode: req.keycode,
            modifiers: req.modifiers,
            owner_events: req.owner_events,
            pointer_mode: req.pointer_mode,
            keyboard_mode: req.keyboard_mode,
            via_xi2: false,
            xi2_mask: 0,
        });
        debug!(
            "client {} GrabKey window=0x{:x} keycode={} modifiers=0x{:x}",
            client_id.0, req.grab_window, req.keycode, req.modifiers
        );
    }
    debug!("client {} #{} GrabKey", client_id.0, sequence.0);
    Ok(RequestOutcome::Handled)
}

pub(super) fn handle_ungrab_key(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    if let Some(req) = x11::parse_ungrab_key(body, header.data) {
        let grab_window = ResourceId(req.grab_window);
        // Xorg ProcUngrabKey: window lookup → keycode range →
        // modifiers BadValue.
        if state.resources.window(grab_window).is_none() {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_WINDOW,
                grab_window.0,
                34,
            );
        }
        if req.keycode != 0 && req.keycode < 8 {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                u32::from(req.keycode),
                34,
            );
        }
        if req.modifiers != ANY_MODIFIER && req.modifiers & !ALL_MODIFIERS_MASK != 0 {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                u32::from(req.modifiers),
                34,
            );
        }
        state.key_grabs.retain(|g| {
            !(g.owner == client_id
                && g.grab_window == grab_window
                && (g.keycode == req.keycode || req.keycode == 0)
                && (g.modifiers == req.modifiers || req.modifiers == 0x8000))
        });
    }
    debug!("client {} #{} UngrabKey", client_id.0, sequence.0);
    Ok(RequestOutcome::Handled)
}
