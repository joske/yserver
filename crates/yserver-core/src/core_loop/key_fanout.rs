//! State-borrowing key event fanout.
//!
//! KeyPress / KeyRelease delivery follows the X11 keyboard model: at
//! most one window receives the event (the active grab's window if a
//! grab is in effect, otherwise the focused window). The event is
//! emitted to subscribers with `KeyPressMask` / `KeyReleaseMask` on
//! that window. XI2 device-event subscribers on the same window also
//! receive a parallel XI2 KeyPress / KeyRelease.
//!
//! "Focus" comes from the per-client `ClientState::focused_window`.
//! In practice all clients share the same value (every `SetInputFocus`
//! mirrors it across clients), so the helper picks the first non-ROOT
//! focus it sees. When every client is rooted, the event is dropped.

use yserver_protocol::x11::{self, ClientId, ResourceId};

use crate::{
    core_loop::fanout::{fanout_event_to_clients, subscribers_by_id},
    host_x11::HostKeyEvent,
    resources::ROOT_WINDOW,
    server::{ActiveKeyboardGrab, ActiveKeyboardGrabSource, ServerState, xi2_mask_for_client},
};

const KEY_PRESS_MASK: u32 = 0x0000_0001;
const KEY_RELEASE_MASK: u32 = 0x0000_0002;
const XI2_MAJOR_OPCODE: u8 = 137;
const XI2_KEYPRESS_EVTYPE: u16 = 2;
const XI2_KEYRELEASE_EVTYPE: u16 = 3;
const XI2_MASTER_KEYBOARD_DEVICE_ID: u16 = 3;
const XI2_SLAVE_KEYBOARD_DEVICE_ID: u16 = 5;
const XI2_RAW_KEY_PRESS_EVTYPE: u16 = 13;
const XI2_RAW_KEY_RELEASE_EVTYPE: u16 = 14;
/// XISelectEvents wildcard deviceids.
const XI2_ALL_DEVICES: u16 = 0;
const XI2_ALL_MASTER_DEVICES: u16 = 1;

/// Fan a host key event out to nested clients.
///
/// Returns the deduped list of clients whose outbound buffer overflowed
/// during the fanout — the caller (run_core) issues
/// `Message::ClientDisconnected` for each.
pub fn key_event_fanout_to_state(
    state: &mut ServerState,
    backend: &mut dyn crate::backend::Backend,
    event: HostKeyEvent,
) -> Vec<ClientId> {
    // QueryKeymap bitmap — device key state tracks the physical
    // event regardless of where (or whether) it gets delivered.
    //
    // NOTE: we deliberately do NOT reconstruct modifier state here and
    // stamp it onto the event. The backend already cooks the
    // authoritative xkb modifier state into `event.state`
    // (`cook_host_key` → `serialize_modifiers`, and XTest fakes are
    // cooked the same way via `on_host_input`). A second server-side
    // tracker (keys_down × modifier-map) is a redundant source of
    // truth that drifts from xkb — `synthesize_held_releases` on a
    // VT-switch clears keys_down without touching xkb — and any
    // `state == 0` override then clobbers every unmodified keypress
    // with the stale modifier ("stuck Ctrl, can't type in wezterm").
    {
        let byte = usize::from(event.keycode / 8);
        let bit = 1u8 << (event.keycode % 8);
        if event.pressed {
            state.keys_down[byte] |= bit;
        } else {
            state.keys_down[byte] &= !bit;
        }
    }
    // DPMS: any key resets the idle timer; from any non-On level
    // we wake the screen *before* fanning out, so the first event
    // of the resumed session lands on a visible scanout.
    let now = std::time::Instant::now();
    // Capture priors BEFORE mutating; needed by the IDLETIME wake handler.
    #[allow(clippy::cast_possible_truncation)]
    let prior_global = now
        .duration_since(state.dpms.last_activity)
        .as_millis()
        .min(u128::from(u32::MAX)) as i64;
    // XI2 master device IDs are always small (3 here); cast u16 → u8 is safe.
    // Per-device prior: fall back to global if no per-device entry yet.
    // Matches `idletime_baseline`'s fallback (server.rs Task 1) — without
    // this, the very first input event for a device whose baseline isn't
    // recorded would compute prior_device=0 and a per-device Negative
    // alarm (whose wait_value > 0) would not see the `old > wait` half of
    // its trigger.
    let prior_device = state
        .per_device_last_activity
        .get(&(XI2_MASTER_KEYBOARD_DEVICE_ID as u8))
        .copied()
        .map(|t| {
            #[allow(clippy::cast_possible_truncation)]
            let v = now.duration_since(t).as_millis().min(u128::from(u32::MAX)) as i64;
            v
        })
        .unwrap_or(prior_global);

    state.dpms.last_activity = now;
    state
        .per_device_last_activity
        .insert(XI2_MASTER_KEYBOARD_DEVICE_ID as u8, now);

    // IDLETIME wake: fires Negative-* alarms before the input event itself
    // reaches clients (predictable ordering).
    crate::core_loop::process_request::evaluate_idletime_negative_alarms_on_input_wake(
        state,
        XI2_MASTER_KEYBOARD_DEVICE_ID as u8,
        prior_global,
        prior_device,
    );

    if state.dpms.enabled && state.dpms.power_level != 0 {
        crate::core_loop::process_request::apply_dpms_transition(state, backend, 0);
        // DPMS coupling tail already flipped SS Off if it was On.
    }
    if matches!(
        state.screensaver.active,
        crate::server::ScreenSaverActive::On
    ) {
        // Standalone SS activation (DPMS was On already; SS came up
        // via idle timer or ForceScreenSaver) — input wakes it.
        crate::core_loop::process_request::apply_screen_saver_transition(
            state,
            backend,
            crate::server::ScreenSaverActive::Off,
            /*forced=*/ false,
        );
    }

    // Unified device freeze (Xorg FreezeThaw switches the whole
    // device to the enqueue proc): while the keyboard device is
    // frozen, the WHOLE key event is withheld in the global pending queue.
    // The replay (`xi1_compute_freezes` → `deliver_routed_key`)
    // regenerates both the core and the XI1 form — queueing the XI1
    // form separately here would double-deliver it on thaw (XTS
    // XUngrabDevice-1: "expecting two events, got 4").
    if state
        .xi1_frozen
        .get(&crate::xinput::DEVICEID_SLAVE_KEYBOARD)
        .is_some_and(crate::server::Xi1Freeze::frozen)
    {
        state
            .sync_pending
            .push_back(crate::server::PendingSyncEvent {
                device: crate::xinput::DEVICEID_SLAVE_KEYBOARD,
                event: crate::server::QueuedInputEvent::HostKey(event),
            });
        return Vec::new();
    }

    let dropped = deliver_routed_key(state, event);

    // XkbStateNotify (GH #59): libxkbcommon-x11 clients (kitty/GLFW, all
    // of Wayland's X11 path) keep their xkb_state synchronized ONLY from
    // these events via `xkb_state_update_mask()` — NOT from the key
    // events. So if the effective modifiers OR the group changed across
    // this key event (cook_host_key already advanced the backend's
    // xkb_state), fan out a full XkbStateNotify to StateNotify selectors.
    // Without this a client that seeded a held modifier from XkbGetState
    // (e.g. kitty launched from a `super + Return` chord, with Mod4 held
    // at query time) never learns the modifier cleared on release, and
    // every key resolves to NoSymbol. The `last_xkb_*` anchors dedup
    // against the XkbLatchLockState handler so neither re-emits the other.
    let g = backend.current_group();
    let (eff_mods, base_mods, latched_mods, locked_mods) = backend.current_xkb_mods();
    if eff_mods != state.last_xkb_mods || g != state.last_xkb_group {
        let base = backend.xkb_info().map_or(0, |(_maj, ev, _err)| ev);
        let subs = crate::core_loop::xkb_layout::subscribers(state, 0x0004);
        // changed: all modifier components + the compat/grab/lookup mirrors
        // Xorg fills (0x1F0F); add the group bits on a group switch.
        let mut changed: u16 = 0x1F0F;
        if g != state.last_xkb_group {
            changed |= 0x0090; // XkbGroupStateMask | XkbGroupLockMask
        }
        let notify = x11::XkbStateNotify {
            device_id: 1,
            mods: eff_mods,
            base_mods,
            latched_mods,
            locked_mods,
            group: g,
            locked_group: g,
            changed,
            keycode: event.keycode,
            event_type: if event.pressed { 2 } else { 3 },
            request_major: 0,
            request_minor: 0,
        };
        let _redundant = fanout_event_to_clients(state, &subs, |buf, seq, order| {
            let _ = x11::write_xkb_state_notify(buf, order, seq, base, notify);
        });
        state.last_xkb_group = g;
        state.last_xkb_mods = eff_mods;
    }

    dropped
}

/// The routing+delivery tail of [`key_event_fanout_to_state`] —
/// callable without a backend so `xi1_compute_freezes` can replay
/// withheld core keys on thaw.
pub(crate) fn deliver_routed_key(state: &mut ServerState, event: HostKeyEvent) -> Vec<ClientId> {
    match key_route(state, &event) {
        // Core delivery has nowhere to go (focus on root, no grab) —
        // but the XI1 fanout routes by the extension keyboard's own
        // device focus (default PointerRoot → window under pointer),
        // so SelectExtensionEvent subscribers still receive DeviceKey
        // events.
        KeyRoute::Drop => deliver_xi1_focused_key(state, &event),
        KeyRoute::PassiveGrabOwner {
            owner,
            grab_window,
            freeze,
            owner_events,
            via_xi2,
        } => {
            // Synchronous passive key grab: hold the activating press
            // so AllowEvents(ReplayKeyboard) can replay it to the
            // focus window if the grab owner declines it. Mirrors the
            // sync passive-button-grab freeze in `pointer_fanout`.
            if freeze && event.pressed {
                state
                    .xi1_frozen
                    .entry(crate::xinput::DEVICEID_SLAVE_KEYBOARD)
                    .or_default()
                    .stored = Some(crate::server::QueuedInputEvent::HostKey(event));
            }
            // Xorg DeliverGrabbedEvent: with owner_events, key events
            // that would naturally land on one of the grab client's
            // windows are reported there instead of the grab window.
            let natural = if owner_events {
                key_grabbed_natural_target(state, &event, owner)
            } else {
                None
            };
            let mut dropped = if let Some(target) = natural {
                deliver_key_to_grab_owner(state, &event, owner, target, via_xi2)
            } else {
                deliver_key_to_grab_owner(state, &event, owner, grab_window, via_xi2)
            };
            // XI1 leg runs under grabs too — the router handles its
            // own grab/freeze semantics, including the armed
            // FreezeNextEvent / FreezeBothNextEvent trip that a key
            // event delivered through the (bridged) grab must fire
            // (XTS XAllowDeviceEvents-11/-12 SyncAll re-freeze).
            merge_dropped(&mut dropped, deliver_xi1_focused_key(state, &event));
            dropped
        }
        KeyRoute::Window(window) => {
            // Normal focus delivery: when the pointer window is a
            // descendant of the focus, the event window is the first
            // window from the pointer window up (bounded at the
            // focus) where a client selected the event — Xorg
            // DeliverFocusedEvent's pointer-walk leg.
            let target = focused_walk_target(state, window, &event);
            let mut dropped = deliver_key_to_window(state, &event, target);
            merge_dropped(&mut dropped, deliver_xi1_focused_key(state, &event));
            dropped
        }
    }
}

/// Replay a frozen key (held by a synchronous passive grab) to the
/// current focus window, bypassing grab matching. Called from the
/// AllowEvents `ReplayKeyboard` / XIAllowEvents `ReplayDevice` path
/// after the grab owner declines the key. Mirrors Xorg
/// `ComputeFreezes` → `DeliverFocusedEvent` (dix/events.c:1360).
pub fn replay_frozen_key_to_focus(state: &mut ServerState, event: HostKeyEvent) -> Vec<ClientId> {
    let focus = current_focus(state);
    if focus == ResourceId(0) {
        return Vec::new();
    }
    let mut dropped = deliver_key_to_window(state, &event, focus);
    merge_dropped(&mut dropped, deliver_xi1_focused_key(state, &event));
    dropped
}

/// Deliver a key event to a single window's subscribers — the normal
/// path (focus window, or an explicit-grab window). Core KeyPress/
/// KeyRelease to `KeyPressMask`/`KeyReleaseMask` subscribers, plus a
/// parallel XI2 device event to XI2 selectors on the same window.
fn deliver_key_to_window(
    state: &mut ServerState,
    event: &HostKeyEvent,
    target_window: ResourceId,
) -> Vec<ClientId> {
    let mask_bit = if event.pressed {
        KEY_PRESS_MASK
    } else {
        KEY_RELEASE_MASK
    };

    // Clients that selected XI2 for this key event on the window. Computed
    // first because XI2 *shadows* core per client: a client receiving the
    // XI2 form must NOT also receive the core form of the same physical
    // event (Xorg behaviour). Without this, a client that selects both
    // core (XSelectInput KeyPressMask) and XI2 (XISelectEvents) — e.g.
    // Chromium's Ozone X11 layer — gets every keystroke twice.
    let xi2_evtype = xi2_evtype_for(event);
    let xi2_targets: Vec<ClientId> = state
        .clients
        .iter()
        .filter_map(|(id, client)| {
            let mask = xi2_mask_for_client(
                client,
                target_window,
                target_window,
                &[
                    XI2_SLAVE_KEYBOARD_DEVICE_ID,
                    XI2_MASTER_KEYBOARD_DEVICE_ID,
                    1,
                    0,
                ],
            );
            if mask & (1 << xi2_evtype) != 0 {
                Some(ClientId(*id))
            } else {
                None
            }
        })
        .collect();

    // Core KeyPress/KeyRelease to KeyPressMask/KeyReleaseMask subscribers,
    // excluding any client already getting the XI2 form above.
    let core_targets: Vec<ClientId> = subscribers_by_id(state, target_window, mask_bit)
        .into_iter()
        .filter(|c| !xi2_targets.contains(c))
        .collect();
    let mut dropped = if core_targets.is_empty() {
        Vec::new()
    } else {
        fanout_event_to_clients(state, &core_targets, |buf, seq, order| {
            x11::encode_key_event(buf, order, key_event_wire(event, seq, target_window));
        })
    };

    if !xi2_targets.is_empty() {
        let xi2_dropped = fanout_event_to_clients(state, &xi2_targets, |buf, seq, order| {
            encode_key_xi2(buf, order, seq, event, target_window);
        });
        merge_dropped(&mut dropped, xi2_dropped);
    }

    dropped
}

/// Deliver a key event to the grab owner client, addressed to the
/// grab window. When a passive (or explicit) keyboard grab is active,
/// X11 delivers to the *grab owner* using the grab's event mask, not
/// to whichever clients happen to have selected key events on the
/// grab window. yserver previously delivered via window selection, so
/// a grab owner that registered the grab via `XIPassiveGrabDevice`
/// (without a matching `XISelectEvents` on the root) received nothing
/// and the key was lost.
///
/// Delivers EXACTLY ONE form, matching the grab's protocol (Xorg
/// `DeliverGrabbedEvent` consults the grab's own xi2mask — XI2 grab →
/// XI2 event, core grab → core event, never both):
/// - core grab (`via_xi2 == false`) → core KeyPress/KeyRelease only.
///   Sending XI2 XGE events to a core `GrabKey` owner NULL-derefs
///   libXi in clients that linked it without ever calling
///   XIQueryVersion (xts5 Xlib11 TCMs crash there).
/// - XI2 grab (`via_xi2 == true`) → XI2 `XI_KeyPress`/`XI_KeyRelease`
///   only. Sending the core form TOO double-delivers every keystroke
///   to a pure-XI2 client (muffin/cinnamon-shell), corrupting its key
///   state — observed as "can't type in the Cinnamon keyring dialog"
///   (x11trace: keycode delivered as both core KeyPress and XI2
///   XI_KeyPress to the same window).
fn deliver_key_to_grab_owner(
    state: &mut ServerState,
    event: &HostKeyEvent,
    owner: ClientId,
    grab_window: ResourceId,
    via_xi2: bool,
) -> Vec<ClientId> {
    if via_xi2 {
        fanout_event_to_clients(state, &[owner], |buf, seq, order| {
            encode_key_xi2(buf, order, seq, event, grab_window);
        })
    } else {
        fanout_event_to_clients(state, &[owner], |buf, seq, order| {
            x11::encode_key_event(buf, order, key_event_wire(event, seq, grab_window));
        })
    }
}

fn xi2_evtype_for(event: &HostKeyEvent) -> u16 {
    if event.pressed {
        XI2_KEYPRESS_EVTYPE
    } else {
        XI2_KEYRELEASE_EVTYPE
    }
}

fn key_event_wire(
    event: &HostKeyEvent,
    sequence: x11::SequenceNumber,
    target_window: ResourceId,
) -> x11::KeyEvent {
    x11::KeyEvent {
        pressed: event.pressed,
        keycode: event.keycode,
        sequence,
        time: event.time,
        root: ROOT_WINDOW,
        event: target_window,
        root_x: event.root_x,
        root_y: event.root_y,
        event_x: event.event_x,
        event_y: event.event_y,
        state: event.state,
    }
}

fn encode_key_xi2(
    buf: &mut Vec<u8>,
    order: x11::ClientByteOrder,
    seq: x11::SequenceNumber,
    event: &HostKeyEvent,
    target_window: ResourceId,
) {
    x11::encode_xi2_device_event(
        buf,
        order,
        seq,
        XI2_MAJOR_OPCODE,
        xi2_evtype_for(event),
        XI2_MASTER_KEYBOARD_DEVICE_ID,
        event.time,
        ROOT_WINDOW,
        target_window,
        ResourceId(0), // child=None; key events target the window directly
        event.root_x,
        event.root_y,
        event.event_x,
        event.event_y,
        event.state,
        u32::from(event.keycode),
        XI2_SLAVE_KEYBOARD_DEVICE_ID,
        0, // flags: no XIPointerEmulated on key events
    );
}

/// One XI2 raw key event (`XI_RawKeyPress` / `XI_RawKeyRelease`): Xorg's
/// `ET_RawKeyPress` / `ET_RawKeyRelease` internal event. Keys carry no
/// valuators, so the keycode, direction and timestamp are all of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawKeyEvent {
    pub keycode: u8,
    pub pressed: bool,
    /// Shared with the device event generated from the same input
    /// (Xorg `GetKeyboardEvents` stamps both with one `ms`).
    pub time: u32,
}

impl RawKeyEvent {
    fn evtype(self) -> u16 {
        if self.pressed {
            XI2_RAW_KEY_PRESS_EVTYPE
        } else {
            XI2_RAW_KEY_RELEASE_EVTYPE
        }
    }
}

/// Generate and deliver the XI2 raw key event for one piece of device key
/// input (libinput or XTEST — never software auto-repeat, see
/// `HostInputEvent::KeyRepeat`). Call it BEFORE the key's device-event
/// processing, including the duplicate press/release guard: Xorg builds the
/// raw event in `GetKeyboardEvents` (dix/getevents.c) and delivers it ahead
/// of the device event, and `Xi/exevents.c` only drops a duplicate down or a
/// stray up afterwards — so a release of a key that is not down still
/// produces a raw release.
///
/// `key_was_down`: the key's down state before this event. `is_modifier`:
/// the key is in the modifier map. A press of a key already down is Xorg's
/// "core repeating" case: it produces no event at all — raw included — when
/// auto-repeat is off globally or for the key, or the key is a modifier.
///
/// Delivery mirrors Xorg's two passes through `DeliverRawEvent`
/// (dix/events.c), slave first then master (mi/mieq.c): the slave form
/// (deviceid = sourceid = slave keyboard) goes to root selectors of the
/// slave keyboard / `XIAllDevices`; the master form (deviceid = master
/// keyboard) to root selectors of the master keyboard / `XIAllMasterDevices`
/// / `XIAllDevices`, subject to the keyboard grab. The master form is queued
/// behind a frozen keyboard like any other master event (Xorg
/// `EnqueueEvent`) and delivered on thaw; the slave is never frozen.
/// Keys arrive only on the master keyboard's attached slave, so an
/// `XIAllDevices` selector receives both forms.
pub fn raw_key_event_to_state(
    state: &mut ServerState,
    event: RawKeyEvent,
    key_was_down: bool,
    is_modifier: bool,
) -> Vec<ClientId> {
    if event.pressed
        && key_was_down
        && (is_modifier || !state.keyboard_control.key_auto_repeats(event.keycode))
    {
        return Vec::new();
    }
    // GetKeyboardEvents refuses keycodes below min_keycode.
    if event.keycode < 8 {
        return Vec::new();
    }
    let evtype = event.evtype();
    let slave_targets = raw_key_root_selectors(
        state,
        &[XI2_SLAVE_KEYBOARD_DEVICE_ID, XI2_ALL_DEVICES],
        evtype,
    );
    let mut dropped = send_raw_key(state, &slave_targets, event, XI2_SLAVE_KEYBOARD_DEVICE_ID);
    if state
        .xi1_frozen
        .get(&crate::xinput::DEVICEID_SLAVE_KEYBOARD)
        .is_some_and(crate::server::Xi1Freeze::frozen)
    {
        state
            .sync_pending
            .push_back(crate::server::PendingSyncEvent {
                device: crate::xinput::DEVICEID_SLAVE_KEYBOARD,
                event: crate::server::QueuedInputEvent::RawKey(event),
            });
    } else {
        merge_dropped(&mut dropped, deliver_raw_key_master(state, event));
    }
    dropped
}

/// The master-keyboard pass of Xorg `DeliverRawEvent`: first the keyboard
/// grab (`DeliverGrabbedEvent`), then every root selector of the master
/// keyboard, `XIAllMasterDevices` or `XIAllDevices`, filtered by
/// `FilterRawEvents`. Also the replay entry for a raw event that was queued
/// behind a frozen keyboard — the grab consulted is the one in effect when
/// the event is finally processed, as on Xorg.
pub(crate) fn deliver_raw_key_master(state: &mut ServerState, event: RawKeyEvent) -> Vec<ClientId> {
    let evtype = event.evtype();
    let bit = 1u32 << evtype;
    let master_devices = [
        XI2_MASTER_KEYBOARD_DEVICE_ID,
        XI2_ALL_MASTER_DEVICES,
        XI2_ALL_DEVICES,
    ];
    let grab = state.active_keyboard_grab;
    let mut dropped = Vec::new();

    // DeliverGrabbedEvent. A core grab gets nothing: EventToCore has no
    // core form of a raw event. For an XI2 grab, owner_events first tries
    // normal delivery (DeliverDeviceEvents) from the focus, or the sprite
    // window under PointerRoot, up to the focus; raw events can only be
    // selected on the root, so that reaches the owner only when the walk
    // gets to the root (focus PointerRoot or the root itself) and the owner
    // selected the event there. Failing that, the grab's own XI2 mask
    // decides (DeliverOneGrabbedEvent).
    if let Some(g) = grab
        && g.via_xi2
    {
        // core_focus.raw: 1 = PointerRoot.
        let focus_walk_reaches_root =
            state.core_focus.raw == 1 || state.core_focus.raw == ROOT_WINDOW.0;
        let natural = g.owner_events
            && focus_walk_reaches_root
            && state.clients.get(&g.owner.0).is_some_and(|c| {
                xi2_mask_for_client(c, ROOT_WINDOW, ROOT_WINDOW, &master_devices) & bit != 0
            });
        if natural || g.xi2_mask & bit != 0 {
            merge_dropped(
                &mut dropped,
                send_raw_key(state, &[g.owner], event, XI2_MASTER_KEYBOARD_DEVICE_ID),
            );
        }
    }

    // FilterRawEvents: with the device grabbed, an XI 2.0 client gets no
    // raw event, and the grab owner is skipped when the grab window is the
    // root — "we've already delivered", although a core grab or a grab
    // mask without the raw type delivered nothing (Xorg does this too).
    let targets: Vec<ClientId> = raw_key_root_selectors(state, &master_devices, evtype)
        .into_iter()
        .filter(|cid| {
            let Some(g) = grab else {
                return true;
            };
            if state.xi2_client_versions.get(cid) == Some(&(2, 0)) {
                return false;
            }
            !(g.grab_window == ROOT_WINDOW && g.owner == *cid)
        })
        .collect();
    merge_dropped(
        &mut dropped,
        send_raw_key(state, &targets, event, XI2_MASTER_KEYBOARD_DEVICE_ID),
    );
    dropped
}

/// Clients that selected `evtype` on the root window under any of
/// `devices` (Xorg `GetClientsForDelivery` on the root).
fn raw_key_root_selectors(state: &ServerState, devices: &[u16], evtype: u16) -> Vec<ClientId> {
    state
        .clients
        .iter()
        .filter(|(_, c)| {
            xi2_mask_for_client(c, ROOT_WINDOW, ROOT_WINDOW, devices) & (1 << evtype) != 0
        })
        .map(|(id, _)| ClientId(*id))
        .collect()
}

/// Write one raw key event with the given `deviceid` to each target. The
/// source is always the slave keyboard, and keys carry no valuators — the
/// valuator mask is two zero words (Xorg `eventToRawEvent`).
fn send_raw_key(
    state: &mut ServerState,
    targets: &[ClientId],
    event: RawKeyEvent,
    deviceid: u16,
) -> Vec<ClientId> {
    if targets.is_empty() {
        return Vec::new();
    }
    fanout_event_to_clients(state, targets, |buf, seq, order| {
        x11::encode_xi2_raw_event(
            buf,
            order,
            seq,
            XI2_MAJOR_OPCODE,
            event.evtype(),
            deviceid,
            event.time,
            u32::from(event.keycode),
            XI2_SLAVE_KEYBOARD_DEVICE_ID,
            0,
            0,
        );
    })
}

fn merge_dropped(into: &mut Vec<ClientId>, more: Vec<ClientId>) {
    for cid in more {
        if !into.contains(&cid) {
            into.push(cid);
        }
    }
}

/// Where a key event should go.
enum KeyRoute {
    /// No focus and no grab — drop the event.
    Drop,
    /// A passive key grab is active — deliver only to the grab owner.
    /// `freeze` is set for a synchronous grab on the activating press,
    /// signalling that the event must be held for possible replay.
    /// `via_xi2` carries the grab's protocol so delivery can match it
    /// (XI2 key events to a core GrabKey owner NULL-deref libXi in
    /// clients that linked it without XIQueryVersion).
    PassiveGrabOwner {
        owner: ClientId,
        grab_window: ResourceId,
        freeze: bool,
        /// X11 `owner_events` — when true, try natural delivery to
        /// the grab owner first (Xorg DeliverGrabbedEvent).
        owner_events: bool,
        via_xi2: bool,
    },
    /// Normal delivery to a window's subscribers (focus window, or an
    /// explicit-grab window).
    Window(ResourceId),
}

/// Apply X11 keyboard routing rules. May activate a passive grab or
/// auto-release one on the matching key release.
fn key_route(state: &mut ServerState, event: &HostKeyEvent) -> KeyRoute {
    // Active grab in effect.
    if let Some(g) = state.active_keyboard_grab {
        let passive = matches!(g.source, ActiveKeyboardGrabSource::PassiveKey { .. });
        // Auto-release a passive-key grab on the matching key-release
        // (the release still goes to the grab owner below).
        if !event.pressed
            && let ActiveKeyboardGrabSource::PassiveKey { keycode: kc } = g.source
            && kc == event.keycode
        {
            state.active_keyboard_grab = None;
            if let Some(freeze) = state
                .xi1_frozen
                .get_mut(&crate::xinput::DEVICEID_SLAVE_KEYBOARD)
            {
                freeze.stored = None;
            }
            // Release the XI1-side holds the activation placed.
            crate::core_loop::pointer_fanout::xi1_core_grab_bridge_release(
                state,
                crate::xinput::DEVICEID_SLAVE_KEYBOARD,
                g.owner,
            );
            // Xorg DeactivateKeyboardGrab: DoFocusEvents(grab_window
            // → focus, NotifyUngrab).
            crate::core_loop::process_request::emit_core_focus_transition(
                state,
                g.grab_window.0,
                state.core_focus.raw,
                2,
            );
        }
        if passive {
            return KeyRoute::PassiveGrabOwner {
                owner: g.owner,
                grab_window: g.grab_window,
                freeze: false,
                owner_events: g.owner_events,
                via_xi2: g.via_xi2,
            };
        }
        // Explicit grab (GrabKeyboard): key events go to the grabbing
        // client UNCONDITIONALLY, reported against the grab window —
        // Xorg DeliverGrabbedEvent; XGrabKeyboard implies KeyPress/
        // KeyRelease selection regardless of the owner's event masks
        // (xterm secure-keyboard, XTS AllowDeviceEvents iskfrozen
        // probes). Window delivery here silently dropped keys when
        // the grabber had no KeyPressMask on the grab window.
        return KeyRoute::PassiveGrabOwner {
            owner: g.owner,
            grab_window: g.grab_window,
            freeze: false,
            owner_events: g.owner_events,
            via_xi2: g.via_xi2,
        };
    }

    let focus = current_focus(state);

    // Press: try to match a passive key grab, activating it. With
    // focus None the grab walk still runs from the root — WM hotkey
    // grabs on the root fire regardless of focus.
    let grab_walk_start = if focus == ResourceId(0) {
        ROOT_WINDOW
    } else {
        focus
    };
    if event.pressed
        && let Some(grab) = state
            .find_key_grab(grab_walk_start, event.keycode, event.state)
            .cloned()
    {
        let crate::server::KeyGrab {
            owner,
            grab_window,
            pointer_mode,
            keyboard_mode,
            owner_events,
            via_xi2,
            xi2_mask,
            ..
        } = grab;
        state.active_keyboard_grab = Some(ActiveKeyboardGrab {
            owner,
            grab_window,
            source: ActiveKeyboardGrabSource::PassiveKey {
                keycode: event.keycode,
            },
            owner_events,
            via_xi2,
            xi2_mask,
        });
        // Xorg ActivateKeyboardGrab: DoFocusEvents(focus →
        // grab_window, NotifyGrab).
        crate::core_loop::process_request::emit_core_focus_transition(
            state,
            state.core_focus.raw,
            grab_window.0,
            1,
        );
        // Core↔XI bridge (Xorg ActivateKeyboardGrab →
        // CheckGrabForSyncs): a sync keyboard_mode freezes the
        // keyboard device's XI1 stream; a sync pointer_mode holds the
        // pointer device on this grab's behalf (XTS
        // XAllowDeviceEvents-10 freezes the pointer "twice" exactly
        // this way).
        crate::core_loop::pointer_fanout::xi1_check_grab_for_syncs(
            state,
            crate::xinput::DEVICEID_SLAVE_KEYBOARD,
            owner,
            keyboard_mode == 0,
            pointer_mode == 0,
        );
        return KeyRoute::PassiveGrabOwner {
            owner,
            grab_window,
            // keyboard_mode 0 == Synchronous → freeze for replay.
            freeze: keyboard_mode == 0,
            owner_events,
            via_xi2,
        };
    }

    // Focus None: keys are discarded (only grabs see them) — Xorg
    // DeliverFocusedEvent with focus->win == NoneWin.
    if focus == ResourceId(0) {
        return KeyRoute::Drop;
    }
    KeyRoute::Window(focus)
}

/// Resolve the current keyboard focus to a delivery window.
///
/// Reads the global `state.core_focus` (the Xorg `FocusClassRec`
/// model): None → `ResourceId(0)` (keys are discarded, grabs only);
/// PointerRoot → the deepest window under the pointer; a window xid →
/// that window.
pub(crate) fn current_focus(state: &ServerState) -> ResourceId {
    match state.core_focus.raw {
        0 => ResourceId(0),
        1 => deepest_window_at_pointer(state),
        w => ResourceId(w),
    }
}

/// XI1 DeviceKeyPress/Release delivery for the slave keyboard —
/// independent of the core key route. The natural target and the
/// selection-walk gating come from the device's own focus
/// (`xi1_focus::key_delivery_route`, the XI1 port of Xorg
/// `DeliverFocusedEvent`); grab/freeze handling inside
/// `xi1_route_device_event` is unaffected by the focus.
fn deliver_xi1_focused_key(state: &mut ServerState, event: &HostKeyEvent) -> Vec<ClientId> {
    let xi1_offset = if event.pressed {
        crate::xinput::XI_DEVICE_KEY_PRESS_OFFSET
    } else {
        crate::xinput::XI_DEVICE_KEY_RELEASE_OFFSET
    };
    let evcode = crate::server::XI_FIRST_EVENT + xi1_offset;
    let (natural, focus_route) = crate::core_loop::xi1_focus::key_delivery_route(
        state,
        crate::xinput::DEVICEID_SLAVE_KEYBOARD,
    );
    crate::core_loop::pointer_fanout::xi1_route_device_event(
        state,
        crate::server::Xi1QueuedEvent {
            deviceid: crate::xinput::DEVICEID_SLAVE_KEYBOARD,
            evcode,
            detail: event.keycode,
            time: event.time,
            root_x: event.root_x,
            root_y: event.root_y,
            event_x: event.event_x,
            event_y: event.event_y,
            state_mask: event.state,
            natural_target: natural,
            focus_route,
            axes: None,
            replay_floor: None,
        },
        true,
    )
}

/// Pointer-walk leg of Xorg `DeliverFocusedEvent`: if the pointer
/// window P is the focus or a descendant of it, the event window is
/// the first window from P upward (bounded at the focus, inclusive)
/// where any client selected the event; otherwise the focus itself.
fn focused_walk_target(state: &ServerState, focus: ResourceId, event: &HostKeyEvent) -> ResourceId {
    let mask_bit = if event.pressed {
        KEY_PRESS_MASK
    } else {
        KEY_RELEASE_MASK
    };
    let p = deepest_window_at_pointer(state);
    let mut chain = vec![p];
    let mut cur = p;
    for _ in 0..256 {
        let Some(w) = state.resources.window(cur) else {
            break;
        };
        if w.parent == cur {
            break;
        }
        chain.push(w.parent);
        cur = w.parent;
    }
    if !chain.contains(&focus) {
        return focus;
    }
    for w in chain {
        if !crate::core_loop::fanout::subscribers_by_id(state, w, mask_bit).is_empty() {
            return w;
        }
        if w == focus {
            break;
        }
    }
    focus
}

/// Natural-delivery probe for an `owner_events` keyboard grab — the
/// keyboard analogue of `pointer_fanout::grabbed_natural_target`
/// (Xorg `DeliverDeviceEvents` with the grab as client filter): walk
/// from the pointer window (if inside the focus subtree) or the focus
/// up; at the FIRST window with any subscriber, deliver there if the
/// grab owner is among them, else abort (grab-window fallback).
fn key_grabbed_natural_target(
    state: &ServerState,
    event: &HostKeyEvent,
    owner: ClientId,
) -> Option<ResourceId> {
    let mask_bit = if event.pressed {
        KEY_PRESS_MASK
    } else {
        KEY_RELEASE_MASK
    };
    let focus = current_focus(state);
    if focus == ResourceId(0) {
        return None;
    }
    // Start at P when it sits inside the focus subtree, else at focus.
    let p = deepest_window_at_pointer(state);
    let mut chain = vec![p];
    let mut cur = p;
    for _ in 0..256 {
        let Some(w) = state.resources.window(cur) else {
            break;
        };
        if w.parent == cur {
            break;
        }
        chain.push(w.parent);
        cur = w.parent;
    }
    let start = if chain.contains(&focus) { p } else { focus };
    let mut cur = start;
    for _ in 0..256 {
        let subs = crate::core_loop::fanout::subscribers_by_id(state, cur, mask_bit);
        if !subs.is_empty() {
            return subs.contains(&owner).then_some(cur);
        }
        let w = state.resources.window(cur)?;
        if w.parent == cur {
            return None;
        }
        cur = w.parent;
    }
    None
}

/// Deepest mapped window containing the cached pointer position —
/// the PointerRoot key-delivery target. Descends `direct_child_at`
/// from the root using `state.pointer_root`.
pub(crate) fn deepest_window_at_pointer(state: &ServerState) -> ResourceId {
    let (root_x, root_y) = state.pointer_root;
    let mut window = ROOT_WINDOW;
    loop {
        let (ox, oy) = state.resources.window_absolute_position(window);
        let wx = i16::try_from(i32::from(root_x).saturating_sub(ox)).unwrap_or(i16::MAX);
        let wy = i16::try_from(i32::from(root_y).saturating_sub(oy)).unwrap_or(i16::MAX);
        match state.direct_child_at(window, wx, wy) {
            Some(child) if child != window => window = child,
            _ => return window,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::{
        ActiveKeyboardGrab, ActiveKeyboardGrabSource, KeyGrab, ScreenSaverActive, ServerState,
    };
    use yserver_protocol::x11::ClientId;

    use crate::server::ClientState;
    use std::{
        collections::{HashMap, HashSet, VecDeque},
        io::Read,
        os::unix::net::UnixStream,
        sync::{Arc, Mutex, atomic::AtomicU16},
    };
    use yserver_protocol::x11::ClientByteOrder;

    // Duplicated from process_request.rs::tests. If you change one,
    // change both. A shared test_fixtures module is the right home
    // long-term; tracked as a follow-up.
    fn install_client(state: &mut ServerState, id: u32) -> UnixStream {
        use crate::resources::ROOT_WINDOW;
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

    fn key_event(pressed: bool, keycode: u8) -> HostKeyEvent {
        HostKeyEvent {
            pressed,
            keycode,
            time: 1,
            root_x: 10,
            root_y: 20,
            event_x: 10,
            event_y: 20,
            state: 0,
        }
    }

    /// Install a client whose core+XI2 selection on `window` is
    /// `core_mask` / `xi2_mask`. Returns the peer socket for reading
    /// what the server delivered.
    fn install_kf(
        state: &mut ServerState,
        id: u32,
        window: ResourceId,
        core_mask: u32,
        xi2_mask: u32,
    ) -> UnixStream {
        let (server_side, peer) = UnixStream::pair().unwrap();
        let client = ClientState {
            writer: Arc::new(Mutex::new(crate::transport::Transport::Unix(server_side))),
            byte_order: ClientByteOrder::LittleEndian,
            last_sequence: Arc::new(AtomicU16::new(0)),
            resource_id_base: 0,
            resource_id_mask: 0,
            event_masks: HashMap::from([(window, core_mask)]),
            save_set: HashSet::new(),
            big_requests_enabled: false,
            xi2_masks: HashMap::from([((window, 3u16), xi2_mask)]),
            xi1_event_classes: HashSet::new(),
            xi1_window_event_classes: HashMap::new(),
            outbound: VecDeque::new(),
            watching_writable: false,
            focused_window: ROOT_WINDOW,
            reader_control: None,
            is_local: true,
            fd_passing: true,
        };
        state.clients.insert(id, client);
        peer
    }

    fn received_bytes(peer: &mut UnixStream) -> usize {
        peer.set_nonblocking(true).unwrap();
        let mut buf = [0u8; 512];
        peer.read(&mut buf).unwrap_or(0)
    }

    /// A synchronous passive key grab routes the activating press to
    /// the grab *owner* (even though the owner has no per-window key
    /// selection, only the grab), and freezes the event for replay.
    /// This is the dead-`p`-in-wezterm fix: previously the press was
    /// delivered via window selection on the grab window, so a grab
    /// Regression: an unmodified keypress (cooked `state == 0`) must
    /// be delivered with `state == 0` — the fanout must NOT OR in any
    /// server-tracked modifier state. Pre-fix, a stale modifier
    /// tracker (`core_mod_state`, drifted from xkb by a release that
    /// bypassed the cook path on VT-switch) clobbered every plain key
    /// with ControlMask → "stuck Ctrl, can't type in wezterm".
    #[test]
    fn unmodified_key_delivered_with_clean_state() {
        const WIN: u32 = 0x0020_0001;
        let mut state = ServerState::new();
        let mut peer = install_kf(&mut state, 9, ResourceId(WIN), KEY_PRESS_MASK, 0);
        let mut backend = crate::backend::recording::RecordingBackend::default();
        state.core_focus.raw = WIN;
        // A modifier-map row mapping keycode 37 → Control, plus 37
        // held: a re-introduced "reconstruct modifier state from
        // keys_down × modmap and stamp it" path would compute
        // ControlMask and clobber the unmodified 'a' below. The
        // contract is that the fanout trusts the cooked `event.state`
        // (here 0) and stamps nothing.
        state.modifier_mapping_override = Some((1, vec![0, 0, 37, 0, 0, 0, 0, 0]));
        let _ = key_event_fanout_to_state(&mut state, &mut backend, key_event(true, 37));
        let _ = received_bytes(&mut peer);

        let _ = key_event_fanout_to_state(&mut state, &mut backend, key_event(true, 38));

        peer.set_nonblocking(true).unwrap();
        let mut buf = [0u8; 64];
        let n = peer.read(&mut buf).unwrap_or(0);
        assert!(n >= 32, "expected a KeyPress event, got {n} bytes");
        assert_eq!(buf[0] & 0x7f, 2, "must be a KeyPress");
        let delivered_state = u16::from_le_bytes([buf[28], buf[29]]);
        assert_eq!(
            delivered_state, 0,
            "unmodified keypress must carry state 0, not the stale tracker's ControlMask",
        );
    }

    /// owner that registered via XIPassiveGrabDevice received nothing.
    #[test]
    fn sync_passive_key_grab_delivers_to_owner_and_freezes() {
        let mut state = ServerState::new();
        // Grab owner selects NOTHING (mask 0) — only the grab matters.
        let mut owner = install_kf(&mut state, 7, ROOT_WINDOW, 0, 0);
        let mut backend = crate::backend::recording::RecordingBackend::default();
        state.key_grabs.push(KeyGrab {
            owner: ClientId(7),
            grab_window: ROOT_WINDOW,
            keycode: 33,
            modifiers: 0,
            owner_events: false,
            pointer_mode: 1,
            keyboard_mode: 0, // synchronous → freeze
            via_xi2: true,
            xi2_mask: 0,
        });

        let dropped = key_event_fanout_to_state(&mut state, &mut backend, key_event(true, 33));
        assert!(dropped.is_empty());
        assert!(
            matches!(
                state.active_keyboard_grab,
                Some(ActiveKeyboardGrab {
                    owner: ClientId(7),
                    source: ActiveKeyboardGrabSource::PassiveKey { keycode: 33 },
                    ..
                })
            ),
            "passive grab must activate, owned by client 7"
        );
        assert!(
            state
                .xi1_frozen
                .get(&crate::xinput::DEVICEID_SLAVE_KEYBOARD)
                .and_then(|f| f.stored.as_ref())
                .is_some(),
            "synchronous grab must freeze the press for replay"
        );
        assert!(
            received_bytes(&mut owner) > 0,
            "grab owner must receive the key press despite no window selection"
        );
    }

    /// `replay_frozen_key_to_focus` re-delivers the held key to the
    /// focused window's subscribers, bypassing the grab — the path
    /// AllowEvents(ReplayKeyboard) drives so the focused app (wezterm)
    /// finally sees the key the WM declined.
    #[test]
    fn replay_frozen_key_reaches_focus_window() {
        const FOCUS_WIN: u32 = 0x0020_0007;
        let mut state = ServerState::new();
        // Focused client selects core KeyPress on its window.
        let mut focus_peer = install_kf(&mut state, 9, ResourceId(FOCUS_WIN), KEY_PRESS_MASK, 0);
        state.clients.get_mut(&9).unwrap().focused_window = ResourceId(FOCUS_WIN);
        state.core_focus.raw = FOCUS_WIN;

        let _ = replay_frozen_key_to_focus(&mut state, key_event(true, 33));
        assert!(
            received_bytes(&mut focus_peer) > 0,
            "replayed key must reach the focused window's subscriber"
        );
    }

    /// Asynchronous passive key grab (keyboard_mode=1) does NOT freeze:
    /// the owner gets the press but there's nothing to replay.
    #[test]
    fn async_passive_key_grab_does_not_freeze() {
        let mut state = ServerState::new();
        let _owner = install_kf(&mut state, 7, ROOT_WINDOW, 0, 0);
        let mut backend = crate::backend::recording::RecordingBackend::default();
        state.key_grabs.push(KeyGrab {
            owner: ClientId(7),
            grab_window: ROOT_WINDOW,
            keycode: 33,
            modifiers: 0,
            owner_events: false,
            pointer_mode: 1,
            keyboard_mode: 1, // asynchronous → no freeze
            via_xi2: true,
            xi2_mask: 0,
        });
        let _ = key_event_fanout_to_state(&mut state, &mut backend, key_event(true, 33));
        assert!(state.active_keyboard_grab.is_some());
        assert!(
            state
                .xi1_frozen
                .get(&crate::xinput::DEVICEID_SLAVE_KEYBOARD)
                .and_then(|f| f.stored.as_ref())
                .is_none(),
            "async grab must not freeze"
        );
    }

    /// GH #59: a key event that changes the effective modifier state must
    /// emit a full XkbStateNotify (xkbType=2) carrying the real mods to
    /// clients that selected XKB StateNotify. libxkbcommon-x11 clients
    /// (kitty/GLFW) sync their modifier state from these events, not from
    /// key events — so the *clear* on modifier release is what lets a
    /// client that seeded a held modifier from XkbGetState recover.
    #[test]
    fn modifier_change_emits_xkb_state_notify_with_real_mods() {
        let mut state = ServerState::new();
        let mut backend = crate::backend::recording::RecordingBackend::new();
        // Client selected XKB StateNotify (bit 0x0004) on the core keyboard.
        let mut peer = install_kf(&mut state, 5, ROOT_WINDOW, 0, 0);
        crate::core_loop::xkb_select::xkb_select_events(&mut state, 5, 0, 0x0004);

        // Super held → effective Mod4 (0x40). Announced on the next key.
        backend.xkb_mods = (0x40, 0x40, 0, 0);
        let _ = key_event_fanout_to_state(&mut state, &mut backend, key_event(true, 133));
        let bytes = read_all_available(&mut peer);
        assert!(
            bytes.len() >= 32,
            "expected an XkbStateNotify, got {}",
            bytes.len()
        );
        assert_eq!(bytes[1], 2, "xkbType = XkbStateNotify");
        assert_eq!(bytes[9], 0x40, "mods @9 = effective Mod4");
        assert_eq!(state.last_xkb_mods, 0x40);

        // Modifier released → effective mods 0. The CLEAR must be announced
        // (this is what unsticks a seeded modifier in kitty).
        backend.xkb_mods = (0, 0, 0, 0);
        let _ = key_event_fanout_to_state(&mut state, &mut backend, key_event(false, 133));
        let bytes2 = read_all_available(&mut peer);
        assert!(
            bytes2.len() >= 32,
            "expected XkbStateNotify(mods=0), got {}",
            bytes2.len()
        );
        assert_eq!(bytes2[1], 2, "xkbType = XkbStateNotify");
        assert_eq!(bytes2[9], 0x00, "mods @9 cleared to 0 on modifier release");
        assert_eq!(state.last_xkb_mods, 0);
    }

    #[test]
    fn dropped_when_focus_is_root_and_no_grabs() {
        // No clients = no focus = nothing to fan out. Just verify the
        // helper returns an empty drop list cleanly.
        let mut state = ServerState::new();
        let mut backend = crate::backend::recording::RecordingBackend::default();
        let dropped = key_event_fanout_to_state(&mut state, &mut backend, key_event(true, 38));
        assert!(dropped.is_empty());
    }

    #[test]
    fn passive_key_grab_activates_on_press_and_clears_on_release() {
        let mut state = ServerState::new();
        // No focus is set on any client — find_key_grab walks up from
        // focus. Setting the grab on ROOT exercises the matching path.
        let grab_owner = ClientId(7);
        let mut backend = crate::backend::recording::RecordingBackend::default();
        state.key_grabs.push(KeyGrab {
            owner: grab_owner,
            grab_window: ROOT_WINDOW,
            keycode: 38,
            modifiers: 0,
            owner_events: false,
            pointer_mode: 1,
            keyboard_mode: 1,
            via_xi2: false,
            xi2_mask: 0,
        });
        // Press: activates passive grab.
        let _ = key_event_fanout_to_state(&mut state, &mut backend, key_event(true, 38));
        match state.active_keyboard_grab {
            Some(ActiveKeyboardGrab {
                owner,
                grab_window,
                source: ActiveKeyboardGrabSource::PassiveKey { keycode },
                ..
            }) => {
                assert_eq!(owner, grab_owner);
                assert_eq!(grab_window, ROOT_WINDOW);
                assert_eq!(keycode, 38);
            }
            other => panic!("expected PassiveKey grab, got {other:?}"),
        }
        // Release with matching keycode clears the grab.
        let _ = key_event_fanout_to_state(&mut state, &mut backend, key_event(false, 38));
        assert!(state.active_keyboard_grab.is_none());
    }

    #[test]
    fn explicit_grab_persists_across_release() {
        let mut state = ServerState::new();
        let mut backend = crate::backend::recording::RecordingBackend::default();
        state.active_keyboard_grab = Some(ActiveKeyboardGrab {
            owner: ClientId(3),
            grab_window: ResourceId(0x100),
            source: ActiveKeyboardGrabSource::Explicit,
            owner_events: false,
            via_xi2: false,
            xi2_mask: 0,
        });
        let _ = key_event_fanout_to_state(&mut state, &mut backend, key_event(false, 38));
        // Explicit grab is NOT cleared by a key release (only passive
        // grabs auto-clear). Persists until UngrabKeyboard.
        assert!(matches!(
            state.active_keyboard_grab,
            Some(ActiveKeyboardGrab {
                source: ActiveKeyboardGrabSource::Explicit,
                ..
            })
        ));
    }

    #[test]
    fn key_event_resets_dpms_last_activity() {
        use std::time::{Duration, Instant};
        let mut state = ServerState::new();
        state.dpms.kms_capable = true;
        state.dpms.enabled = true;
        state.dpms.last_activity = Instant::now() - Duration::from_secs(10);
        let stale = state.dpms.last_activity;
        let mut backend = crate::backend::recording::RecordingBackend::default();

        let _ = key_event_fanout_to_state(&mut state, &mut backend, key_event(true, 33));

        let elapsed = state.dpms.last_activity.duration_since(stale);
        assert!(
            elapsed > Duration::from_secs(9),
            "last_activity should be ≈now, not stale"
        );
    }

    #[test]
    fn key_event_during_off_wakes_via_set_dpms_power_on() {
        let mut state = ServerState::new();
        state.dpms.kms_capable = true;
        state.dpms.enabled = true;
        state.dpms.power_level = 3; // Off
        let mut backend = crate::backend::recording::RecordingBackend::default();

        let _ = key_event_fanout_to_state(&mut state, &mut backend, key_event(true, 33));

        let calls = backend.calls.lock().unwrap().clone();
        assert!(
            calls
                .iter()
                .any(|c| matches!(c, crate::backend::recording::RecordedCall::SetDpmsPower(0))),
            "wake must call set_dpms_power(0); got {calls:?}"
        );
        assert_eq!(
            state.dpms.power_level, 0,
            "in-memory level should be On after wake"
        );
    }

    #[test]
    fn key_event_during_off_with_backend_error_still_advances_state() {
        let mut state = ServerState::new();
        state.dpms.kms_capable = true;
        state.dpms.enabled = true;
        state.dpms.power_level = 3;
        let mut backend = crate::backend::recording::RecordingBackend::default();
        backend.dpms_set_returns_err = true;

        let _ = key_event_fanout_to_state(&mut state, &mut backend, key_event(true, 33));

        assert_eq!(
            state.dpms.power_level, 0,
            "state must advance on backend error"
        );
    }

    #[test]
    fn key_event_during_screen_saver_on_flips_off_via_independent_path() {
        // Pre-state: DPMS On (so the existing DPMS-wake prologue
        // doesn't fire), SS On (activated standalone via idle timer
        // or ForceScreenSaver). Input must flip SS Off with forced=0.
        let mut state = ServerState::new();
        state.dpms.kms_capable = true;
        state.dpms.enabled = true;
        // dpms.power_level already 0 from new()
        state.screensaver.active = ScreenSaverActive::On;
        state.screensaver.selected_by.insert(ClientId(1), 0x01);
        let mut backend = crate::backend::recording::RecordingBackend::default();

        let _ = key_event_fanout_to_state(&mut state, &mut backend, key_event(true, 33));

        assert_eq!(state.screensaver.active, ScreenSaverActive::Off);
        assert!(!state.screensaver.forced, "input-driven Off is non-forced");
    }

    #[test]
    fn key_event_updates_global_and_per_device_vck_last_activity() {
        use std::time::Duration;
        let mut state = ServerState::new();
        state.dpms.last_activity = std::time::Instant::now() - Duration::from_secs(30);
        let stale = state.dpms.last_activity;
        let mut backend = crate::backend::recording::RecordingBackend::default();

        let _ = key_event_fanout_to_state(&mut state, &mut backend, key_event(true, 33));

        assert!(
            state.dpms.last_activity > stale,
            "global last_activity advanced"
        );
        let vck = state
            .per_device_last_activity
            .get(&3)
            .copied()
            .expect("VCK per-device entry inserted");
        assert!(vck > stale, "VCK per-device last_activity advanced");
    }

    #[test]
    fn key_event_fires_neg_transition_alarm_when_prior_idle_crosses_threshold() {
        use std::time::Duration;
        use yserver_protocol::x11::sync as x11sync;
        let mut state = ServerState::new();
        // User idle for 90s, NegativeTransition alarm at 60s.
        state.dpms.last_activity = std::time::Instant::now() - Duration::from_secs(90);
        state
            .per_device_last_activity
            .insert(3, std::time::Instant::now() - Duration::from_secs(90));
        let alarm_id = 0x2000;
        state.sync_alarms.insert(
            alarm_id,
            crate::server::SyncAlarm {
                owner: ClientId(1),
                counter: x11sync::IDLETIME_COUNTER,
                wait_value: 60_000,
                delta: 0,
                test_type: x11sync::TEST_NEGATIVE_TRANSITION as u8,
                events: false,
                state: x11sync::ALARM_STATE_ACTIVE,
            },
        );
        let mut backend = crate::backend::recording::RecordingBackend::default();

        let _ = key_event_fanout_to_state(&mut state, &mut backend, key_event(true, 33));

        // Alarm stays Active (Transition + delta=0 — Task 2 fix); cache reflects post-wake idle=0.
        assert_eq!(
            state.sync_alarms[&alarm_id].state,
            x11sync::ALARM_STATE_ACTIVE
        );
        assert_eq!(
            state
                .idletime_last_evaluated
                .get(&x11sync::IDLETIME_COUNTER)
                .copied(),
            Some(0),
            "post-wake last_evaluated should be 0"
        );
    }

    #[test]
    fn key_event_fires_neg_transition_alarm_on_per_device_idletime_vck() {
        // Regression for the per-device fallback bug: a NegativeTransition
        // alarm on IDLETIME_DEVICE_VCK must fire on the very first input
        // even if `per_device_last_activity[3]` has no entry yet.
        // Without the fallback-to-global fix in the prologue, the computed
        // prior_device would be 0 and the trigger `old > wait && new <= wait`
        // would not hold — no AlarmNotify would reach the wire.
        //
        // PRIMARY assertion is AlarmNotify (type=84) on the client's
        // outbound stream; cache + state are secondary checks.
        use std::time::Duration;
        use yserver_protocol::x11::sync as x11sync;
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, 1);
        state.dpms.last_activity = std::time::Instant::now() - Duration::from_secs(90);
        assert!(
            !state.per_device_last_activity.contains_key(&3),
            "test precondition: no per-device entry"
        );

        let alarm_id = 0x3000;
        state.sync_alarms.insert(
            alarm_id,
            crate::server::SyncAlarm {
                owner: ClientId(1),
                counter: x11sync::IDLETIME_DEVICE_VCK,
                wait_value: 60_000,
                delta: 0,
                test_type: x11sync::TEST_NEGATIVE_TRANSITION as u8,
                events: true, // load-bearing
                state: x11sync::ALARM_STATE_ACTIVE,
            },
        );
        let mut backend = crate::backend::recording::RecordingBackend::default();

        let _ = key_event_fanout_to_state(&mut state, &mut backend, key_event(true, 33));

        // PRIMARY: AlarmNotify event type 84.
        let bytes = read_all_available(&mut peer);
        // AlarmNotify is a 32-byte sequential event; type byte at offset 0.
        assert!(
            bytes.len() >= 32,
            "expected AlarmNotify event (32B); got {} bytes",
            bytes.len()
        );
        assert_eq!(
            bytes[0], 84,
            "AlarmNotify event type (SYNC_FIRST_EVENT + 1)"
        );
        assert_eq!(bytes[1], 1, "AlarmNotify kind = AlarmNotify (1)");
        assert_eq!(
            state.sync_alarms[&alarm_id].state,
            x11sync::ALARM_STATE_ACTIVE
        );
        assert_eq!(
            state
                .idletime_last_evaluated
                .get(&x11sync::IDLETIME_DEVICE_VCK)
                .copied(),
            Some(0)
        );
    }

    /// XI2 raw key events (issue #173). Expected bytes and delivery sets
    /// are Xorg captures: Xvfb (xorg-server 21.1) driven by
    /// `tools/vng-scenarios/xi2-raw-keys-probe.c` over XTEST, several
    /// clients per run so every recipient's stream is visible, e.g.
    ///   probe mon:m22:2:1 mon:a22:2:0 mon:m20:0:1 mon:a20:0:0 \
    ///         grabkbd:async p38 r38 ungrabkbd
    /// Xvfb device ids match yserver's for this path: 3 = master keyboard,
    /// 5 = the slave the key came from (Xvfb's XTEST keyboard; yserver's
    /// one slave keyboard).
    mod raw_keys {
        use super::*;
        use crate::host_x11::HostXidMap;

        const RAW_PRESS: u16 = 13;
        const RAW_RELEASE: u16 = 14;
        const RAW_KEY_MASK: u32 = (1 << 13) | (1 << 14);
        const CHILD: ResourceId = ResourceId(0x0040_0001);

        /// One XI_RawKeyPress/Release exactly as Xorg wrote it — a captured
        /// wire image (sequence and time blanked in the capture, filled in
        /// here; Xvfb's XI opcode 0x83 replaced by yserver's 137):
        ///   2383.... 02000000 0d000300 ........ 26000000 05000200
        ///   00000000 00000000 00000000 00000000
        /// i.e. 40 bytes, length 2, sourceid 5, valuators_len 2, flags 0,
        /// and an all-zero two-word valuator mask with no axis values.
        fn xorg_raw_key(evtype: u16, deviceid: u16, keycode: u8, time: u32) -> Vec<u8> {
            let t = time.to_le_bytes();
            vec![
                0x23,
                137,
                0x00,
                0x00,
                0x02,
                0x00,
                0x00,
                0x00, //
                evtype as u8,
                0x00,
                deviceid as u8,
                0x00,
                t[0],
                t[1],
                t[2],
                t[3], //
                keycode,
                0x00,
                0x00,
                0x00,
                0x05,
                0x00,
                0x02,
                0x00, //
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00, //
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
            ]
        }

        /// A client selecting `mask` on the root under each of `devices`,
        /// having announced `xi_version` (None: never called XIQueryVersion).
        fn install_root_selector(
            state: &mut ServerState,
            id: u32,
            devices: &[u16],
            mask: u32,
            xi_version: Option<(u16, u16)>,
        ) -> UnixStream {
            let peer = install_kf(state, id, ROOT_WINDOW, 0, 0);
            let client = state.clients.get_mut(&id).unwrap();
            client.xi2_masks.clear();
            for d in devices {
                client.xi2_masks.insert((ROOT_WINDOW, *d), mask);
            }
            if let Some(v) = xi_version {
                state.xi2_client_versions.insert(ClientId(id), v);
            }
            peer
        }

        /// Every XI2 raw event in what `peer` received, whole.
        fn raw_events(peer: &mut UnixStream) -> Vec<Vec<u8>> {
            let bytes = read_all_available(peer);
            let mut out = Vec::new();
            let mut i = 0;
            while i + 32 <= bytes.len() {
                let len = if bytes[i] & 0x7f == 35 {
                    32 + 4 * u32::from_le_bytes(bytes[i + 4..i + 8].try_into().unwrap()) as usize
                } else {
                    32
                };
                let ev = bytes[i..i + len].to_vec();
                if ev[0] == 35 && matches!(u16::from_le_bytes([ev[8], ev[9]]), 13 | 14) {
                    out.push(ev);
                }
                i += len;
            }
            out
        }

        fn raw(keycode: u8, pressed: bool, time: u32) -> RawKeyEvent {
            RawKeyEvent {
                keycode,
                pressed,
                time,
            }
        }

        fn press_release(state: &mut ServerState, keycode: u8) {
            let _ = raw_key_event_to_state(state, raw(keycode, true, 100), false, false);
            let _ = raw_key_event_to_state(state, raw(keycode, false, 101), true, false);
        }

        fn keyboard_grab(
            owner: u32,
            window: ResourceId,
            via_xi2: bool,
            owner_events: bool,
            xi2_mask: u32,
        ) -> ActiveKeyboardGrab {
            ActiveKeyboardGrab {
                owner: ClientId(owner),
                grab_window: window,
                source: ActiveKeyboardGrabSource::Explicit,
                owner_events,
                via_xi2,
                xi2_mask,
            }
        }

        /// No grab. Capture: XIAllMasterDevices and the master keyboard get
        /// the master form; XIAllDevices gets the slave form THEN the master
        /// form (slave processed first, mi/mieq.c); a slave selector gets
        /// the slave form; an XI 2.0 client is not filtered without a grab.
        #[test]
        fn raw_key_forms_per_selection_match_xorg() {
            let mut state = ServerState::new();
            let mut all_master =
                install_root_selector(&mut state, 1, &[1], RAW_KEY_MASK, Some((2, 2)));
            let mut all = install_root_selector(&mut state, 2, &[0], RAW_KEY_MASK, Some((2, 2)));
            let mut vck = install_root_selector(&mut state, 3, &[3], RAW_KEY_MASK, Some((2, 2)));
            let mut slave = install_root_selector(&mut state, 4, &[5], RAW_KEY_MASK, Some((2, 2)));
            let mut xi20 = install_root_selector(&mut state, 5, &[1], RAW_KEY_MASK, Some((2, 0)));
            // A client selecting only RawKeyRelease gets only releases.
            let mut release_only = install_root_selector(&mut state, 6, &[1], 1 << 14, None);

            press_release(&mut state, 38);

            let master = vec![
                xorg_raw_key(RAW_PRESS, 3, 38, 100),
                xorg_raw_key(RAW_RELEASE, 3, 38, 101),
            ];
            assert_eq!(raw_events(&mut all_master), master);
            assert_eq!(raw_events(&mut vck), master);
            assert_eq!(raw_events(&mut xi20), master);
            assert_eq!(
                raw_events(&mut all),
                vec![
                    xorg_raw_key(RAW_PRESS, 5, 38, 100),
                    xorg_raw_key(RAW_PRESS, 3, 38, 100),
                    xorg_raw_key(RAW_RELEASE, 5, 38, 101),
                    xorg_raw_key(RAW_RELEASE, 3, 38, 101),
                ]
            );
            assert_eq!(
                raw_events(&mut slave),
                vec![
                    xorg_raw_key(RAW_PRESS, 5, 38, 100),
                    xorg_raw_key(RAW_RELEASE, 5, 38, 101)
                ]
            );
            assert_eq!(
                raw_events(&mut release_only),
                vec![xorg_raw_key(RAW_RELEASE, 3, 38, 101)]
            );
        }

        /// Xorg GetKeyboardEvents: a press of a key already down yields a
        /// raw press for an auto-repeating non-modifier (capture: `p38 p38`
        /// → two raw presses) and nothing for a modifier (`p50 p50` → one)
        /// or with auto-repeat off (dix/getevents.c "Handle core repeating").
        /// A release of a key that is not down still yields a raw release
        /// (capture: a lone `r38` → RawKeyRelease).
        #[test]
        fn raw_key_press_while_down_follows_get_keyboard_events() {
            let mut state = ServerState::new();
            let mut peer = install_root_selector(&mut state, 1, &[1], RAW_KEY_MASK, None);

            let _ = raw_key_event_to_state(&mut state, raw(38, true, 7), true, false);
            assert_eq!(
                raw_events(&mut peer),
                vec![xorg_raw_key(RAW_PRESS, 3, 38, 7)]
            );

            let _ = raw_key_event_to_state(&mut state, raw(50, true, 8), true, true);
            assert!(
                raw_events(&mut peer).is_empty(),
                "modifier press while down: no raw event"
            );

            let _ = raw_key_event_to_state(&mut state, raw(38, false, 9), false, false);
            assert_eq!(
                raw_events(&mut peer),
                vec![xorg_raw_key(RAW_RELEASE, 3, 38, 9)]
            );

            state.keyboard_control.auto_repeats[38 >> 3] &= !(1 << (38 & 7));
            let _ = raw_key_event_to_state(&mut state, raw(38, true, 10), true, false);
            assert!(
                raw_events(&mut peer).is_empty(),
                "per-key auto-repeat off: no raw event"
            );
            let _ = raw_key_event_to_state(&mut state, raw(38, true, 11), false, false);
            assert_eq!(
                raw_events(&mut peer).len(),
                1,
                "a first press still yields one"
            );

            state.keyboard_control.auto_repeats[38 >> 3] |= 1 << (38 & 7);
            state.keyboard_control.global_auto_repeat = false;
            let _ = raw_key_event_to_state(&mut state, raw(38, true, 12), true, false);
            assert!(
                raw_events(&mut peer).is_empty(),
                "global auto-repeat off: no raw event"
            );
        }

        /// Core GrabKeyboard(root), async. Capture G1 + C1: the XI 2.0
        /// XIAllMasterDevices client loses the master form, the XI 2.0
        /// XIAllDevices client keeps only the slave form (the slave is not
        /// grabbed), XI 2.2 clients and a client that never queried the
        /// version are unaffected, and the grabbing client — although it
        /// selected raw keys on the root — gets nothing: a core grab has no
        /// raw form, and FilterRawEvents skips the owner of a root grab.
        #[test]
        fn raw_key_under_core_grab_matches_xorg() {
            let mut state = ServerState::new();
            let mut owner = install_root_selector(&mut state, 1, &[1], RAW_KEY_MASK, Some((2, 2)));
            let mut m22 = install_root_selector(&mut state, 2, &[1], RAW_KEY_MASK, Some((2, 2)));
            let mut a22 = install_root_selector(&mut state, 3, &[0], RAW_KEY_MASK, Some((2, 2)));
            let mut m20 = install_root_selector(&mut state, 4, &[1], RAW_KEY_MASK, Some((2, 0)));
            let mut a20 = install_root_selector(&mut state, 5, &[0], RAW_KEY_MASK, Some((2, 0)));
            let mut never = install_root_selector(&mut state, 6, &[1], RAW_KEY_MASK, None);
            state.active_keyboard_grab = Some(keyboard_grab(1, ROOT_WINDOW, false, false, 0));

            let _ = raw_key_event_to_state(&mut state, raw(38, true, 5), false, false);

            let master = vec![xorg_raw_key(RAW_PRESS, 3, 38, 5)];
            let slave = vec![xorg_raw_key(RAW_PRESS, 5, 38, 5)];
            assert!(raw_events(&mut owner).is_empty());
            assert_eq!(raw_events(&mut m22), master);
            assert_eq!(
                raw_events(&mut a22),
                [slave.clone(), master.clone()].concat()
            );
            assert!(raw_events(&mut m20).is_empty());
            assert_eq!(raw_events(&mut a20), slave);
            assert_eq!(raw_events(&mut never), master);
        }

        /// XIGrabDevice(master keyboard) async. Captures G2/G3/G3b/O1-O4:
        /// how many master-form raw presses the grabbing client receives,
        /// by grab window, owner_events, whether the grab mask selects raw
        /// keys, and whether the owner also selected raw keys on the root.
        /// Bystanders are unaffected (XI 2.2) in every case.
        #[test]
        fn raw_key_under_xi2_grab_matches_xorg() {
            // (grab window, owner_events, grab mask has raw, owner selects on root, owner copies)
            let cases = [
                (ROOT_WINDOW, false, true, true, 1), // G2: via grab; root copy skipped
                (ROOT_WINDOW, false, true, false, 1), // G3b: via grab only
                (CHILD, false, true, true, 2),       // G3: via grab + root copy
                (CHILD, true, false, true, 2),       // O1: owner_events + root copy
                (CHILD, false, false, true, 1),      // O2: root copy only
                (ROOT_WINDOW, true, false, true, 1), // O3: owner_events; root copy skipped
                (ROOT_WINDOW, false, false, true, 0), // O4: none at all
            ];
            for (i, (window, owner_events, mask_has_raw, owner_selects, expect)) in
                cases.into_iter().enumerate()
            {
                let mut state = ServerState::new();
                // Xvfb's default focus: PointerRoot.
                state.core_focus.raw = 1;
                let mut owner = install_root_selector(
                    &mut state,
                    1,
                    &[1],
                    if owner_selects { RAW_KEY_MASK } else { 0 },
                    Some((2, 2)),
                );
                let mut bystander =
                    install_root_selector(&mut state, 2, &[1], RAW_KEY_MASK, Some((2, 2)));
                let grab_mask = (1 << 2) | (1 << 3) | if mask_has_raw { RAW_KEY_MASK } else { 0 };
                state.active_keyboard_grab =
                    Some(keyboard_grab(1, window, true, owner_events, grab_mask));

                let _ = raw_key_event_to_state(&mut state, raw(38, true, 5), false, false);

                assert_eq!(
                    raw_events(&mut owner),
                    vec![xorg_raw_key(RAW_PRESS, 3, 38, 5); expect],
                    "case {i}"
                );
                assert_eq!(
                    raw_events(&mut bystander),
                    vec![xorg_raw_key(RAW_PRESS, 3, 38, 5)],
                    "case {i}"
                );
            }
        }

        /// owner_events delivery walks from the focus; raw keys are only
        /// selectable on the root, so with focus on a window it never gets
        /// there and the grab mask alone decides (dix/events.c
        /// DeliverGrabbedEvent → DeliverDeviceEvents(focus, .., stopAt=focus)).
        #[test]
        fn raw_key_owner_events_needs_focus_walk_to_reach_root() {
            let mut state = ServerState::new();
            state.core_focus.raw = CHILD.0;
            let mut owner = install_root_selector(&mut state, 1, &[1], RAW_KEY_MASK, Some((2, 2)));
            state.active_keyboard_grab = Some(keyboard_grab(1, ROOT_WINDOW, true, true, 1 << 2));

            let _ = raw_key_event_to_state(&mut state, raw(38, true, 5), false, false);

            assert!(raw_events(&mut owner).is_empty());
        }

        /// Core GrabKeyboard(root) with keyboard_mode Sync, then
        /// AllowEvents(AsyncKeyboard). Capture G4: while frozen only the
        /// slave form is delivered; the master forms are held in input
        /// order and delivered on thaw, filtered by the grab still in
        /// effect then (the XI 2.0 client gets none).
        #[test]
        fn raw_key_master_form_waits_for_thaw() {
            let mut state = ServerState::new();
            let mut backend = crate::backend::recording::RecordingBackend::default();
            let mut m22 = install_root_selector(&mut state, 2, &[1], RAW_KEY_MASK, Some((2, 2)));
            let mut a22 = install_root_selector(&mut state, 3, &[0], RAW_KEY_MASK, Some((2, 2)));
            let mut m20 = install_root_selector(&mut state, 4, &[1], RAW_KEY_MASK, Some((2, 0)));
            state.active_keyboard_grab = Some(keyboard_grab(1, ROOT_WINDOW, false, false, 0));
            state
                .xi1_frozen
                .entry(crate::xinput::DEVICEID_SLAVE_KEYBOARD)
                .or_default()
                .state = crate::server::Xi1SyncState::FrozenNoEvent;

            press_release(&mut state, 38);

            assert!(
                raw_events(&mut m22).is_empty(),
                "master form held while frozen"
            );
            assert!(raw_events(&mut m20).is_empty());
            assert_eq!(
                raw_events(&mut a22),
                vec![
                    xorg_raw_key(RAW_PRESS, 5, 38, 100),
                    xorg_raw_key(RAW_RELEASE, 5, 38, 101)
                ],
                "slave form is not frozen"
            );

            crate::core_loop::pointer_fanout::xi1_thaw_device(
                &mut state,
                &mut backend,
                &HostXidMap::new(),
                crate::xinput::DEVICEID_SLAVE_KEYBOARD,
            );

            let master = vec![
                xorg_raw_key(RAW_PRESS, 3, 38, 100),
                xorg_raw_key(RAW_RELEASE, 3, 38, 101),
            ];
            assert_eq!(raw_events(&mut m22), master);
            assert_eq!(raw_events(&mut a22), master);
            assert!(
                raw_events(&mut m20).is_empty(),
                "grab still active at thaw: XI 2.0 filtered"
            );
        }

        /// Capture G5 (passive sync GrabKey, AllowEvents(ReplayKeyboard)):
        /// the release queued behind the freeze is processed after the
        /// replay released the grab, so its master form also reaches the
        /// XI 2.0 client — the grab consulted is the one at processing time.
        #[test]
        fn raw_key_queued_master_form_sees_grab_at_processing_time() {
            let mut state = ServerState::new();
            let mut backend = crate::backend::recording::RecordingBackend::default();
            let mut m20 = install_root_selector(&mut state, 4, &[1], RAW_KEY_MASK, Some((2, 0)));
            state.active_keyboard_grab = Some(ActiveKeyboardGrab {
                source: ActiveKeyboardGrabSource::PassiveKey { keycode: 38 },
                ..keyboard_grab(1, ROOT_WINDOW, false, false, 0)
            });
            state
                .xi1_frozen
                .entry(crate::xinput::DEVICEID_SLAVE_KEYBOARD)
                .or_default()
                .state = crate::server::Xi1SyncState::FrozenWithEvent;
            let _ = raw_key_event_to_state(&mut state, raw(38, false, 101), true, false);
            assert!(raw_events(&mut m20).is_empty());

            state.active_keyboard_grab = None;
            crate::core_loop::pointer_fanout::xi1_thaw_device(
                &mut state,
                &mut backend,
                &HostXidMap::new(),
                crate::xinput::DEVICEID_SLAVE_KEYBOARD,
            );

            assert_eq!(
                raw_events(&mut m20),
                vec![xorg_raw_key(RAW_RELEASE, 3, 38, 101)]
            );
        }

        /// The activating press of a passive grab is replayed (ReplayKeyboard)
        /// without regenerating its raw event: raw describes the physical
        /// input and went out before the grab activated (capture G5: one
        /// RawKeyPress per client in total).
        #[test]
        fn replayed_key_does_not_repeat_raw_event() {
            let mut state = ServerState::new();
            let mut m22 = install_root_selector(&mut state, 2, &[1], RAW_KEY_MASK, Some((2, 2)));
            state.core_focus.raw = 1;

            let _ = replay_frozen_key_to_focus(&mut state, key_event(true, 38));

            assert!(raw_events(&mut m22).is_empty());
        }
    }
}
