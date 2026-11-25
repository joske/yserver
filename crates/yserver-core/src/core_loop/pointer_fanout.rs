//! State-borrowing replacement for `server::pointer_event_fanout`.
//!
//! Mirrors the pre-lift logic from `server.rs`:
//!   * translate root_x/root_y from host-screen coords to ynest-root,
//!   * honour any active or passive pointer grab,
//!   * walk the propagation chain for core device events,
//!   * route parallel XI2 device forms leaf-to-root and fan out raw events.
//!
//! All work happens inside a single `&mut ServerState` borrow scope —
//! per-target writers go through `client_io::write_or_buffer` so the
//! D3 lift can wire the disconnect list back into the core loop.
//!
//! The xid_map is still passed as `Arc<Mutex<HostXidMap>>`. Phase F1
//! demotes it to a plain field on `HostX11Backend` and at that point
//! the helper takes `&HostXidMap`.

#[cfg(test)]
mod tests;

mod buttons;
mod owner_events;
use buttons::{master_button_transition, pointer_button_transition};
pub(in crate::core_loop) use owner_events::xtest_wire_valuators;
use owner_events::{
    OwnerEventsRoute, OwnerProtocol, Xi1SelectionFilter, grab_for_form, owner_events_route,
    xi1_device_mask_on_window, xi1_event_mask_bit, xi1_selection_filter_for_slave_grab,
    xi1_window_selectors, xi1_window_selectors_with_grab_class, xi2_motion_targets_for_source,
};

use yserver_protocol::x11::{self, ClientId, ResourceId, SequenceNumber};

use crate::{
    core_loop::fanout::{
        client_target_id, fanout_event_to_clients, pointer_propagation_target_by_id,
    },
    host_x11::{HostPointerEvent, HostXidMap, PointerEventKind},
    resources::ROOT_WINDOW,
    server::{ServerState, xi2_mask_for_client},
};

const XI2_MAJOR_OPCODE: u8 = 137;
const XI2_MASTER_POINTER_DEVICE_ID: u16 = 2;
const XI2_XTEST_POINTER_DEVICE_ID: u16 = crate::xinput::DEVICEID_XTEST_POINTER;

impl ServerState {
    /// Seed cached pointer and master sprite positions from the backend's
    /// actual initial cursor location.
    pub fn seed_master_sprite_position(&mut self, x: i32, y: i32) {
        let position = (
            i16::try_from(x.clamp(i32::from(i16::MIN), i32::from(i16::MAX)))
                .expect("clamped root x fits i16"),
            i16::try_from(y.clamp(i32::from(i16::MIN), i32::from(i16::MAX)))
                .expect("clamped root y fits i16"),
        );
        self.pointer_root = position;
        self.master_sprite_root = position;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PointerReplay {
    pub device: u16,
    pub grab_window: ResourceId,
}

#[derive(Clone, Copy)]
struct PointerXiSource {
    slave_deviceid: Option<u16>,
    sourceid: u16,
    attached_master: Option<u16>,
}

#[derive(Clone, Copy)]
struct PointerButtonTransition {
    source_accepted: bool,
    master_accepted: bool,
    master_changed: bool,
}

/// Whether a pointer origin can still produce input. KMS calls this before
/// integrating relative motion into its cursor; fanout repeats the check so
/// queued and replayed events cannot outlive their source.
pub fn pointer_origin_is_live(state: &ServerState, origin: crate::core_loop::InputOrigin) -> bool {
    resolve_pointer_xi_source(state, origin, false).is_some()
}

pub fn floating_pointer_device_id(
    state: &ServerState,
    origin: crate::core_loop::InputOrigin,
) -> Option<u16> {
    let source = resolve_pointer_xi_source(state, origin, false)?;
    source
        .slave_deviceid
        .filter(|_| source.attached_master.is_none())
}

fn resolve_pointer_xi_source(
    state: &ServerState,
    origin: crate::core_loop::InputOrigin,
    tree_change: bool,
) -> Option<PointerXiSource> {
    use crate::{core_loop::InputOrigin, xinput::XiFacetKind};

    let source = match origin {
        InputOrigin::Physical(source_id) => {
            let info = state.xi_devices.source(source_id)?;
            if !info.enabled {
                return None;
            }
            match state.xi_devices.facet(source_id, XiFacetKind::PointerTouch) {
                Some(device_id) => {
                    let device = state.xi_devices.device(device_id)?;
                    if !device.enabled
                        || device
                            .attached_master
                            .is_some_and(|master| master != XI2_MASTER_POINTER_DEVICE_ID)
                    {
                        return None;
                    }
                    PointerXiSource {
                        slave_deviceid: Some(device_id),
                        sourceid: device_id,
                        attached_master: device.attached_master,
                    }
                }
                // Capacity exhaustion and keyboard-only sources still feed
                // the core/master pointer stream without claiming XTEST 4.
                None => PointerXiSource {
                    slave_deviceid: None,
                    sourceid: XI2_MASTER_POINTER_DEVICE_ID,
                    attached_master: Some(XI2_MASTER_POINTER_DEVICE_ID),
                },
            }
        }
        InputOrigin::XTest(device_id) => {
            let device = state.xi_devices.device(device_id)?;
            if !device.enabled {
                return None;
            }
            if device_id == XI2_MASTER_POINTER_DEVICE_ID {
                PointerXiSource {
                    slave_deviceid: None,
                    sourceid: XI2_MASTER_POINTER_DEVICE_ID,
                    attached_master: Some(XI2_MASTER_POINTER_DEVICE_ID),
                }
            } else if (device_id == XI2_XTEST_POINTER_DEVICE_ID
                || device.facet == Some(XiFacetKind::PointerTouch))
                && device
                    .attached_master
                    .is_none_or(|master| master == XI2_MASTER_POINTER_DEVICE_ID)
            {
                PointerXiSource {
                    slave_deviceid: Some(device_id),
                    sourceid: device_id,
                    attached_master: device.attached_master,
                }
            } else {
                return None;
            }
        }
        InputOrigin::NestedHost => PointerXiSource {
            slave_deviceid: None,
            sourceid: XI2_MASTER_POINTER_DEVICE_ID,
            attached_master: Some(XI2_MASTER_POINTER_DEVICE_ID),
        },
    };

    Some(if tree_change {
        // Xorg CheckMotion(NULL) is a master-generated event, even when the
        // pointer happened to be over a physical slave before the tree edit.
        PointerXiSource {
            slave_deviceid: None,
            sourceid: XI2_MASTER_POINTER_DEVICE_ID,
            attached_master: Some(XI2_MASTER_POINTER_DEVICE_ID),
        }
    } else {
        source
    })
}

/// Apply Xorg's `UpdateFromMaster` bookkeeping for an accepted pointer
/// event: a source change announces `SlaveSwitch`, and the master inherits
/// the current source valuators. Scroll Motion and scroll-stop call this same
/// helper so both event paths leave the same master baseline.
fn update_from_pointer_master(
    state: &mut ServerState,
    source: PointerXiSource,
    announce_switch: bool,
    copy_valuators: bool,
) -> Vec<ClientId> {
    let mut dropped = Vec::new();
    let (Some(source_id), Some(master_id)) = (source.slave_deviceid, source.attached_master) else {
        return dropped;
    };
    if announce_switch {
        merge_dropped(
            &mut dropped,
            crate::xinput::hotplug::announce_xi2_slave_switch(state, master_id, source_id),
        );
    }
    if copy_valuators
        && master_id == XI2_MASTER_POINTER_DEVICE_ID
        && let Some(device) = state.xi_devices.device(source_id)
    {
        state.scroll_axis_value = device.scroll_axis_values;
    }
    dropped
}

/// Whether a wheel button's direction has a ScrollClass on its generating
/// slave. Xorg maps buttons 4/5 to the vertical axis and 6/7 to the horizontal
/// axis (`dix/getevents.c:1661-1677`), then emulates only when that axis is
/// present (`:1652-1657,1684-1692`). XTEST is initialized by `CorePointerProc`
/// with two relative axes and no ScrollClass (`dix/devices.c:660-690`).
/// Physical libinput pointers install horizontal and vertical ScrollClasses
/// (`xf86-input-libinput/src/xf86libinput.c:1109-1117`). For a master copy,
/// `slave_deviceid` remains the generating slave, so its class shape controls
/// the conversion.
fn pointer_source_has_scroll_class(
    state: &ServerState,
    source: PointerXiSource,
    button: u8,
) -> bool {
    let axis = match button {
        4 | 5 => 2,
        6 | 7 => 3,
        _ => return false,
    };
    let Some(device_id) = source.slave_deviceid else {
        return false;
    };
    state.xi_devices.device(device_id).is_some_and(|device| {
        matches!(
            (device.class_shape, axis),
            (crate::xinput::XiClassShape::PhysicalPointer, 2 | 3)
        )
    })
}

/// Fan a host pointer event out to nested clients.
///
/// `handle_grabs` toggles passive-grab matching and active-grab
/// redirection. Pass `false` from `AllowEvents ReplayPointer` to avoid
/// re-checking the same passive grab that was just released.
///
/// `is_replay` is set when the call comes from the core
/// `AllowEvents(ReplayPointer)` re-delivery path. That path keeps XI2
/// fanout suppressed because the original physical event has already
/// been delivered to XI2 listeners. XI2 replay after `XIAllowEvents`
/// uses `is_replay=false` because synchronous XI2 passive grabs must
/// re-deliver the device event to the natural target.
pub fn pointer_event_fanout_to_state(
    state: &mut ServerState,
    backend: &mut dyn crate::backend::Backend,
    xid_map: &HostXidMap,
    event: HostPointerEvent,
    handle_grabs: bool,
    is_replay: bool,
) -> Vec<ClientId> {
    let mut info = ImplicitGrabFanoutInfo::default();
    let dropped = pointer_event_fanout_to_state_inner(
        state,
        backend,
        xid_map,
        event,
        handle_grabs,
        is_replay,
        is_replay,
        false,
        None,
        false,
        None,
        &mut info,
    );
    implicit_pointer_grab_lifecycle(state, &event, &info, None);
    if !info.queued {
        release_passive_grab_on_button_release(state, event.kind, event.origin);
    }
    dropped
}

/// Fan out the events of a warp: `WarpPointer`, a confine grab pulling the
/// pointer in, or the pointer moved onto a CRTC after a layout change.
///
/// As [`pointer_event_fanout_to_state`], except that no XI2 raw event is
/// generated. A raw event describes the device, and a warp did not come
/// from one: Xorg's `ProcWarpPointer` (and `CheckPhysLimits`) call
/// `SetCursorPosition(..., TRUE)`, which `miPointerWarpCursor` turns into
/// `miPointerMove`, whose `GetPointerEvents` passes `POINTER_NORAW`; and
/// `fill_pointer_events` (dix/getevents.c) makes no raw event under that
/// flag. RECORD still sees the motion, as before.
pub fn warp_pointer_event_fanout_to_state(
    state: &mut ServerState,
    backend: &mut dyn crate::backend::Backend,
    xid_map: &HostXidMap,
    event: HostPointerEvent,
) -> Vec<ClientId> {
    let mut info = ImplicitGrabFanoutInfo::default();
    let dropped = pointer_event_fanout_to_state_inner(
        state, backend, xid_map, event, true, false, false, true, None, false, None, &mut info,
    );
    implicit_pointer_grab_lifecycle(state, &event, &info, None);
    if !info.queued {
        release_passive_grab_on_button_release(state, event.kind, event.origin);
    }
    dropped
}

/// Replay a frozen activating pointer event to its natural target.
///
/// Device events must be regenerated because the synchronous grab withheld
/// them from the natural target. Raw events describe physical device input and
/// were already delivered when the event first arrived, so replay must not
/// generate a second raw cookie.
pub fn replay_frozen_pointer_event_to_state(
    state: &mut ServerState,
    backend: &mut dyn crate::backend::Backend,
    xid_map: &HostXidMap,
    event: HostPointerEvent,
    replay: PointerReplay,
) -> Vec<ClientId> {
    let mut info = ImplicitGrabFanoutInfo::default();
    let dropped = pointer_event_fanout_to_state_inner(
        state,
        backend,
        xid_map,
        event,
        true,
        false,
        true,
        false,
        Some(replay),
        false,
        None,
        &mut info,
    );
    implicit_pointer_grab_lifecycle(state, &event, &info, Some(replay));
    if !info.queued {
        release_passive_grab_on_button_release(state, event.kind, event.origin);
    }
    dropped
}

/// Process the master's part of a pointer event whose slave part ran while
/// only the master was frozen (spec invariant 8). Xorg's master pass
/// (`../xserver/mi/mieq.c:507`) is the master's own `processInputProc`:
/// the master's button transition, its passive-grab check for a press, and
/// its core/XI2 master forms all happen here.
///
/// This is the device restriction of [`replay_frozen_pointer_event_to_state`]
/// for the master (only the master's forms, no raw event, no slave form),
/// except that the whole chain is searched for passive grabs (no replay
/// floor: nothing has grabbed this event yet).
fn play_queued_master_pointer_event(
    state: &mut ServerState,
    backend: &mut dyn crate::backend::Backend,
    xid_map: &HostXidMap,
    event: HostPointerEvent,
    master: u16,
    scroll_motion: Option<(u8, i32)>,
) -> Vec<ClientId> {
    let replay = PointerReplay {
        device: master,
        grab_window: ROOT_WINDOW,
    };
    let mut info = ImplicitGrabFanoutInfo::default();
    let dropped = pointer_event_fanout_to_state_inner(
        state,
        backend,
        xid_map,
        event,
        true,
        false,
        true,
        false,
        Some(replay),
        true,
        scroll_motion,
        &mut info,
    );
    implicit_pointer_grab_lifecycle(state, &event, &info, Some(replay));
    if !info.queued {
        release_passive_grab_on_button_release(state, event.kind, event.origin);
    }
    dropped
}

/// Delivery facts one fanout pass feeds the implicit-grab lifecycle —
/// Xorg `ActivateImplicitGrab`'s (client, pWin, deliveryMask, grabtype)
/// arguments (dix/events.c:2150), captured from successful natural press
/// deliveries and resolved the way Xorg's walk resolves them: deepest
/// window first (leaf-to-root propagation), and within one window the
/// CORE form first — `DeliverEventsToWindow` delivers core before XI/XI2
/// and activates the implicit grab on whichever form it was delivering.
#[derive(Clone, Copy)]
struct DeliveredPress {
    owner: ClientId,
    /// Event window the press was delivered on (Xorg tempGrab->window).
    window: ResourceId,
    via_xi2: bool,
    /// Core deliveryMask (the recipient's window selection); 0 for XI2.
    core_mask: u32,
    /// Merged XI2 selection of ALL clients on `window`, snapshot at
    /// delivery (Xorg xi2mask_merge of the window's masks); 0 for core.
    xi2_mask: u64,
}

fn xi2_effective_mask(
    state: &ServerState,
    window: ResourceId,
    device_id: u16,
    is_master: bool,
) -> u64 {
    let mut device_candidates = vec![device_id, 0];
    if is_master {
        device_candidates.push(1);
    }
    state
        .clients
        .values()
        .map(|client| xi2_mask_for_client(client, window, window, &device_candidates))
        .fold(0, |merged, selected| merged | selected)
}

#[derive(Default)]
struct ImplicitGrabFanoutInfo {
    /// Event was withheld by the queue-while-frozen gate: nothing was
    /// delivered, and the `buttons_down` bookkeeping (which already ran)
    /// must not drive grab lifecycle. The lifecycle for this event runs
    /// when `xi1_compute_freezes` replays it.
    queued: bool,
    source_button_transition: bool,
    master_button_transition: bool,
    core_press: Option<DeliveredPress>,
    xi2_master_press: Option<DeliveredPress>,
    xi2_slave_press: Option<DeliveredPress>,
}

impl ImplicitGrabFanoutInfo {
    fn consider_xi2_press(
        &mut self,
        resources: &crate::resources::ResourceTable,
        form: Xi2PointerForm,
        candidate: DeliveredPress,
    ) {
        let slot = match form {
            Xi2PointerForm::Master => &mut self.xi2_master_press,
            Xi2PointerForm::Slave => &mut self.xi2_slave_press,
        };
        let Some(current) = *slot else {
            *slot = Some(candidate);
            return;
        };
        let replace = if candidate.window == current.window {
            resources.window_owner(candidate.window) == Some(candidate.owner)
                && resources.window_owner(current.window) != Some(current.owner)
        } else if resources.is_descendant_of(candidate.window, current.window) {
            true
        } else {
            debug_assert!(
                resources.is_descendant_of(current.window, candidate.window),
                "XI2 implicit-grab candidates must share the hit ancestor chain"
            );
            false
        };
        if replace {
            *slot = Some(candidate);
        }
    }

    fn delivered_master_press(
        &self,
        resources: &crate::resources::ResourceTable,
    ) -> Option<DeliveredPress> {
        match (self.core_press, self.xi2_master_press) {
            // Same window, both forms delivered: the CORE press wins. Xorg
            // `DeliverEventsToWindow` delivers core first and activates the
            // implicit grab from that call, so `ActivateImplicitGrab` types
            // the grab CORE (`type == ButtonPress`, dix/events.c:2158) — its
            // own comment at dix/events.c:2417 spells this out: "since core
            // events are delivered first, an implicit grab may be activated
            // on a core grab, stopping the XI events."
            //
            // Preferring XI2 here typed the implicit grab `via_xi2`, which
            // made the active-grab redirect suppress the CORE form of every
            // subsequent grabbed event (`if !via_xi2` below) while still
            // setting `handled_core_via_grab` — so the core ButtonRelease was
            // captured and dropped, and natural propagation never ran either.
            // Enlightenment selects core ButtonPress on its canvas AND XI2 on
            // the slave pointer (see the slave-device carve-out in the core
            // dedup below), so every dock click lost its release: the button
            // stayed down client-side, turning clicks into drags and then
            // wedging input entirely. Measured on silence/E27 — core
            // press/release 10/2 on yserver vs 3/3 on Xorg, XI2 9/8.
            (Some(core), Some(xi2)) if core.window == xi2.window => Some(core),
            (Some(core), Some(xi2)) if resources.is_descendant_of(xi2.window, core.window) => {
                Some(xi2)
            }
            (Some(core), Some(xi2)) if resources.is_descendant_of(core.window, xi2.window) => {
                Some(core)
            }
            (Some(core), Some(xi2)) => {
                debug_assert!(
                    false,
                    "core/XI2 implicit-grab candidates must share the hit ancestor chain: \
                     core={:?} xi2={:?}",
                    core.window, xi2.window
                );
                Some(xi2)
            }
            (Some(core), None) => Some(core),
            (None, Some(xi2)) => Some(xi2),
            (None, None) => None,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn pointer_event_fanout_to_state_inner(
    state: &mut ServerState,
    backend: &mut dyn crate::backend::Backend,
    xid_map: &HostXidMap,
    event: HostPointerEvent,
    handle_grabs: bool,
    is_replay: bool,
    suppress_raw: bool,
    warp: bool,
    replay: Option<PointerReplay>,
    deferred_master: bool,
    deferred_scroll_motion: Option<(u8, i32)>,
    info: &mut ImplicitGrabFanoutInfo,
) -> Vec<ClientId> {
    // Reject stale/suspended physical events and invalid XTEST targets before
    // pointer mappings, held buttons, barriers, cursor bookkeeping or fanout
    // can observe them. This check also runs for queued/replayed events.
    let Some(xi_source) = resolve_pointer_xi_source(state, event.origin, event.tree_change) else {
        return Vec::new();
    };

    // SetPointerMapping: physical button → logical button before any
    // routing (Xorg UpdateDeviceState applies b->map at event
    // generation). A 0 entry disables the button — the event vanishes.
    let physical_detail = event.detail;
    let mut event = event;
    if matches!(
        event.kind,
        PointerEventKind::ButtonPress | PointerEventKind::ButtonRelease
    ) && let Some(map) = &state.pointer_mapping_override
        && let Some(&mapped) = map.get(usize::from(event.detail).wrapping_sub(1))
    {
        if mapped == 0 {
            return Vec::new();
        }
        event.detail = mapped;
    }
    // Track logical buttons-down for the passive-grab activation
    // predicate (`find_passive_grab` rejects a grab when another
    // button is already down — XGrabButton-1). This is NOT stamped
    // onto delivered events: the backend cooks the authoritative
    // modifier/button state into `event.state` (see the modifier
    // source-of-truth note in `key_fanout`).
    // ComputeFreezes keeps replayDev as the event's device across deactivation
    // and replay (Xorg `dix/events.c:1320,1904`), even if its source is later
    // detached from that master.
    let replay_is_slave = replay.is_some_and(|replay| {
        xi_source.slave_deviceid == Some(replay.device)
            && state.xi_devices.role(replay.device)
                == Some(crate::xinput::XiDeviceRole::SlavePointer)
    });
    let replay_is_master = replay.is_some_and(|replay| {
        state.xi_devices.role(replay.device) == Some(crate::xinput::XiDeviceRole::MasterPointer)
    });
    let replay_has_slave_form = replay.is_none_or(|_| replay_is_slave);
    let replay_has_master_form = replay.is_none_or(|_| replay_is_master);
    let replaying = replay.is_some() || state.playing_sync_events || is_replay || suppress_raw;

    // Resolve the same unified freeze predicate here that the enqueue path
    // below uses. Xorg calls DeviceEventCallback while enqueueing a frozen
    // input event (dix/events.c:1164-1181), before later duplicate suppression;
    // this lets RECORD observe XTEST edges that do not change the master
    // button bitmap while a physical button is already held.
    let source_device_id = xi_source.slave_deviceid.unwrap_or(xi_source.sourceid);
    let freeze_device = [Some(source_device_id), xi_source.attached_master]
        .into_iter()
        .flatten()
        .find(|id| {
            state
                .xi1_frozen
                .get(id)
                .is_some_and(crate::server::Xi1Freeze::frozen)
        });
    let pointer_frozen_unified = freeze_device.is_some();
    let freezable_event = replay.is_none()
        && !is_replay
        && handle_grabs
        && pointer_frozen_unified
        && !matches!(
            event.kind,
            PointerEventKind::EnterNotify | PointerEventKind::LeaveNotify,
        );
    // Xorg freezes per device (`../xserver/mi/mieq.c:502-508`: the slave's
    // `processInputProc` runs first, then the master's; `FreezeThaw` swaps
    // in the enqueue proc for the frozen device only). A frozen slave
    // withholds the whole event; a frozen master with a live slave
    // withholds only the master's part (spec invariant 8).
    let master_part_only = freezable_event
        && xi_source.slave_deviceid.is_some()
        && freeze_device == xi_source.attached_master;
    let queued_while_frozen = freezable_event && !master_part_only;

    let mut button_transition = if matches!(
        event.kind,
        PointerEventKind::ButtonPress | PointerEventKind::ButtonRelease
    ) {
        let transition = if deferred_master {
            PointerButtonTransition {
                source_accepted: true,
                master_accepted: false,
                master_changed: false,
            }
        } else if replay_is_master
            && (1..=16).contains(&event.detail)
            && state.buttons_down & (1u16 << (event.detail - 1)) != 0
        {
            // If the master already has this button, ReplayPointer is
            // reprocessing an edge whose slave half ran on arrival. Later
            // slave input may already have released it while only the master
            // was frozen, so do not restore the source bit from the replay.
            master_button_transition(
                state,
                xi_source,
                1u16 << (event.detail - 1),
                event.kind == PointerEventKind::ButtonPress,
                Some(
                    replay
                        .expect("master replay retains its pending device")
                        .device,
                ),
            )
        } else {
            pointer_button_transition(
                state,
                event.origin,
                xi_source,
                event.detail,
                event.kind == PointerEventKind::ButtonPress,
                !master_part_only,
            )
        };
        if deferred_master {
            // The queued master part: the source's half ran when the slave
            // pass did; the master's runs now, as Xorg's master pass does.
            PointerButtonTransition {
                source_accepted: true,
                ..if (1..=16).contains(&event.detail) {
                    master_button_transition(
                        state,
                        xi_source,
                        1u16 << (event.detail - 1),
                        event.kind == PointerEventKind::ButtonPress,
                        Some(
                            replay
                                .expect("deferred master event retains its pending device")
                                .device,
                        ),
                    )
                } else {
                    transition
                }
            }
        } else if replaying {
            if replay_is_slave {
                // ReplayDevice reprocesses the source device only. Preserve
                // its transition while undoing any aggregate bit newly set
                // by the ordinary source+master transition.
                if transition.master_changed && (1..=16).contains(&event.detail) {
                    state.buttons_down &= !(1u16 << (event.detail - 1));
                }
                PointerButtonTransition {
                    source_accepted: true,
                    master_accepted: false,
                    master_changed: false,
                }
            } else {
                // A frozen input may have updated its held state before it was
                // queued, or it may first become owned when the activating press
                // is replayed. The transition helper is idempotent in both cases;
                // preserve delivery of the queued/replayed event even when that
                // state transition was already applied.
                PointerButtonTransition {
                    source_accepted: true,
                    master_accepted: true,
                    master_changed: transition.master_changed,
                }
            }
        } else {
            transition
        }
    } else {
        PointerButtonTransition {
            source_accepted: true,
            master_accepted: true,
            master_changed: false,
        }
    };
    if replay.is_some() && !deferred_master {
        button_transition.master_accepted = replay_has_master_form;
    }
    if replay_is_slave {
        button_transition.master_changed = false;
    }
    info.source_button_transition = button_transition.source_accepted;
    info.master_button_transition = button_transition.master_accepted;
    // Pointer confinement (Xorg CheckPhysLimits): while a confined
    // grab is active, motion outside the confine rectangle is
    // replaced by a warp to the nearest inside point; press/release
    // coordinates clamp in place. A tree-change crossing is no motion:
    // the request that moved the confine window re-clamps after it.
    if !is_replay
        && !event.tree_change
        && state.pointer_confine_to.0 != 0
        && let Some(w) = state.resources.window(state.pointer_confine_to)
        && w.map_state == crate::resources::MapState::Viewable
    {
        let (x0, y0) = state
            .resources
            .window_absolute_position(state.pointer_confine_to);
        let (x1, y1) = (x0 + i32::from(w.width), y0 + i32::from(w.height));
        let cx = i32::from(event.root_x).clamp(x0, (x1 - 1).max(x0));
        let cy = i32::from(event.root_y).clamp(y0, (y1 - 1).max(y0));
        if cx != i32::from(event.root_x) || cy != i32::from(event.root_y) {
            // Clamp in place — the event delivers at the nearest
            // inside point — and pull the physical cursor along.
            // `warp_pointer_root` re-enters this fanout with the
            // generated motion; the guard stops a second warp if the
            // re-derived coordinates still disagree (recursing here
            // overflowed the stack — 2026-06-07 round-5 crash).
            #[allow(clippy::cast_possible_truncation)]
            {
                event.root_x = cx as i16;
                event.root_y = cy as i16;
            }
            if !state.confine_warp_active {
                state.confine_warp_active = true;
                let prev = state.barrier_bypass;
                state.barrier_bypass = true;
                backend.warp_pointer_root(state, cx, cy);
                state.barrier_bypass = prev;
                state.confine_warp_active = false;
            }
        }
    }
    // Pointer barriers only constrain genuine relative motion. The
    // motion source is already encoded in the event kind/producer:
    // absolute device motion and warps are marked non-relative and
    // skip this block via `barrier_bypass`.
    if !state.pointer_barriers.is_empty() && matches!(event.kind, PointerEventKind::MotionNotify) {
        log::trace!(
            target: "yserver_core::barriers",
            "motion gate: barriers={} bypass={} confine_warp={} replay={} prev=({},{}) new=({},{})",
            state.pointer_barriers.len(),
            state.barrier_bypass,
            state.confine_warp_active,
            is_replay,
            state.pointer_root.0,
            state.pointer_root.1,
            event.root_x,
            event.root_y,
        );
    }
    if !is_replay
        && matches!(event.kind, PointerEventKind::MotionNotify)
        && !state.barrier_bypass
        && !state.confine_warp_active
        && !state.pointer_barriers.is_empty()
    {
        let (ox, oy) = (
            i32::from(state.pointer_root.0),
            i32::from(state.pointer_root.1),
        );
        let mut nx = i32::from(event.root_x);
        let mut ny = i32::from(event.root_y);
        if (nx, ny) != (ox, oy) {
            use crate::core_loop::barriers::{
                BarrierGeom, NEGATIVE_X, NEGATIVE_Y, POSITIVE_X, POSITIVE_Y, clamp_to_barrier,
                direction_of, find_nearest, inside_hit_box,
            };

            // NB: every barrier is treated as applying to the (single)
            // master pointer — `barrier.devices` is intentionally not
            // filtered here. Xorg's barrier_blocks_device matters only
            // with multiple master pointers; create-validation already
            // restricts the device list to {0,1,2} (wildcards + the lone
            // master pointer), so every creatable barrier applies to it.
            // Add the device filter here if yserver ever gains a second
            // master pointer (a spec non-goal today).
            let keys: Vec<u32> = state.pointer_barriers.keys().copied().collect();
            let candidates: Vec<(usize, BarrierGeom)> = keys
                .iter()
                .enumerate()
                .map(|(i, k)| {
                    let b = &state.pointer_barriers[k];
                    (
                        i,
                        BarrierGeom {
                            x1: i32::from(b.x1),
                            y1: i32::from(b.y1),
                            x2: i32::from(b.x2),
                            y2: i32::from(b.y2),
                            directions: b.directions,
                        },
                    )
                })
                .collect();
            let mut seen: Vec<usize> = Vec::new();
            let mut cx = ox;
            let mut cy = oy;
            let mut dir = direction_of(cx, cy, nx, ny);
            log::trace!(
                target: "yserver_core::barriers",
                "clamp scan: prev=({ox},{oy}) new=({nx},{ny}) dir={dir} candidates={}",
                candidates.len(),
            );
            while dir != 0 {
                let Some((idx, _dist, geom)) =
                    find_nearest(&candidates, &seen, dir, cx, cy, nx, ny)
                else {
                    log::trace!(
                        target: "yserver_core::barriers",
                        "clamp scan: no blocking barrier for dir={dir} at ({cx},{cy})->({nx},{ny})",
                    );
                    break;
                };
                log::trace!(
                    target: "yserver_core::barriers",
                    "clamp: barrier idx={idx} matched dir={dir}, clamping ({nx},{ny})",
                );
                seen.push(idx);
                let barrier_xid = keys[idx];
                let Some((barrier_window, barrier_owner, event_id, dtime)) =
                    (match state.pointer_barriers.get_mut(&barrier_xid) {
                        None => None,
                        Some(barrier) => {
                            barrier.seen = true;
                            let was_hit = barrier.hit;
                            barrier.hit = true;
                            if barrier.release_event_id == barrier.event_id {
                                None
                            } else {
                                clamp_to_barrier(&geom, dir, &mut nx, &mut ny);
                                let dtime = if was_hit {
                                    event.time.saturating_sub(barrier.last_timestamp)
                                } else {
                                    0
                                };
                                let barrier_window = barrier.window;
                                let barrier_owner = barrier.owner;
                                let event_id = barrier.event_id;
                                barrier.last_timestamp = event.time;
                                if geom.x1 == geom.x2 {
                                    dir &= !(POSITIVE_X | NEGATIVE_X);
                                    cx = nx;
                                } else {
                                    dir &= !(POSITIVE_Y | NEGATIVE_Y);
                                    cy = ny;
                                }
                                Some((barrier_window, barrier_owner, event_id, dtime))
                            }
                        }
                    })
                else {
                    continue;
                };
                let _dropped = emit_barrier_event(
                    state,
                    barrier_xid,
                    barrier_owner,
                    barrier_window,
                    25,
                    event.time,
                    event_id,
                    dtime,
                    0,
                    2,
                    nx,
                    ny,
                    f64::from(nx - ox),
                    f64::from(ny - oy),
                );
            }

            // A clamp occurred iff the scan moved (nx,ny) off the raw
            // post-motion position. Compare against the *incoming*
            // event coords (still un-clamped here) — NOT (ox,oy), which
            // is the previous position and so would be "different" on
            // every ordinary motion, warping needlessly each frame.
            let clamped = (nx, ny) != (i32::from(event.root_x), i32::from(event.root_y));
            #[allow(clippy::cast_possible_truncation)]
            {
                // Shift the window-relative coords by the same delta the
                // clamp applied to root. `translate_host_event` (run
                // later, at the tail of this fn) re-derives
                // `root = window.x + event_x`, so without shifting
                // `event_x` it would overwrite the clamped root with the
                // un-clamped value — leaving `pointer_root` past the
                // wall, so the next motion segment never re-crosses the
                // barrier (porous barrier; HW-observed 2026-06-18).
                let ddx = nx - i32::from(event.root_x);
                let ddy = ny - i32::from(event.root_y);
                event.event_x = (i32::from(event.event_x) + ddx) as i16;
                event.event_y = (i32::from(event.event_y) + ddy) as i16;
                event.root_x = nx as i16;
                event.root_y = ny as i16;
            }
            if clamped && !state.confine_warp_active {
                state.confine_warp_active = true;
                let prev = state.barrier_bypass;
                state.barrier_bypass = true;
                backend.warp_pointer_root(state, nx, ny);
                state.barrier_bypass = prev;
                state.confine_warp_active = false;
            }

            let mut leave_targets: Vec<(u32, crate::server::PointerBarrier)> = Vec::new();
            for (barrier_xid, barrier) in state.pointer_barriers.iter_mut() {
                barrier.seen = false;
                if !barrier.hit {
                    continue;
                }
                let geom = BarrierGeom {
                    x1: i32::from(barrier.x1),
                    y1: i32::from(barrier.y1),
                    x2: i32::from(barrier.x2),
                    y2: i32::from(barrier.y2),
                    directions: barrier.directions,
                };
                if inside_hit_box(&geom, nx, ny) {
                    continue;
                }
                barrier.hit = false;
                barrier.event_id = barrier.event_id.saturating_add(1);
                leave_targets.push((*barrier_xid, barrier.clone()));
            }
            for (barrier_xid, barrier) in leave_targets {
                let _dropped = emit_barrier_event(
                    state,
                    barrier_xid,
                    barrier.owner,
                    barrier.window,
                    26,
                    event.time,
                    barrier.event_id.saturating_sub(1),
                    event.time.saturating_sub(barrier.last_timestamp),
                    0,
                    2,
                    nx,
                    ny,
                    0.0,
                    0.0,
                );
            }
        }
    }
    // A tree-change crossing is not user input: it neither resets the idle
    // clocks nor wakes DPMS or the screen saver.
    if !event.tree_change {
        let now = std::time::Instant::now();
        // Capture priors BEFORE mutating; needed by the IDLETIME wake handler.
        #[allow(clippy::cast_possible_truncation)]
        let prior_global = now
            .duration_since(state.dpms.last_activity)
            .as_millis()
            .min(u128::from(u32::MAX)) as i64;
        // XI2 master device IDs are always small (2 here); cast u16 → u8 is safe.
        // Per-device prior: fall back to global if no per-device entry yet.
        // Matches `idletime_baseline`'s fallback (server.rs Task 1) — without
        // this, the very first input event for a device whose baseline isn't
        // recorded would compute prior_device=0 and a per-device Negative
        // alarm (whose wait_value > 0) would not see the `old > wait` half of
        // its trigger.
        let prior_device = state
            .per_device_last_activity
            .get(&(XI2_MASTER_POINTER_DEVICE_ID as u8))
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
            .insert(XI2_MASTER_POINTER_DEVICE_ID as u8, now);

        // IDLETIME wake: fires Negative-* alarms before the input event itself
        // reaches clients (predictable ordering).
        crate::core_loop::process_request::evaluate_idletime_negative_alarms_on_input_wake(
            state,
            XI2_MASTER_POINTER_DEVICE_ID as u8,
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
    }

    let mut dropped = Vec::new();

    // Step 1 — translate host-screen coords to ynest-root coords.
    let event = translate_host_event(state, xid_map, event);

    // Resolve passive activation before master processing. Xorg's
    // `mieqProcessDeviceEvent` skips the master pass when a slave activation
    // detaches that slave (`../xserver/mi/mieq.c:502-508`); in particular,
    // RECORD's device-event callback must not see a core event for it.
    // Keep the matched grab for the activation block below so this remains
    // the only activation path.
    let replay_device_has_active_grab =
        replay.is_some_and(|replay| active_grab_for_device(state, replay.device).is_some());
    let passive_grab_blocked = if replay.is_some() {
        replay_device_has_active_grab
    } else {
        active_grab_for_source(state, xi_source).is_some()
    };
    let passive_transition_accepted = if deferred_master {
        button_transition.master_accepted
    } else {
        button_transition.source_accepted
    };
    let mut passive_grab_match = if !queued_while_frozen
        && handle_grabs
        && event.kind == PointerEventKind::ButtonPress
        && passive_transition_accepted
        && !passive_grab_blocked
    {
        try_match_passive_grab(
            state,
            xid_map,
            event,
            replay,
            deferred_master,
            button_transition.master_accepted,
        )
    } else {
        None
    };
    let passive_slave_activation = passive_grab_match
        .as_ref()
        .is_some_and(|(_, _, pass)| matches!(pass, crate::server::PassiveGrabPass::Slave(_)));
    if passive_slave_activation {
        // The source transition must precede lookup so its first-button
        // predicate sees the pressed slave bit. Roll back the aggregate
        // master bit before any master side effect or RECORD callback.
        if button_transition.master_changed && (1..=16).contains(&event.detail) {
            state.buttons_down &= !(1u16 << (event.detail - 1));
            button_transition.master_changed = false;
        }
        button_transition.master_accepted = false;
        info.master_button_transition = false;
    }
    // A master pass the frozen master cannot run yet (spec invariant 8): its
    // passive grabs are not searched now, and its forms wait in the queue,
    // whether or not the master's button bitmap changes (Xorg enqueues the
    // master copy first and drops duplicates when it dequeues it). A slave
    // activation skips the master pass altogether.
    if master_part_only
        && matches!(
            passive_grab_match,
            Some((_, _, crate::server::PassiveGrabPass::Master))
        )
    {
        passive_grab_match = None;
    }
    let master_part_deferred = master_part_only && !passive_slave_activation;
    if master_part_deferred {
        // Core and XI2 master forms wait for the master's pass; the
        // transition above already left the master half undone.
        button_transition.master_accepted = false;
        info.master_button_transition = false;
    }

    // Xorg's GetPointerEvents calls UpdateFromMaster before applying the
    // generated scroll valuator. DeviceChanged must therefore expose the
    // source's previous scroll baseline, with this event's new value applied
    // only afterwards.
    if button_transition.source_accepted && !passive_slave_activation && !replay_is_slave {
        merge_dropped(
            &mut dropped,
            update_from_pointer_master(state, xi_source, !is_replay && !suppress_raw, true),
        );
    }

    let source_has_scroll_class = pointer_source_has_scroll_class(state, xi_source, event.detail);
    // The slave pass generates the cooked valuator once. A deferred master
    // pass receives that exact axis/value from its pending event instead of
    // incrementing the source's cumulative counter again.
    let scroll_motion = if deferred_master {
        deferred_scroll_motion
    } else if !queued_while_frozen
        && event.kind == PointerEventKind::ButtonPress
        && button_transition.source_accepted
        && source_has_scroll_class
    {
        let (axis_idx, delta): (usize, i32) = match event.detail {
            4 => (0, -1),
            5 => (0, 1),
            6 => (1, -1),
            7 => (1, 1),
            _ => unreachable!(),
        };
        if let Some(device_id) = xi_source.slave_deviceid
            && let Some(device) = state.xi_devices.device_mut(device_id)
        {
            device.scroll_axis_values[axis_idx] =
                device.scroll_axis_values[axis_idx].wrapping_add(delta);
        } else if xi_source.attached_master.is_some() {
            state.scroll_axis_value[axis_idx] =
                state.scroll_axis_value[axis_idx].wrapping_add(delta);
        }
        let axis = if axis_idx == 0 { 2 } else { 3 };
        let value = xi_source
            .slave_deviceid
            .and_then(|source_id| state.xi_devices.device(source_id))
            .map_or(state.scroll_axis_value[axis_idx], |device| {
                device.scroll_axis_values[axis_idx]
            });
        Some((axis, value))
    } else {
        None
    };

    // RECORD sees each physical pointer event once, before grabs and
    // delivery, and not again when a frozen queue replays it (Xorg
    // `ProcessDeviceEvent` skips the callback while playingEvents).
    let master_record_accepted = match event.kind {
        PointerEventKind::ButtonPress | PointerEventKind::ButtonRelease => {
            button_transition.master_accepted
        }
        // A floating slave has no master copy in Xorg's mieq path
        // (mi/mieq.c:397-425), so it cannot produce a core MotionNotify for
        // RECORD either.
        PointerEventKind::MotionNotify => xi_source.attached_master.is_some(),
        PointerEventKind::EnterNotify | PointerEventKind::LeaveNotify => false,
    };
    if !suppress_raw
        && !state.playing_sync_events
        && xi_source.attached_master.is_some()
        && (master_record_accepted || queued_while_frozen || master_part_deferred)
    {
        let event_type = match event.kind {
            PointerEventKind::ButtonPress => Some(4),
            PointerEventKind::ButtonRelease => Some(5),
            PointerEventKind::MotionNotify => Some(6),
            PointerEventKind::EnterNotify | PointerEventKind::LeaveNotify => None,
        };
        if let Some(event_type) = event_type {
            crate::core_loop::record::record_device_event(
                state,
                crate::core_loop::record::RecordedDeviceEvent {
                    event_type,
                    detail: if event_type == 6 { 0 } else { event.detail },
                    repeat: false,
                    time: event.time,
                    root_x: event.root_x,
                    root_y: event.root_y,
                    state: event.state,
                },
            );
        }
    }

    // The master part queued behind a frozen master re-enters here at thaw;
    // its motion record was made when the slave part was processed.
    if matches!(event.kind, PointerEventKind::MotionNotify) && replay.is_none() {
        const MOTION_HISTORY_CAPACITY: usize = 256;
        if state.pointer_motion_history.len() == MOTION_HISTORY_CAPACITY {
            state.pointer_motion_history.pop_front();
        }
        state
            .pointer_motion_history
            .push_back(crate::server::PointerMotionRecord {
                time: event.time,
                root_x: event.root_x,
                root_y: event.root_y,
            });
    }

    // Cache the pointer position so server-generated events that must
    // carry it (XI2 focus events) don't ship (0,0). Mirrors Xorg keeping
    // the sprite position in device state.
    let floating_device = xi_source
        .slave_deviceid
        .filter(|_| xi_source.attached_master.is_none());
    let sprite_before = state.master_sprite_root;
    if let Some(device_id) = floating_device {
        state.floating_pointer_positions.insert(
            device_id,
            (f32::from(event.root_x), f32::from(event.root_y)),
        );
    } else {
        state.pointer_root = (event.root_x, event.root_y);
    }

    // Sync-passive-grab freeze queue (Xorg `dix/events.c:1320`
    // ComputeFreezes + PlayReleasedEvents). While a sync passive
    // grab is frozen — between the activating press and
    // AllowEvents — subsequent pointer events MUST NOT leak to
    // the natural target. Marco does ~10 round-trips of focus and
    // property work between the press and AllowEvents(ReplayPointer);
    // a fast user release in that window would otherwise reach the
    // app before the replayed press, malforming the gesture
    // (menus + titlebar drags break). Queue them here for replay.
    //
    // Crossings (Enter/Leave) and the replay path itself bypass —
    // crossings are pointer-tracking notifications Xorg doesn't
    // queue, and the replay re-entry mustn't recursively re-queue.
    //
    // The enqueue decision keys on the UNIFIED per-device freeze state
    // ALONE — Xorg has ONE freeze signal, `ComputeFreezes`
    // (dix/events.c:1327) sets `sync.frozen = sync.other || state >=
    // FROZEN`, which is exactly `Xi1Freeze::frozen()`, and the device
    // uses the enqueue proc iff `sync.frozen`. A sync passive grab
    // activation sets this state (FrozenNoEvent, via
    // `xi1_check_grab_for_syncs` in the activation path below), so the
    // unified flag fully covers it. (Historically a second core
    // representation — `pointer_grab_is_passive` + a core activating-event
    // slot — was OR'd in here; several paths thawed the unified state
    // independently, and the two disagreeing wedged the pointer forever:
    // the Steam menu/Library input-wedge, HW 2026-07-15, #94 follow-up.
    // The dual representation has since been removed; `xi1_frozen` is the
    // single source of truth.)
    if queued_while_frozen {
        log::trace!(
            "pointer_fanout: QUEUE-WHILE-FROZEN kind={:?} button={} root=({},{}) queue_len={}",
            event.kind,
            event.detail,
            event.root_x,
            event.root_y,
            state.sync_pending.len() + 1,
        );
        // Store the physical button detail: replay re-enters mapping, so a
        // mapped detail here would be mapped a second time.
        let mut canonical = event;
        canonical.detail = physical_detail;
        state
            .sync_pending
            .push_back(crate::server::PendingSyncEvent {
                device: freeze_device.expect("frozen flag has its controlling device"),
                event: crate::server::QueuedInputEvent::HostPointer(canonical),
            });
        info.queued = true;
        return dropped;
    }
    if master_part_deferred {
        log::trace!(
            "pointer_fanout: MASTER-PART-QUEUED kind={:?} button={} root=({},{}) queue_len={}",
            event.kind,
            event.detail,
            event.root_x,
            event.root_y,
            state.sync_pending.len() + 1,
        );
        // Physical detail again: the thaw re-enters the button mapping.
        let mut canonical = event;
        canonical.detail = physical_detail;
        state
            .sync_pending
            .push_back(crate::server::PendingSyncEvent {
                device: freeze_device.expect("frozen master controls the deferred part"),
                event: crate::server::QueuedInputEvent::HostPointerMaster(canonical, scroll_motion),
            });
    }

    // The master cursor snapshot advances only when its event form is
    // processed. A live slave pass cannot move the sprite while the master
    // form waits behind a freeze, so successive slave forms keep the same
    // pre-move recipient until thaw. Whole events queued on the source device
    // and floating input do not advance this master cursor snapshot.
    if floating_device.is_none()
        && !queued_while_frozen
        && !master_part_deferred
        && !passive_slave_activation
        && !replay_is_slave
        && !matches!(
            event.kind,
            PointerEventKind::EnterNotify | PointerEventKind::LeaveNotify,
        )
    {
        state.master_sprite_root = (event.root_x, event.root_y);
    }

    if matches!(
        event.kind,
        PointerEventKind::ButtonPress | PointerEventKind::ButtonRelease
    ) {
        log::trace!(
            "pointer_fanout entry: kind={:?} button={} host_xid=0x{:x} root=({},{}) event_xy=({},{})",
            event.kind,
            event.detail,
            event.host_xid,
            event.root_x,
            event.root_y,
            event.event_x,
            event.event_y,
        );
    }

    // Resolve the actual hit window (deepest mapped child under cursor)
    // up front. We need it for both the core-event paths below (passive
    // grab matching, normal propagation) and for the XI2 fanout.
    // Buttons: target locked at event generation (host_xid); motion/
    // crossings: live pointer. See `resolve_pointer_hit` — fixes
    // click-below (restack between press and delivery retargeting the
    // in-flight click to the window raised on top in the meantime).
    let root_hit = resolve_pointer_hit(state, xid_map, &event);
    let top_level_id_opt = root_hit
        .map(|(target, _, _)| state.top_level_for_target(target))
        .or_else(|| xid_map.get(&event.host_xid).copied());
    let top_level_id = top_level_id_opt.unwrap_or(ROOT_WINDOW);
    let (target, target_x, target_y) = root_hit.unwrap_or_else(|| {
        xid_map
            .get(&event.host_xid)
            .copied()
            .and_then(|tl| {
                state
                    .pointer_target_at(tl, event.event_x, event.event_y)
                    .or(Some((tl, event.event_x, event.event_y)))
            })
            .unwrap_or((ROOT_WINDOW, event.event_x, event.event_y))
    });

    // Click-hit diagnostic: for a button press, log the resolved hit
    // window AND the clients the press will actually be delivered to
    // (core + XI2), each named by WM_CLASS. Delivery keys off `target`
    // (= root_hit) for both forms, so this exposes any divergence
    // between "where the hit-test resolved" and "who received the
    // click" — and the per-sibling stacking/shape breakdown shows WHY a
    // press fell through a higher window onto the one below. Gated to
    // trace level on a dedicated target so it stays zero-cost otherwise.
    if matches!(event.kind, PointerEventKind::ButtonPress)
        && log::log_enabled!(target: "yserver::input::clickhit", log::Level::Trace)
    {
        let host_label = xid_map
            .get(&event.host_xid)
            .map_or_else(|| "<none>".to_string(), |id| state.debug_window_label(*id));
        let grab_label = active_grab_target(state).map_or_else(
            || "<none>".to_string(),
            |(win, client, _, _, owner_events, via_xi2, _)| {
                format!(
                    "redirect_to={} {} owner_events={owner_events} via_xi2={via_xi2}",
                    state.debug_window_label(win),
                    state.debug_client_label(client),
                )
            },
        );
        let mask_bit = pointer_mask_bit(event.kind, event.state);
        let core_clients = pointer_propagation_target_by_id(
            state,
            target,
            target_x,
            target_y,
            mask_bit,
            xi2_absorbing_evtype(event.kind),
        )
        .map(|(_, _, _, c, _)| c)
        .unwrap_or_default();
        let xi2_evt = xi2_evtype(event.kind);
        let xi2_clients = compute_xi2_targets_for_source(
            state,
            target,
            top_level_id,
            xi2_evt,
            xi_source.slave_deviceid,
            None,
        );
        let label_clients = |cs: &[ClientId]| -> String {
            if cs.is_empty() {
                "<none>".to_string()
            } else {
                cs.iter()
                    .map(|c| state.debug_client_label(*c))
                    .collect::<Vec<_>>()
                    .join(",")
            }
        };
        log::trace!(
            target: "yserver::input::clickhit",
            "BUTTON-PRESS detail={} producer_host_xid=0x{:x}->{host_label} \
             active_grab=[{grab_label}] DELIVERS core_to=[{}] xi2_to=[{}]\n{}\n  {}",
            event.detail,
            event.host_xid,
            label_clients(&core_clients),
            label_clients(&xi2_clients),
            state.debug_explain_pointer_hit(event.root_x, event.root_y),
            state.debug_net_client_list_stacking(),
        );
    }

    // ── Core fanout ─────────────────────────────────────────────────
    let mut handled_core_via_grab = false;
    let mut deferred_core_events = Vec::new();

    // Step 2 — active-grab redirection (core events only).
    // Grab delivery is not stage-gated in Xorg: queued events drained by
    // PlayReleasedEvents deliver through DeliverGrabbedEvent while the
    // grab persists. `handle_grabs=false` re-entries (ReplayDevice/
    // ReplayPointer press replays, xi1_compute_freezes queue drains) must
    // still honor an active grab — the passive-grab MATCHING and freeze
    // QUEUE stay handle_grabs-gated (those are the re-entry hazards).
    // With no grab in effect, active_grab_target is None and this is the
    // exact pre-fix behavior for the replayed press.
    if button_transition.master_accepted
        && let Some(grab) = grab_for_form(state, xi_source, Xi2PointerForm::Master, replay)
        && let Some((grab_window, grab_client, gx, gy, owner_events, via_xi2, grab_event_mask)) =
            active_pointer_grab_target(state, grab)
    {
        // Xorg DeliverGrabbedEvent's owner_events=true rule is NOT
        // "the immediate hit window is owned by the grab client".
        // It is "the event would normally be reported to the grab
        // client" — i.e. the usual propagation walk, filtered to the
        // grab client. This matters for WMs that grab on a tiny hidden
        // helper window but select motion on a visible ancestor/frame:
        // once the pointer moves over a foreign app child, the event
        // should still propagate naturally to the WM's selected frame
        // window, not snap over to the helper grab_window with huge
        // grab-relative coordinates.
        let mask_bit = pointer_mask_bit(event.kind, event.state);
        let natural_route = (!matches!(
            event.kind,
            PointerEventKind::EnterNotify | PointerEventKind::LeaveNotify
        ) && owner_events)
            .then(|| {
                owner_events_route(
                    state,
                    Xi2PointerForm::Master,
                    xi_source.slave_deviceid,
                    grab_client,
                    target,
                    target_x,
                    target_y,
                    mask_bit,
                    Some(xi2_evtype(event.kind)),
                    None,
                )
            });
        if let Some(route) = natural_route {
            match route {
                OwnerEventsRoute::Natural {
                    window,
                    x,
                    y,
                    child,
                    protocol: OwnerProtocol::Core,
                } => {
                    let focus = state.crossing_has_focus(window);
                    let extras = fanout_core_pointer_event_ordered(
                        state,
                        &mut deferred_core_events,
                        &[grab_client],
                        window,
                        child,
                        event,
                        x,
                        y,
                        focus,
                        xi_source,
                        target,
                        top_level_id_opt.unwrap_or(ROOT_WINDOW),
                        is_replay,
                    );
                    merge_dropped(&mut dropped, extras);
                    handled_core_via_grab = true;
                }
                OwnerEventsRoute::Natural { .. } => {
                    // The XI2 form is emitted by its own fanout below. A
                    // master grab still consumes this core copy.
                    handled_core_via_grab = true;
                }
                OwnerEventsRoute::Fallback => {
                    if !via_xi2 && grab_event_mask & mask_bit != 0 {
                        let event_x = clamp_grab_coord(event.root_x, gx);
                        let event_y = clamp_grab_coord(event.root_y, gy);
                        let focus = state.crossing_has_focus(grab_window);
                        // Xorg DeliverGrabbedEvent -> FixUpEventFromWindow:
                        // the grab window's child toward the pointer.
                        let child = xi2_child_toward(state, grab_window, target);
                        let extras = fanout_core_pointer_event_ordered(
                            state,
                            &mut deferred_core_events,
                            &[grab_client],
                            grab_window,
                            child,
                            event,
                            event_x,
                            event_y,
                            focus,
                            xi_source,
                            target,
                            top_level_id_opt.unwrap_or(ROOT_WINDOW),
                            is_replay,
                        );
                        merge_dropped(&mut dropped, extras);
                    }
                    handled_core_via_grab = true;
                }
            }
        } else if !matches!(
            event.kind,
            PointerEventKind::EnterNotify | PointerEventKind::LeaveNotify
        ) {
            // owner_events=false goes directly to the grab window.
            if !via_xi2 && grab_event_mask & mask_bit != 0 {
                let event_x = clamp_grab_coord(event.root_x, gx);
                let event_y = clamp_grab_coord(event.root_y, gy);
                let focus = state.crossing_has_focus(grab_window);
                let child = xi2_child_toward(state, grab_window, target);
                let extras = fanout_core_pointer_event_ordered(
                    state,
                    &mut deferred_core_events,
                    &[grab_client],
                    grab_window,
                    child,
                    event,
                    event_x,
                    event_y,
                    focus,
                    xi_source,
                    target,
                    top_level_id_opt.unwrap_or(ROOT_WINDOW),
                    is_replay,
                );
                merge_dropped(&mut dropped, extras);
            }
            handled_core_via_grab = true;
        }
        // For Enter/Leave (natural pointer crossings between windows),
        // never mark handled_core_via_grab — let them fall through to
        // the normal core propagation in step 4. Pre-fix the existing
        // code set the flag unconditionally and dropped natural
        // crossings entirely while a grab was active, so GTK3 menus
        // (xfce4-panel's main menu, marco's title-bar popup) never
        // received the "pointer entered me" notification needed to
        // engage hover/click tracking. Matches Xorg
        // `dix/events.c::DeliverGrabbedEvent` which only re-routes
        // pointer events explicitly listed in the grab's event mask.
    }

    // Step 3 — passive button-grab matching for ButtonPress.
    //
    // Delivery mirrors Xorg `ActivatePassiveGrab` (dix/events.c:3833):
    //
    // 1. The activating press goes to the grab client on the GRAB
    //    WINDOW — `FixUpEventFromWindow(grab->window)` then
    //    `TryClientEvents(rClient(grab), …)` with the event's own
    //    filter. No `owner_events` walk and no mask check: that walk
    //    lives in `DeliverGrabbedEvent` and only applies to the events
    //    that follow (GH #277, probe V9: event = grab window, child =
    //    its child toward the sprite, even when the grab client
    //    selects the press on that child or on an ancestor).
    // 2. `GrabModeSync` freezes the pointer queue and stores the press
    //    for a ReplayPointer.
    //
    // Pre-fix the qualification accepted any descendant of the grab
    // window — wmaker's click-to-focus sync grab on a CLIENT's
    // window leaked the activating press to the app client while
    // the queue froze; wmaker never saw the press, never called
    // AllowEvents, and the pointer stream wedged (cursor moves,
    // clicks dead — silence HW 2026-06-04).
    // Xorg `Xi/exevents.c:1925`: `CheckDeviceGrabs` (passive-grab
    // matching) only runs on a ButtonPress when there is no active
    // grab — `if (!grab && CheckDeviceGrabs(...))`. With an active
    // grab in place the press goes through `DeliverGrabbedEvent`
    // instead. Gating the activation on `!handled_core_via_grab`
    // alone is wrong because step 2 above intentionally falls through
    // (does NOT set `handled_core_via_grab`) for `owner_events=true`
    // when the natural target is owned by the grab client (GTK3 menu
    // pattern). That fall-through ran passive-grab matching while an
    // active grab was already in place, activated a SYNC passive grab
    // on the same press, stored the activating event (`Xi1Freeze::stored`),
    // and the unified-freeze QUEUE-WHILE-FROZEN check then swallowed
    // every subsequent press/release into a growing queue with no
    // path to thaw — the XFCE/MATE click-lockup that appeared after
    // the unified-freeze work landed on master.
    let active_grab_present = if replay.is_some() {
        replay_device_has_active_grab
    } else {
        active_grab_for_source(state, xi_source).is_some()
    };
    if !handled_core_via_grab
        && !active_grab_present
        && handle_grabs
        && event.kind == PointerEventKind::ButtonPress
        && button_transition.source_accepted
        && let Some((grab, _hit_window, pass)) = passive_grab_match.take()
    {
        log::debug!(
            "pointer_fanout: PASSIVE-GRAB match button={} grab_owner={:?} grab_window=0x{:x} mode={} owner_events={}",
            event.detail,
            grab.owner,
            grab.grab_window.0,
            grab.pointer_mode,
            grab.owner_events,
        );
        // Activate the passive grab atomically with the dispatch.
        let active_grab = crate::server::ActivePointerGrab {
            owner: grab.owner,
            grab_window: grab.grab_window,
            event_mask: (grab.event_mask & u32::from(u16::MAX)) as u16,
            cursor: ResourceId(0),
            time: event.time,
            owner_events: grab.owner_events,
            via_xi2: grab.via_xi2,
            implicit: false,
            passive: true,
            xi2_mask: if grab.via_xi2 {
                u64::from(grab.event_mask)
            } else {
                0
            },
        };
        let (dynamic_device, sync_device) = match pass {
            crate::server::PassiveGrabPass::Slave(device_id) => (true, device_id),
            crate::server::PassiveGrabPass::Master => {
                (false, crate::xinput::DEVICEID_MASTER_POINTER)
            }
        };
        if dynamic_device {
            state.xi2_pointer_grabs.insert(sync_device, active_grab);
            let _ = state.detach_xi2_slave(sync_device);
        } else {
            state.set_pointer_grab(active_grab);
        }

        // Xorg `ActivatePointerGrab` → `DoEnterLeaveEvents(sprite →
        // grab window, NotifyGrab)` (dix/events.c:1635): the sprite
        // moves up to the grab window, emitting the Leave(sprite)…
        // Enter(grab_window) crossing chain BEFORE the activating
        // event. Mirrors the explicit-grab path (GrabPointer). Without
        // it, icewm's `YWindow::handleCrossing` — which hides a panel
        // tooltip on ANY LeaveNotify — never sees the widget's Leave, so
        // the tooltip showing when you click a panel item never goes
        // away (traced: tooltip windows mapped, never destroyed; air HW
        // 2026-07-01). The symmetric NotifyUngrab chain is emitted when
        // the grab tears down (AllowEvents ReplayPointer / release).
        if !dynamic_device {
            crate::core_loop::process_request::emit_core_pointer_grab_chain(
                state,
                target,
                grab.grab_window,
                1, // NotifyGrab
            );
        }

        let mut delivered = false;
        if let Some(grab_target) = client_target_id(state, grab.owner) {
            // Xorg ActivatePassiveGrab hands the activating event to the
            // grabbing client with the event's own filter as the mask
            // (dix/events.c TryClientEvents(..., GetEventFilter(device, xE),
            // ...)), so it arrives even when the grab's event mask lacks
            // ButtonPress — dtwm's front panel grabs ButtonRelease only in
            // sync mode and thaws on the press it is handed.
            // Xorg DeliverOneGrabbedEvent -> FixUpEventFromWindow always
            // rewrites eventX/Y from the root coordinates and the grab
            // window's screen-absolute origin.  The producer's event_x/y
            // are relative to its own host window (and on KMS are root
            // relative), so copying them here reports bogus coordinates
            // whenever the grab window is not at the origin.  Marco then
            // mistakes ordinary Steam content clicks for frame actions and
            // starts a toplevel move.
            if grab.via_xi2 {
                // XI2 passive activation is delivered by the XI2 form path
                // below. Never also substitute a core ButtonPress, whether
                // this grab belongs to a slave or the master.
                delivered = true;
            } else {
                let (gx, gy) = state.resources.window_absolute_position(grab.grab_window);
                let event_x = clamp_grab_coord(event.root_x, gx);
                let event_y = clamp_grab_coord(event.root_y, gy);
                let focus = state.crossing_has_focus(grab.grab_window);
                // As an active grab's (FixUpEventFromWindow).
                let grab_child = xi2_child_toward(state, grab.grab_window, target);
                let extras = fanout_core_pointer_event_ordered(
                    state,
                    &mut deferred_core_events,
                    &[grab_target],
                    grab.grab_window,
                    grab_child,
                    event,
                    event_x,
                    event_y,
                    focus,
                    xi_source,
                    target,
                    top_level_id_opt.unwrap_or(ROOT_WINDOW),
                    is_replay,
                );
                merge_dropped(&mut dropped, extras);
                delivered = true;
            }
        }
        // Xorg ActivatePointerGrab → CheckGrabForSyncs: a sync
        // pointer_mode freezes the pointer's device stream; a sync
        // keyboard_mode holds the KEYBOARD on this grab's behalf
        // (XGrabButton-18/19/20 freeze the keyboard via a button
        // grab and thaw it with AllowEvents).
        xi1_check_grab_for_syncs(
            state,
            sync_device,
            grab.owner,
            grab.pointer_mode == 0,
            grab.keyboard_mode == 0,
        );
        // Like XI1 passive activation below, this event has already been
        // delivered to the grab owner and is the event ReplayDevice must
        // reprocess at the natural target (FrozenWithEvent).
        if delivered && grab.pointer_mode == 0 {
            let sync = state.xi1_frozen.entry(sync_device).or_default();
            if sync.state == crate::server::Xi1SyncState::FrozenNoEvent {
                sync.state = crate::server::Xi1SyncState::FrozenWithEvent;
                sync.stored = Some(crate::server::QueuedInputEvent::HostPointer(event));
            }
        }
        // ConfineCursorToWindow — record the confinement and pull
        // the pointer inside the confine window (XGrabButton-23/24).
        state.pointer_confine_to = grab.confine_to;
        if grab.confine_to.0 != 0 {
            crate::core_loop::process_request::confine_pointer_now(state, backend);
        }
        // During a grab, core pointer events never reach other
        // clients — both branches above are the only deliveries.
        handled_core_via_grab = true;
    }

    // Step 4 — normal core propagation, only when no grab took ownership.
    //
    // For Crossing events we also run when top_level_id is None: the
    // producer (`update_pointer_window`) emits Leave/Enter chain events
    // with host_xid pointing at the KMS root container for the
    // ROOT_WINDOW endpoint, and that host_xid isn't in xid_map.
    let is_crossing = matches!(
        event.kind,
        PointerEventKind::EnterNotify | PointerEventKind::LeaveNotify
    );
    if !handled_core_via_grab
        && button_transition.master_accepted
        && (top_level_id_opt.is_some() || is_crossing)
    {
        let mask_bit = pointer_mask_bit(event.kind, event.state);
        // Crossings carry a *per-window* endpoint (the producer stamps each
        // Leave/Enter chain event with that window's host_xid). The live
        // `target` above tracks the deepest hit, which for a Leave is the
        // window being ENTERED, not the one being left — so resolving the
        // whole chain against `target` collapses every event onto the
        // destination and starves the left window of its LeaveNotify.
        // icewm hides a per-widget tooltip on LeaveNotify, so the tooltip on
        // the widget the pointer left never disappeared (air HW 2026-07-02).
        // Route crossings from their own window (host_xid → resource),
        // mirroring the XI2 path below. Derive event coordinates from the
        // core tree instead of trusting the backend's event_x/y: KMS only
        // has local geometry for its scene top-levels, so synthetic chain
        // events on nested CEF windows otherwise fall back to root coords.
        // Xorg computes the coordinates independently for every window in
        // the crossing chain (EnterLeaveEvent / FixUpEventFromWindow).
        let (cross_start, cross_x, cross_y) = if is_crossing {
            xid_map
                .get(&event.host_xid)
                .copied()
                .map_or((target, target_x, target_y), |cw| {
                    let (ox, oy) = state.resources.window_absolute_position(cw);
                    (
                        cw,
                        clamp_grab_coord(event.root_x, ox),
                        clamp_grab_coord(event.root_y, oy),
                    )
                })
        } else {
            (target, target_x, target_y)
        };
        let (nested_id, event_x, event_y, mut core_targets, propagation_child) = if is_crossing {
            (
                cross_start,
                cross_x,
                cross_y,
                core_crossing_targets(state, cross_start, mask_bit),
                ResourceId(0),
            )
        } else {
            pointer_propagation_target_by_id(
                state,
                cross_start,
                cross_x,
                cross_y,
                mask_bit,
                xi2_absorbing_evtype(event.kind),
            )
            .unwrap_or((cross_start, cross_x, cross_y, Vec::new(), ResourceId(0)))
        };

        // XI2 shadows core per client (Xorg behaviour, mirrors
        // `deliver_key_to_window`): a client that receives the XI2
        // form of this event must NOT also receive the core form, or
        // it processes every click/motion twice. Chromium's Ozone X11
        // layer selects both core and XI2 on its windows; without this
        // dedup its 3-dots / extensions menu got two ButtonPress
        // events and opened-then-closed. Skip on replay — the XI2
        // fanout below is skipped on replay, so there is no XI2 form
        // to dedup against.
        if !is_replay {
            let xi2_evt = xi2_evtype(event.kind);
            if xi2_evt != 0 {
                // Dedup against the SAME window the core form was just
                // delivered to. For crossings that is the per-window
                // `cross_start` (the XI2 fanout below also resolves
                // crossings to that window); using the live deepest hit
                // here would compute the dedup set for the wrong window and
                // leak a double (core + XI2) crossing to a client selecting
                // both on the window being left/entered.
                let (dedup_target, dedup_top_level) = if is_crossing {
                    (cross_start, state.top_level_for_target(cross_start))
                } else {
                    (target, top_level_id)
                };
                let xi2_dedup = compute_xi2_targets_for_source(
                    state,
                    dedup_target,
                    dedup_top_level,
                    xi2_evt,
                    xi_source.slave_deviceid,
                    None,
                );
                // Only the MASTER-pointer XI2 form duplicates the core
                // event, so shadow core only for those clients (Chromium's
                // Ozone X11 selects core + XI2 on the master → it must NOT
                // get both). A client whose XI2 selection is a specific
                // SLAVE device receives a distinct per-device event (Xorg
                // delivers both core AND the slave XI2), so it must KEEP
                // the core form. Enlightenment selects core ButtonPress on
                // its canvas AND XI2 on the slave pointer; deduping it out
                // of core left only the slave event (EFL routes that to its
                // multi/touch handler as button 0), so no click ever
                // registered on the e desktop.
                core_targets.retain(|c| {
                    !xi2_dedup.contains(c)
                        || xi_source.slave_deviceid.is_some_and(|slave_deviceid| {
                            xi2_stamp_deviceid_for_source(
                                state,
                                *c,
                                dedup_target,
                                dedup_top_level,
                                xi2_evt,
                                Some(slave_deviceid),
                            ) == slave_deviceid
                        })
                });
            }
        }

        if matches!(
            event.kind,
            PointerEventKind::ButtonPress | PointerEventKind::ButtonRelease
        ) {
            log::debug!(
                "pointer_fanout: kind={:?} button={} host_xid=0x{:x} top_level=0x{:x} target=0x{:x} \
                 propagation_window=0x{:x} child=0x{:x} core_targets={:?} root=({},{}) event_xy=({},{})",
                event.kind,
                event.detail,
                event.host_xid,
                top_level_id.0,
                target.0,
                nested_id.0,
                propagation_child.0,
                core_targets.iter().map(|c| c.0).collect::<Vec<_>>(),
                event.root_x,
                event.root_y,
                event_x,
                event_y,
            );
        }

        let focus = state.crossing_has_focus(nested_id);

        let extras = fanout_core_pointer_event_ordered(
            state,
            &mut deferred_core_events,
            &core_targets,
            nested_id,
            propagation_child,
            event,
            event_x,
            event_y,
            focus,
            xi_source,
            target,
            top_level_id,
            is_replay,
        );
        // Capture the core candidate. The final resolver compares it with
        // XI2 using Xorg's deepest-window, XI2-before-core ordering.
        // Window owner first = Xorg's DeliverToWindowOwner order; the
        // order among other same-window subscribers is "expressly
        // arbitrary" in Xorg too (events.c:2296). Dropped recipients
        // (write failed) are excluded.
        if event.kind == PointerEventKind::ButtonPress && info.core_press.is_none() {
            let owner = state
                .resources
                .window_owner(nested_id)
                .filter(|o| core_targets.contains(o) && !extras.contains(o))
                .or_else(|| core_targets.iter().find(|c| !extras.contains(c)).copied());
            if let Some(owner) = owner {
                let core_mask = state
                    .clients
                    .get(&owner.0)
                    .and_then(|c| c.event_masks.get(&nested_id).copied())
                    .unwrap_or(0);
                info.core_press = Some(DeliveredPress {
                    owner,
                    window: nested_id,
                    via_xi2: false,
                    core_mask,
                    xi2_mask: 0,
                });
            }
        }
        merge_dropped(&mut dropped, extras);
    }

    // ── XI2 fanout ──────────────────────────────────────────────────
    //
    // Skip on replay: XI2 was already fanned out on the original
    // libinput-driven invocation. Re-running here would deliver a second
    // XI2 ButtonPress for the same physical click and confuse GTK gesture
    // controllers (see is_replay rationale on the public fn).
    if is_replay {
        merge_dropped(
            &mut dropped,
            flush_deferred_core_pointer_events(state, deferred_core_events),
        );
        return dropped;
    }

    // ── XI 1.x device-event fanout ──────────────────────────────────
    //
    // Legacy XInput events (DeviceButtonPress/Release,
    // DeviceMotionNotify) for clients that selected the matching
    // `XEventClass` via SelectExtensionEvent. XI1 events propagate up
    // the ancestor chain like core events (Xorg dix
    // DeliverDeviceEvents) and report the resolved source facet (masters
    // are not XI1-openable).
    //
    // Deliberately BEFORE the `top_level_id_opt` gate below: XI1
    // routing resolves its own target from `natural_target` and its
    // own grab state, so a cursor over the bare root (host_xid not in
    // xid_map — no top-level under it) must still route. The XTS
    // AllowDeviceEvents probes are exactly that shape: the fake device
    // motion parks the cursor at x=0 off every window, and the grab
    // owner still expects its DeviceMotionNotify.
    let xi1_offset = match event.kind {
        PointerEventKind::ButtonPress => Some(crate::xinput::XI_DEVICE_BUTTON_PRESS_OFFSET),
        PointerEventKind::ButtonRelease => Some(crate::xinput::XI_DEVICE_BUTTON_RELEASE_OFFSET),
        PointerEventKind::MotionNotify => Some(crate::xinput::XI_DEVICE_MOTION_NOTIFY_OFFSET),
        _ => None,
    };
    let active_source_grab = active_grab_for_source(state, xi_source);
    let xi2_grab_delivery = handle_grabs
        && event.kind == PointerEventKind::ButtonPress
        && active_source_grab.is_some_and(|grab| grab.passive)
        && state
            .xi1_frozen
            .get(&source_device_id)
            .is_some_and(|freeze| freeze.stored.is_some());
    let owner_events_slave_route = if xi2_grab_delivery
        && let Some(grab) = grab_for_form(state, xi_source, Xi2PointerForm::Slave, replay)
        && grab.passive
    {
        // ActivatePassiveGrab delivers the initial event on the grab window,
        // independent of owner_events; later events use the normal resolver.
        Some((grab, OwnerEventsRoute::Fallback))
    } else if !xi2_grab_delivery
        && !matches!(
            event.kind,
            PointerEventKind::EnterNotify | PointerEventKind::LeaveNotify
        )
        && let Some(grab) = grab_for_form(state, xi_source, Xi2PointerForm::Slave, replay)
        && let Some((_, grab_client, _, _, owner_events, _, _)) =
            active_pointer_grab_target(state, grab)
    {
        let xi1_class = xi_source
            .slave_deviceid
            .zip(xi1_offset)
            .map(|(device, offset)| {
                (u32::from(device) << 8) | u32::from(crate::server::XI_FIRST_EVENT + offset)
            });
        let route = if owner_events {
            owner_events_route(
                state,
                Xi2PointerForm::Slave,
                xi_source.slave_deviceid,
                grab_client,
                target,
                target_x,
                target_y,
                pointer_mask_bit(event.kind, event.state),
                Some(xi2_evtype(event.kind)),
                xi1_class,
            )
        } else {
            OwnerEventsRoute::Fallback
        };
        Some((grab, route))
    } else {
        None
    };
    // Xorg runs the slave's pass before the master's (`../xserver/mi/mieq.c:
    // 504-508`), and only the master's pass moves the sprite. A slave's
    // motion is therefore delivered to the window the sprite was in BEFORE
    // the move; the master's copy uses the new one (measured on Xvfb 21.1.24
    // with XTEST device 4: slave selectors get motion leaving their window,
    // master selectors the motion entering it). Replayed and flushed events
    // have no earlier sprite position and keep the live target.
    // One rule for the XI1 and XI2 slave motion forms.
    let slave_pass_target = if event.kind == PointerEventKind::MotionNotify
        && replay.is_none()
        && !replaying
        && floating_device.is_none()
        && xi_source.slave_deviceid.is_some_and(|device| {
            !state.xi2_pointer_grabs.contains_key(&device)
                && !state.xi1_active_grabs.contains_key(&device)
        }) {
        state
            .root_pointer_target_at(sprite_before.0, sprite_before.1)
            .map_or(target, |(window, _, _)| window)
    } else {
        target
    };
    let mut slave_press_delivery = None;
    if let Some(offset) = xi1_offset
        && replay_has_slave_form
        && button_transition.source_accepted
        && let Some(deviceid) = xi_source.slave_deviceid
    {
        let evcode = crate::server::XI_FIRST_EVENT + offset;
        let detail = if event.kind == PointerEventKind::MotionNotify {
            0
        } else {
            event.detail
        };
        let q = crate::server::Xi1QueuedEvent {
            deviceid,
            evcode,
            detail,
            synthetic_repeat: false,
            time: event.time,
            root_x: event.root_x,
            root_y: event.root_y,
            state_mask: event.state,
            natural_target: slave_pass_target,
            // Pointer devices have no focus class — always the
            // plain selection walk (Xorg ProcessOtherEvent only
            // routes keyboard events through DeliverFocusedEvent).
            focus_route: crate::server::Xi1FocusRoute::Walk,
            axes: event.device_axes,
            replay_floor: None,
        };
        if event.kind == PointerEventKind::ButtonPress
            && state.xi_devices.role(deviceid) == Some(crate::xinput::XiDeviceRole::SlavePointer)
            && !state.xi1_active_grabs.contains_key(&deviceid)
            && !state.xi2_pointer_grabs.contains_key(&deviceid)
        {
            slave_press_delivery = Some(resolve_slave_press_delivery(
                state, target, deviceid, q.evcode,
            ));
        }
        let extras = xi1_route_device_event(state, q, true, slave_press_delivery.as_ref());
        merge_dropped(&mut dropped, extras);
    }

    let Some(top_level_id) = top_level_id_opt else {
        log::debug!(
            "pointer_fanout: kind={:?} host_xid=0x{:x} not in xid_map — XI2 fanout skipped",
            event.kind,
            event.host_xid,
        );
        merge_dropped(
            &mut dropped,
            flush_deferred_core_pointer_events(state, deferred_core_events),
        );
        return dropped;
    };
    let xi2_evtype = xi2_evtype(event.kind);
    let xi2_raw_evtype = xi2_raw_evtype(event.kind);
    let xi2_motion_valuators = event
        .device_axes
        .map(|axes| xtest_wire_valuators(axes, Some((event.root_x, event.root_y))));

    // Start with the hit target. Normal XI2 device forms resolve their
    // first selected ancestor below; grabs and crossings override it.
    let (mut event_x, mut event_y) = (target_x, target_y);
    let mut nested_id = target;
    // XI2 crossings carry a *per-window* endpoint, not a single hit. The
    // producer (update_pointer_window → normal_mode_crossings) emits one
    // Enter/Leave per window on the sprite-trace path, each tagged with
    // that window's host_xid + detail (Ancestor/Virtual/Inferior). The
    // generic resolution above collapses every chain event onto the
    // deepest hit (`target`), so a client that selected XI_Enter on an
    // *intermediate* ancestor never matches — XI2 crossings are strictly
    // per-window with no upward propagation. That is exactly muffin's
    // focus-follows-mouse: it selects XI_Enter on the managed client
    // window, which for an app with its own content child (wezterm) sits
    // *between* the leaf the pointer is over and the top-level frame, so
    // the crossing fell through the {leaf, top-level} crack and wezterm
    // never focused on hover (Caja, whose content is the managed window,
    // worked). Resolve crossings against the producer's own window
    // (host_xid → resource) — matching Xorg, which delivers a crossing to
    // every path window that selected it. Coalesce-safe: for the common
    // case the producer's window IS the deepest hit, so this is a no-op.
    // Clients reached only through the slave's earlier window.
    let mut xi2_slave_only_targets: Vec<ClientId> = Vec::new();
    let mut xi2_targets = if matches!(
        event.kind,
        PointerEventKind::EnterNotify | PointerEventKind::LeaveNotify
    ) && let Some(crossing_win) = xid_map.get(&event.host_xid).copied()
    {
        nested_id = crossing_win;
        // Compute per-window coordinates from root + core geometry. The KMS
        // producer cannot derive local coordinates for nested client windows
        // from its top-level scene geometry and used to stamp root coords on
        // those chain entries (visible in Steam as event_x == root_x).
        let (ox, oy) = state.resources.window_absolute_position(crossing_win);
        event_x = clamp_grab_coord(event.root_x, ox);
        event_y = clamp_grab_coord(event.root_y, oy);
        xi2_crossing_targets_for_source(state, crossing_win, xi2_evtype, xi_source)
    } else {
        let mut targets = compute_xi2_targets_for_source(
            state,
            target,
            top_level_id,
            xi2_evtype,
            xi_source.slave_deviceid,
            slave_press_delivery.as_ref(),
        );
        if slave_pass_target != target {
            // The slave's form is routed from the sprite's previous window.
            let earlier = compute_xi2_targets_for_source(
                state,
                slave_pass_target,
                state.top_level_for_target(slave_pass_target),
                xi2_evtype,
                xi_source.slave_deviceid,
                slave_press_delivery.as_ref(),
            );
            xi2_slave_only_targets = earlier
                .iter()
                .filter(|client| !targets.contains(client))
                .copied()
                .collect();
            merge_dropped(&mut targets, earlier);
        }
        targets
    };
    let (xi2_raw_slave_targets, xi2_raw_master_targets) = xi2_raw_evtype
        .map(|raw_evtype| compute_xi2_raw_targets(state, raw_evtype, xi_source))
        .unwrap_or_default();
    let mut xi2_raw_targets = xi2_raw_slave_targets.clone();
    merge_dropped(&mut xi2_raw_targets, xi2_raw_master_targets.clone());

    // Set when delivery is grab-scoped (passive-freeze or active-grab
    // redirect): the event window is then the grab window, not the
    // recipient's own selection, so per-recipient propagation resolution
    // (issue #94) is skipped for those paths.
    // Xorg `DeliverGrabbedEvent` grab-window fallback for a single owner:
    // (grab_client, grab_window, event_x, event_y). Set when owner_events
    // natural delivery does NOT reach the grab owner, so it must still get
    // the event on the grab window while other natural recipients keep
    // their own windows. (#94 follow-up — grabbed ButtonRelease.)
    let mut xi2_grab_window_target: Option<(ClientId, ResourceId, i16, i16)> = None;
    // Master-form grabs decide the master pass only. Slave selectors keep
    // their independent delivery unless an exact slave grab applies.
    let mut owner_events_master_route: Option<(
        crate::server::ActivePointerGrab,
        OwnerEventsRoute,
    )> = None;
    let mut xi2_slave_grab_window_target: Option<(ClientId, ResourceId, i16, i16)> = None;

    // Physical wheel input arrives as button 4–7 transitions, then this
    // fanout adds the XI2 Motion carrying the scroll valuator. Xorg turns a
    // scroll ButtonPress into MotionNotify and fills it before separately
    // generating legacy wheel buttons (dix/getevents.c:1646-1697, 1703-1718),
    // so a Motion-only XI2 grab must accept the emulated wheel press as Motion.
    let xi2_scroll_button_press = event.kind == PointerEventKind::ButtonPress
        && button_transition.source_accepted
        && source_has_scroll_class;
    let xi2_grab_accepts_event = |mask: u64| {
        mask & (1_u64 << xi2_evtype) != 0 || (xi2_scroll_button_press && mask & (1_u64 << 6) != 0)
    };
    let mut xi2_scroll_master_grab_motion_only = false;
    let mut xi2_scroll_slave_grab_motion_only = false;

    // Synchronous passive XI2 button grabs freeze the device event at
    // the grab owner until XIAllowEvents(ReplayDevice) replays it.
    // Without this filter GTK sees the press on the unfocused target
    // before muffin finishes focus activation, then never receives the
    // replay it expects.
    if xi2_grab_delivery && let Some(grab_owner) = active_source_grab.map(|grab| grab.owner) {
        xi2_targets.retain(|cid| *cid == grab_owner);
    }

    // Decide the master form across XI2, XI1 and core selections. The slave
    // form remains independent of a master grab, like Xorg's per-device
    // delivery passes (`../xserver/mi/mieq.c:484-508`).
    if !xi2_grab_delivery
        && !matches!(
            event.kind,
            PointerEventKind::EnterNotify | PointerEventKind::LeaveNotify
        )
        && let Some(grab) = grab_for_form(state, xi_source, Xi2PointerForm::Master, replay)
        && let Some((grab_window, grab_client, gx, gy, owner_events, via_xi2, _)) =
            active_pointer_grab_target(state, grab)
    {
        let route = if owner_events {
            owner_events_route(
                state,
                Xi2PointerForm::Master,
                xi_source.slave_deviceid,
                grab_client,
                target,
                target_x,
                target_y,
                pointer_mask_bit(event.kind, event.state),
                Some(xi2_evtype),
                None,
            )
        } else {
            OwnerEventsRoute::Fallback
        };
        owner_events_master_route = Some((grab, route));
        match route {
            OwnerEventsRoute::Natural {
                protocol: OwnerProtocol::Xi2,
                ..
            } => {
                if !xi2_targets.contains(&grab_client) {
                    xi2_targets.push(grab_client);
                }
            }
            OwnerEventsRoute::Fallback if via_xi2 && xi2_grab_accepts_event(grab.xi2_mask) => {
                if !xi2_targets.contains(&grab_client) {
                    xi2_targets.push(grab_client);
                }
                xi2_scroll_master_grab_motion_only =
                    xi2_scroll_button_press && grab.xi2_mask & (1_u64 << xi2_evtype) == 0;
                xi2_grab_window_target = Some((
                    grab_client,
                    grab_window,
                    clamp_grab_coord(event.root_x, gx),
                    clamp_grab_coord(event.root_y, gy),
                ));
            }
            _ => {}
        }
    }

    // Resolve the slave pass independently. A grab on this source redirects
    // only its slave form; an attached master copy still follows its own
    // selection or master grab (`../xserver/mi/mieq.c:484-508`).
    if let Some((grab, route)) = owner_events_slave_route {
        match route {
            OwnerEventsRoute::Natural {
                protocol: OwnerProtocol::Xi2,
                ..
            } => {
                if !xi2_targets.contains(&grab.owner) {
                    xi2_targets.push(grab.owner);
                }
            }
            OwnerEventsRoute::Fallback if grab.via_xi2 && xi2_grab_accepts_event(grab.xi2_mask) => {
                if !xi2_targets.contains(&grab.owner) {
                    xi2_targets.push(grab.owner);
                }
                let (grab_window, grab_client, gx, gy, ..) =
                    active_pointer_grab_target(state, grab)
                        .expect("resolved slave grab keeps its owner and window");
                xi2_slave_grab_window_target = Some((
                    grab_client,
                    grab_window,
                    clamp_grab_coord(event.root_x, gx),
                    clamp_grab_coord(event.root_y, gy),
                ));
                xi2_scroll_slave_grab_motion_only =
                    xi2_scroll_button_press && grab.xi2_mask & (1_u64 << xi2_evtype) == 0;
            }
            _ => {}
        }
    }

    if matches!(
        event.kind,
        PointerEventKind::ButtonPress | PointerEventKind::ButtonRelease
    ) {
        log::debug!(
            "pointer_fanout XI2: kind={:?} button={} time={} target=0x{:x} top_level=0x{:x} \
             xi2_targets={:?} xi2_raw_targets={:?} root=({},{}) event_xy=({},{}) state=0x{:x}",
            event.kind,
            event.detail,
            event.time,
            target.0,
            top_level_id.0,
            xi2_targets.iter().map(|c| c.0).collect::<Vec<_>>(),
            xi2_raw_targets.iter().map(|c| c.0).collect::<Vec<_>>(),
            event.root_x,
            event.root_y,
            event_x,
            event_y,
            event.state,
        );
    }

    // XI2 raw events.
    // None for a warp ([`warp_pointer_event_fanout_to_state`]).
    if !suppress_raw
        && !warp
        && let Some(raw_evtype) = xi2_raw_evtype
    {
        // mieq processes the attached slave first, then copies ET_Raw* to its
        // master by rewriting deviceid and retaining sourceid. Keep that
        // order for clients selecting both forms. Master-only origins have
        // no slave event to copy.
        if let Some(deviceid) = xi_source.slave_deviceid {
            let extras =
                fanout_event_to_clients(state, &xi2_raw_slave_targets, |buf, seq, order| {
                    x11::encode_xi2_raw_event(
                        buf,
                        order,
                        seq,
                        XI2_MAJOR_OPCODE,
                        raw_evtype,
                        deviceid,
                        event.time,
                        u32::from(event.detail),
                        xi_source.sourceid,
                        event.raw_dx,
                        event.raw_dy,
                    );
                });
            merge_dropped(&mut dropped, extras);
        }
        let extras = fanout_event_to_clients(state, &xi2_raw_master_targets, |buf, seq, order| {
            x11::encode_xi2_raw_event(
                buf,
                order,
                seq,
                XI2_MAJOR_OPCODE,
                raw_evtype,
                XI2_MASTER_POINTER_DEVICE_ID,
                event.time,
                u32::from(event.detail),
                xi_source.sourceid,
                event.raw_dx,
                event.raw_dy,
            );
        });
        merge_dropped(&mut dropped, extras);
    }

    // If this is a wheel button press (4 = up, 5 = down, 6 = left,
    // 7 = right), prepend an XI_Motion event carrying the scroll-axis
    // valuator update before the XI_ButtonPress. The Motion's axis
    // value is the **CUMULATIVE** scroll counter, not the per-event
    // delta — XI2 §11.5: clients compute the delta as
    // `(current - previous) / increment`. The previous-value
    // baseline comes from `XIQueryDevice` (the valuator class
    // declares the current position) and `DeviceChanged` events as
    // clients connect mid-session.
    //
    // A pre-2026-05-29 yserver sent `delta` (±1) here. GDK reads the
    // axisvalue, subtracts its cached previous (which after the first
    // scroll is also 1), and gets 0 — no scroll. The first scroll
    // worked (1 - 0 = 1), every subsequent scroll on the same client
    // got stuck. This bug went unnoticed because GDK was falling back
    // to the legacy XI_ButtonPress(4..7) emulation (XIPointerEmulated
    // flag wasn't being set, so GDK accepted those buttons as scroll).
    // The XI_POINTER_EMULATED fix (Chrome scroll-crash repair) made
    // GDK correctly skip the emulated buttons, exposing the latent
    // cumulative-vs-delta bug.
    //
    // Xorg builds smooth-scroll Motion before its separate legacy wheel
    // transitions (dix/getevents.c:1646-1697, 1703-1718). Resolve each device
    // form through the same grab/owner-events decision used by scroll-stop.
    if let Some((axis, value)) = scroll_motion {
        // A cooked scroll Motion is independent of whether its legacy wheel
        // button changes the aggregate master bitmap. It does, however,
        // follow the frozen device's processing boundary. A queued master
        // Motion remains accepted even if the separate legacy button
        // transition is rejected at thaw.
        let scroll_master_accepted = if deferred_master {
            replay_has_master_form
        } else {
            !master_part_deferred && !passive_slave_activation && replay_has_master_form
        };
        for route in xi2_motion_targets_for_source(
            state,
            xi_source,
            target,
            nested_id,
            target_x,
            target_y,
            event.root_x,
            event.root_y,
            button_transition.source_accepted,
            scroll_master_accepted,
            replay,
        ) {
            let extras = fanout_event_to_clients(
                state,
                std::slice::from_ref(&route.client),
                |buf, seq, order| {
                    x11::encode_xi2_motion_with_scroll(
                        buf,
                        order,
                        seq,
                        XI2_MAJOR_OPCODE,
                        route.deviceid,
                        event.time,
                        ROOT_WINDOW,
                        route.window,
                        route.child,
                        event.root_x,
                        event.root_y,
                        route.x,
                        route.y,
                        event.state,
                        xi_source.sourceid,
                        axis,
                        value,
                    );
                },
            );
            merge_dropped(&mut dropped, extras);
        }
    }

    // XI2 device events (crossing or non-crossing).
    //
    // Per-client `deviceid`: a client that selected XI2 events on the
    // *slave* pointer device must receive the event stamped with the
    // slave's deviceid; clients selecting the master (or the
    // XIAllMasterDevices/XIAllDevices wildcards) get the master's
    // deviceid. This mirrors Xorg's sprite-delivery: the event's
    // `deviceid` is the device the receiving client selected on, while
    // `sourceid` is always the originating slave.
    //
    // Without this, a slave-device selector (Enlightenment selects XI2
    // ButtonPress per physical slave pointer) received events stamped
    // with the master deviceid it never selected, and its per-device
    // dispatch discarded them — every click on the e desktop did
    // nothing while keyboard (master-routed) worked.
    // A client can receive an event in BOTH the master-stamped and
    // slave-stamped form, exactly as Xorg does: the master form if it
    // selected under the concrete master pointer / `XIAllMasterDevices(1)`
    // / `XIAllDevices(0)`, and the slave form if it selected under the
    // concrete slave pointer / `XIAllDevices(0)`. `XIAllDevices(0)`
    // selectors (SDL3's idiom) get BOTH — SDL reads smooth scroll ONLY
    // off the slave-stamped motion (its handler gates on
    // `deviceid == sourceid`), while cursor position + GDK-style clients
    // use the master form. Delivering only the master form (the pre-fix
    // behaviour) left every SDL3 app unable to scroll (issue #72): the
    // scroll valuator rode a master-stamped motion SDL3 never parses.
    // XI2 device forms propagate leaf-to-root (Xorg `DeliverDeviceEvents`),
    // stopping globally at the first window with any matching selector.
    // Every selector on that window receives the form, with `child` pointing
    // toward the hit and coordinates relative to the selected window.
    // Stamping the raw hit target
    // uniformly (the old behaviour) handed clients a window they neither own
    // nor selected on — Chromium/CEF's Ozone X11 layer then called
    // `GetWindowFromXID()` on the foreign XID, got `nullptr`, and
    // dereferenced it on the slave-device button path (Steam Library-tab
    // crash, issue #94). Grab-redirected and crossing delivery keep their
    // grab/producer window (resolved above); everything else propagates.
    //
    // Emit the SLAVE-stamped form BEFORE the master-stamped form, PER
    // CONNECTION, matching Xorg's `mieqProcessDeviceEvent` (mi/mieq.c:
    // "process slave first, then master"). This ordering is load-bearing for
    // clients that both (a) select `XIAllDevices(0)` — so they receive BOTH
    // forms — and (b) compress consecutive XI_Motion keeping only the last
    // while dropping the slave-deviceid copy (Qt/Telegram:
    // `qxcbconnection.cpp` + `qxcbconnection_xi2.cpp`). With master-first the
    // master copy is trailed by its slave copy → compressed away → surviving
    // slave copy dropped → ZERO motion/smooth-scroll reaches the widget.
    // Slave-first leaves the master copy — the one Qt keeps — trailing, so it
    // survives. Verified against mate.xtrace vs mate-xorg.xtrace 2026-07-10.
    let is_crossing_evt = matches!(
        event.kind,
        PointerEventKind::EnterNotify | PointerEventKind::LeaveNotify
    );
    for cid in &xi2_targets {
        let (wants_master, mut wants_slave) = xi2_pointer_forms_for_source(
            state,
            *cid,
            target,
            top_level_id,
            xi2_evtype,
            xi_source.slave_deviceid,
            slave_press_delivery.as_ref(),
        );
        let wants_slave_at_target = wants_slave;
        if slave_pass_target != target {
            wants_slave = xi2_pointer_forms_for_source(
                state,
                *cid,
                slave_pass_target,
                top_level_id,
                xi2_evtype,
                xi_source.slave_deviceid,
                slave_press_delivery.as_ref(),
            )
            .1;
        }
        // An exact slave grab uses the grab's XI2 mask and device identity
        // for grab delivery. There need not be a separate XISelectEvents
        // selection on that device, and a detached source has no master
        // cooked-event form to fall back to.
        let exact_slave_grab = xi2_grab_delivery
            && active_source_grab.is_some_and(|grab| grab.owner == *cid && grab.via_xi2)
            && xi_source.slave_deviceid.is_some_and(|device_id| {
                state.xi2_pointer_grabs.get(&device_id).is_some_and(|grab| {
                    grab.owner == *cid && grab.via_xi2 && xi2_grab_accepts_event(grab.xi2_mask)
                })
            });
        if exact_slave_grab {
            wants_slave = true;
        }
        let wants_slave_at_target = wants_slave_at_target || exact_slave_grab;
        let wants_slave_form = if let Some((grab, route)) = owner_events_slave_route {
            match route {
                OwnerEventsRoute::Natural {
                    protocol: OwnerProtocol::Xi2,
                    ..
                } => grab.owner == *cid,
                OwnerEventsRoute::Fallback => {
                    grab.owner == *cid && grab.via_xi2 && xi2_grab_accepts_event(grab.xi2_mask)
                }
                OwnerEventsRoute::Natural { .. } => false,
            }
        } else {
            wants_slave
        };
        let wants_master_form = if let Some((grab, route)) = owner_events_master_route {
            match route {
                OwnerEventsRoute::Natural {
                    protocol: OwnerProtocol::Xi2,
                    ..
                } => grab.owner == *cid,
                OwnerEventsRoute::Fallback => {
                    grab.owner == *cid && grab.via_xi2 && xi2_grab_accepts_event(grab.xi2_mask)
                }
                OwnerEventsRoute::Natural { .. } => false,
            }
        } else {
            if owner_events_slave_route.is_some_and(|(grab, _)| grab.owner == *cid) {
                wants_master
            } else {
                wants_master || !wants_slave_at_target
            }
        } && (!xi2_slave_only_targets.contains(cid)
            || owner_events_master_route.is_some_and(|(grab, _)| grab.owner == *cid));
        let mut forms = Vec::with_capacity(2);
        if is_crossing_evt {
            // A crossing exists in exactly one form (`xi2_crossing_form`):
            // an attached slave never gets its own Enter/Leave, so a client
            // selecting XIAllDevices sees one crossing, not a second,
            // unbalanced per-slave one, like Xorg.
            let (form, deviceid) = xi2_crossing_form(xi_source);
            forms.push((deviceid, true, form));
        } else if let Some(slave_deviceid) = xi_source.slave_deviceid
            && replay_has_slave_form
        {
            forms.push((
                slave_deviceid,
                wants_slave_form && button_transition.source_accepted,
                Xi2PointerForm::Slave,
            ));
        }
        // Master form for master / `XIAllMasterDevices` / `XIAllDevices`
        // selectors, AND as the default when the client has no matching
        // window selection (grab owners receive via their grab mask, not
        // `XISelectEvents`, and that funnel is master-routed). A source with
        // no published slave can only deliver its master form.
        if !is_crossing_evt
            && replay_has_master_form
            && (replay.is_some()
                || xi_source.slave_deviceid.is_none()
                || xi_source.attached_master == Some(XI2_MASTER_POINTER_DEVICE_ID))
        {
            forms.push((
                XI2_MASTER_POINTER_DEVICE_ID,
                wants_master_form && button_transition.master_accepted,
                Xi2PointerForm::Master,
            ));
        }
        for (deviceid, button_want, form) in forms {
            if form == Xi2PointerForm::Master {
                merge_dropped(
                    &mut dropped,
                    flush_deferred_core_pointer_events_for(state, &mut deferred_core_events, *cid),
                );
            }
            if !button_want {
                continue;
            }
            // Master-form routing can be grab-scoped while the same source's
            // slave form remains natural. Keep the fixed geometry local to
            // the form that made the routing decision.
            let fixed_geometry = if form == Xi2PointerForm::Slave
                && let Some((grab, route)) = owner_events_slave_route
                && grab.owner == *cid
            {
                match route {
                    OwnerEventsRoute::Natural {
                        window,
                        x,
                        y,
                        child,
                        protocol: OwnerProtocol::Xi2,
                    } => Some((window, child, x, y)),
                    OwnerEventsRoute::Fallback => xi2_slave_grab_window_target
                        .filter(|(owner, ..)| owner == cid)
                        .map(|(_, window, x, y)| {
                            let child = xi2_child_toward(state, window, target);
                            (window, child, x, y)
                        }),
                    OwnerEventsRoute::Natural { .. } => None,
                }
            } else if form == Xi2PointerForm::Master
                && let Some((_, window, x, y)) =
                    xi2_grab_window_target.filter(|(owner, ..)| owner == cid)
            {
                let child = xi2_child_toward(state, window, target);
                Some((window, child, x, y))
            } else if form == Xi2PointerForm::Master
                && let Some((
                    grab,
                    OwnerEventsRoute::Natural {
                        window,
                        x,
                        y,
                        child,
                        protocol: OwnerProtocol::Xi2,
                    },
                )) = owner_events_master_route
                && grab.owner == *cid
            {
                Some((window, child, x, y))
            } else if is_crossing_evt || (xi2_grab_delivery && form == Xi2PointerForm::Slave) {
                Some((nested_id, ResourceId(0), event_x, event_y))
            } else {
                None
            };
            let (ev_win, ev_child, ev_x, ev_y) = fixed_geometry.unwrap_or_else(|| {
                let win = if form == Xi2PointerForm::Slave {
                    match slave_press_delivery.as_ref() {
                        Some(SlavePressDelivery::Xi2 { window, recipients })
                            if recipients.contains(cid) =>
                        {
                            *window
                        }
                        Some(_) => target,
                        None => xi2_route_window_for_source(
                            state,
                            slave_pass_target,
                            form,
                            xi2_evtype,
                            xi_source.slave_deviceid,
                        )
                        .unwrap_or(target),
                    }
                } else {
                    xi2_route_window_for_source(
                        state,
                        target,
                        form,
                        xi2_evtype,
                        xi_source.slave_deviceid,
                    )
                    .unwrap_or(target)
                };
                let child_target = if form == Xi2PointerForm::Slave {
                    slave_pass_target
                } else {
                    target
                };
                // Xorg's FindChildForEvent reports the sprite path's child
                // for XI2 fixup (21.1.24 dix/events.c:2598-2619, 2623-2642).
                // The slave pass uses the prior sprite target even when the
                // event window is also the post-move target.
                let child = xi2_child_toward(state, win, child_target);
                if win == nested_id {
                    let child = if form == Xi2PointerForm::Slave {
                        child
                    } else {
                        // The master form's shortcut has nested_id == target.
                        ResourceId(0)
                    };
                    (nested_id, child, event_x, event_y)
                } else {
                    let (ax, ay) = state.resources.window_absolute_position(win);
                    (
                        win,
                        child,
                        clamp_grab_coord(event.root_x, ax),
                        clamp_grab_coord(event.root_y, ay),
                    )
                }
            });
            let focus = state.crossing_has_focus(ev_win);
            // Xorg snapshots the slave's `axisVal` into DeviceChanged
            // (dix/getevents.c:265-286), then copies the slave event to its
            // master (mi/mieq.c:422-425). Keep both XI forms on that same
            // source counter so a source switch cannot create a false delta.
            let scroll_motion_only_grab = match form {
                Xi2PointerForm::Slave => xi2_scroll_slave_grab_motion_only,
                Xi2PointerForm::Master => xi2_scroll_master_grab_motion_only,
            } && grab_for_form(state, xi_source, form, replay)
                .is_some_and(|grab| grab.owner == *cid);
            let extras =
                fanout_event_to_clients(state, std::slice::from_ref(cid), |buf, seq, order| {
                    if is_crossing_evt {
                        x11::encode_xi2_crossing_event(
                            buf,
                            order,
                            seq,
                            XI2_MAJOR_OPCODE,
                            xi2_evtype,
                            deviceid,
                            event.time,
                            ROOT_WINDOW,
                            ev_win,
                            event.root_x,
                            event.root_y,
                            ev_x,
                            ev_y,
                            event.state,
                            event.crossing_mode,
                            event.detail,
                            xi_source.sourceid,
                            focus,
                        );
                    } else {
                        // Mark scroll-emulated XI_ButtonPress/Release(4..7)
                        // with XIPointerEmulated so XI2-aware clients discard
                        // the legacy button after consuming the matching
                        // XI_Motion scroll-axis update (see
                        // `yserver-protocol::x11::XI_POINTER_EMULATED`).
                        let xi2_flags: u32 = if matches!(
                            event.kind,
                            PointerEventKind::ButtonPress | PointerEventKind::ButtonRelease
                        ) && source_has_scroll_class
                        {
                            x11::XI_POINTER_EMULATED
                        } else {
                            0
                        };
                        if button_want && !scroll_motion_only_grab {
                            x11::encode_xi2_device_event(
                                buf,
                                order,
                                seq,
                                XI2_MAJOR_OPCODE,
                                xi2_evtype,
                                deviceid,
                                event.time,
                                ROOT_WINDOW,
                                ev_win,
                                ev_child,
                                event.root_x,
                                event.root_y,
                                ev_x,
                                ev_y,
                                event.state,
                                u32::from(event.detail),
                                xi_source.sourceid,
                                xi2_flags,
                                xi2_motion_valuators
                                    .as_ref()
                                    .map(|(first, values)| (*first, values.as_slice())),
                            );
                        }
                    }
                });
            let delivered = !extras.contains(cid);
            merge_dropped(&mut dropped, extras);
            if event.kind == PointerEventKind::ButtonPress
                && button_want
                && delivered
                && grab_for_form(state, xi_source, form, replay).is_none()
            {
                // Under a grab this records the grab owner, and the
                // lifecycle's no-grab gate discards it. Natural delivery is
                // the only path that can install. Snapshot the selection for
                // this form's device, as Xorg xi2mask_isset does.
                info.consider_xi2_press(
                    &state.resources,
                    form,
                    DeliveredPress {
                        owner: *cid,
                        window: ev_win,
                        via_xi2: true,
                        core_mask: 0,
                        xi2_mask: xi2_effective_mask(
                            state,
                            ev_win,
                            deviceid,
                            form == Xi2PointerForm::Master,
                        ),
                    },
                );
            }
        }
    }

    merge_dropped(
        &mut dropped,
        flush_deferred_core_pointer_events(state, deferred_core_events),
    );
    dropped
}

/// X11 implicit pointer grab lifecycle (Xorg ActivateImplicitGrab,
/// dix/events.c:2150-2193 + install site :2415-2421; release
/// Xi/exevents.c:1931-1958). Runs AFTER the fanout delivered the event:
/// the activating press is delivered pre-grab (delivery capture, not
/// rerouting) and the final release is delivered UNDER the grab before
/// deactivation — both matching Xorg's ordering.
fn implicit_pointer_grab_lifecycle(
    state: &mut ServerState,
    event: &HostPointerEvent,
    info: &ImplicitGrabFanoutInfo,
    replay: Option<PointerReplay>,
) {
    if info.queued {
        return;
    }
    match event.kind {
        PointerEventKind::ButtonPress => {
            if info.master_button_transition {
                let source_has_non_implicit_xi2_grab = if let Some(replay) = replay {
                    active_grab_for_device(state, replay.device).is_some_and(|grab| !grab.implicit)
                } else {
                    resolve_pointer_xi_source(state, event.origin, event.tree_change)
                        .and_then(|source| source.slave_deviceid)
                        .and_then(|device_id| state.xi2_pointer_grabs.get(&device_id))
                        .is_some_and(|grab| !grab.implicit)
                };
                if state.active_pointer_grab.is_none()
                    && !source_has_non_implicit_xi2_grab
                    && let Some(press) = info.delivered_master_press(&state.resources)
                {
                    // Master implicit grabs come only from core and XI2
                    // master-form deliveries. An XI1 grab keyed by the
                    // source slave occupies its own per-device slot.
                    state.set_pointer_grab(crate::server::ActivePointerGrab {
                        owner: press.owner,
                        grab_window: press.window,
                        event_mask: (press.core_mask & 0xFFFF) as u16,
                        cursor: ResourceId(0),
                        time: event.time,
                        owner_events: !press.via_xi2 && press.core_mask & 0x0100_0000 != 0,
                        via_xi2: press.via_xi2,
                        implicit: true,
                        passive: false,
                        xi2_mask: press.xi2_mask,
                    });
                    // Xorg ActivatePointerGrab updates grabTime on implicit
                    // activation too (dix/events.c:1637): timestamp validation
                    // in GrabPointer/UngrabPointer/AllowEvents must see this click.
                    state.last_pointer_grab_time = event.time;
                }
            }

            let implicit_slave_device = if let Some(replay) = replay {
                (state.xi_devices.role(replay.device)
                    == Some(crate::xinput::XiDeviceRole::SlavePointer))
                .then_some(replay.device)
            } else {
                resolve_pointer_xi_source(state, event.origin, event.tree_change)
                    .and_then(|source| source.slave_deviceid)
            };
            if info.source_button_transition
                && let Some(device_id) = implicit_slave_device
                && !state.xi2_pointer_grabs.contains_key(&device_id)
                && !state.xi1_active_grabs.contains_key(&device_id)
                && let Some(press) = info.xi2_slave_press
            {
                state.xi2_pointer_grabs.insert(
                    device_id,
                    crate::server::ActivePointerGrab {
                        owner: press.owner,
                        grab_window: press.window,
                        event_mask: 0,
                        cursor: ResourceId(0),
                        time: event.time,
                        owner_events: false,
                        via_xi2: true,
                        implicit: true,
                        passive: false,
                        xi2_mask: press.xi2_mask,
                    },
                );
            }
        }
        // Xi/exevents.c:1931: deactivate when no buttons remain down,
        // after the release was delivered under the grab. Bare clear:
        // an implicit grab set no cursor override / confine / freeze /
        // crossing chain, so the explicit-grab teardown helpers
        // (NotifyUngrab chain, freeze-bridge release) must NOT run.
        PointerEventKind::ButtonRelease
            if info.master_button_transition
                && state.buttons_down == 0
                && state.active_pointer_grab.is_some_and(|g| g.implicit) =>
        {
            state.clear_pointer_grab();
        }
        _ => {}
    }
}

/// Emit an XI2 scroll **stop**: a delta-0 `XI_Motion` carrying the current,
/// unchanged scroll valuator for both scroll axes, so GDK's XI2 backend sets
/// `scroll.is_stop = TRUE` — its condition is literally `delta_x == 0.0 &&
/// delta_y == 0.0` on a present scroll valuator (gdkdevicemanager-xi2.c). This
/// is libinput's fingers-lifted signal for two-finger scrolling; Firefox's
/// SwipeTracker uses the stop to *commit* a horizontal-swipe history navigation
/// (bug 1539730) — without it the back/forward arrow tracks but never fires.
///
/// XI2 smooth-scroll selectors only: no core event, no button, and none of the
/// grab / crossing / barrier machinery in [`pointer_event_fanout_to_state`] — a
/// stop carries no position change and must not perturb pointer state. The
/// target window is resolved exactly as a motion would (deepest window under
/// the cursor) so the stop reaches the client already receiving the scroll.
pub fn emit_scroll_stop_to_state(
    state: &mut ServerState,
    xid_map: &HostXidMap,
    origin: crate::core_loop::InputOrigin,
    pointer_host_xid: u32,
    root_x: i16,
    root_y: i16,
    state_mask: u16,
    time: u32,
) {
    let Some(xi_source) = resolve_pointer_xi_source(state, origin, false) else {
        return;
    };
    // The stop is a pointer event for UpdateFromMaster too. This announces
    // a source switch and copies the same source valuators used by ordinary
    // pointer events before the stop's XI2 motion forms are emitted.
    let _dropped = update_from_pointer_master(state, xi_source, true, true);
    let probe = HostPointerEvent {
        origin,
        kind: PointerEventKind::MotionNotify,
        host_xid: pointer_host_xid,
        detail: 0,
        time,
        root_x,
        root_y,
        event_x: root_x,
        event_y: root_y,
        state: state_mask,
        crossing_mode: 0,
        child: 0,
        raw_dx: 0,
        raw_dy: 0,
        tree_change: false,
        device_axes: None,
    };
    let root_hit = resolve_pointer_hit(state, xid_map, &probe);
    let top_level_id = root_hit
        .map(|(t, _, _)| state.top_level_for_target(t))
        .or_else(|| xid_map.get(&pointer_host_xid).copied())
        .unwrap_or(ROOT_WINDOW);
    let (target, event_x, event_y) = root_hit.unwrap_or((top_level_id, root_x, root_y));

    for route in xi2_motion_targets_for_source(
        state, xi_source, target, target, event_x, event_y, root_x, root_y, true, true, None,
    ) {
        let scroll = if route.form == Xi2PointerForm::Slave {
            state
                .xi_devices
                .device(route.deviceid)
                .map_or(state.scroll_axis_value, |device| device.scroll_axis_values)
        } else {
            state.scroll_axis_value
        };
        let _ = fanout_event_to_clients(
            state,
            std::slice::from_ref(&route.client),
            |buf, seq, order| {
                // Current cumulative valuator per axis (unchanged → delta
                // 0 → is_stop). Index 0 is vertical axis 2; index 1 is
                // horizontal axis 3.
                let axes = [(2, scroll[0]), (3, scroll[1])];
                for (axis, value) in axes {
                    x11::encode_xi2_motion_with_scroll(
                        buf,
                        order,
                        seq,
                        XI2_MAJOR_OPCODE,
                        route.deviceid,
                        time,
                        ROOT_WINDOW,
                        route.window,
                        route.child,
                        root_x,
                        root_y,
                        route.x,
                        route.y,
                        state_mask,
                        xi_source.sourceid,
                        axis,
                        value,
                    );
                }
            },
        );
    }
}

/// Resolved natural delivery for one attached slave ButtonPress. Xorg's
/// DeliverDeviceEvents checks XI2 and then XI1 at each window, stopping at
/// the first protocol/window with recipients; the press delivery and XI1
/// implicit-grab activation both consume this result.
#[derive(Debug)]
pub(crate) enum SlavePressDelivery {
    Xi2 {
        window: ResourceId,
        recipients: Vec<ClientId>,
    },
    Xi1 {
        window: ResourceId,
        recipients: Vec<ClientId>,
    },
    None,
}

/// Route one XI 1.x device input event through grab, freeze, and selection
/// semantics. Pointer and key fanouts and the AllowDeviceEvents thaw path
/// share this entry point. Synchronous grabs re-freeze after each key/button
/// event, passive grabs auto-release on their matching release, and a delivered
/// DeviceButtonPress selection creates an implicit grab (Xorg
/// `ActivateImplicitGrab`, `../xserver/dix/events.c:2152-2196`; MPX
/// `a511c445d` postdates XTS XSelectExtensionEvent assertion 5).
pub(crate) fn xi1_route_device_event(
    state: &mut ServerState,
    q: crate::server::Xi1QueuedEvent,
    allow_freeze: bool,
    slave_press_delivery: Option<&SlavePressDelivery>,
) -> Vec<ClientId> {
    use crate::xinput::{
        XI_DEVICE_BUTTON_PRESS_OFFSET, XI_DEVICE_BUTTON_RELEASE_OFFSET, XI_DEVICE_KEY_PRESS_OFFSET,
        XI_DEVICE_KEY_RELEASE_OFFSET,
    };
    let first = crate::server::XI_FIRST_EVENT;
    state.xi1_last_input_time = state.xi1_last_input_time.max(q.time);
    let is_press = q.evcode == first + XI_DEVICE_KEY_PRESS_OFFSET
        || q.evcode == first + XI_DEVICE_BUTTON_PRESS_OFFSET;
    let is_release = q.evcode == first + XI_DEVICE_KEY_RELEASE_OFFSET
        || q.evcode == first + XI_DEVICE_BUTTON_RELEASE_OFFSET;
    let is_button_press = q.evcode == first + XI_DEVICE_BUTTON_PRESS_OFFSET;
    let is_button_release = q.evcode == first + XI_DEVICE_BUTTON_RELEASE_OFFSET;

    // 1. Frozen → queue (Xorg FreezeThaw switching processInputProc
    // to the enqueue proc: NOTHING is delivered while frozen, not
    // even to the grab owner).
    if state
        .xi1_frozen
        .get(&q.deviceid)
        .is_some_and(crate::server::Xi1Freeze::frozen)
    {
        state
            .sync_pending
            .push_back(crate::server::PendingSyncEvent {
                device: q.deviceid,
                event: crate::server::QueuedInputEvent::Xi1Routed(q),
            });
        log::debug!(
            "xi1_route: device {} frozen — queued evcode={} detail={}",
            q.deviceid,
            q.evcode,
            q.detail,
        );
        return Vec::new();
    }

    // Maintain the per-device axis values (Xorg `axisVal`): real
    // motion writes the sprite position into axes 0/1; faked device
    // motion writes its explicit payload. After the frozen check, like
    // the bitmask updates below.
    if q.evcode == first + crate::xinput::XI_DEVICE_MOTION_NOTIFY_OFFSET {
        let entry = state.xi1_device_input_state.entry(q.deviceid).or_default();
        if let Some(axes) = q.axes {
            for i in 0..usize::from(axes.count.min(6)) {
                if let Some(slot) = entry.valuators.get_mut(usize::from(axes.first) + i) {
                    *slot = axes.values[i];
                }
            }
        } else {
            entry.valuators[0] = i32::from(q.root_x);
            entry.valuators[1] = i32::from(q.root_y);
        }
    }

    // Maintain the per-device key/button-down bitmasks consumed by
    // DeviceStateNotify (Xorg `dev->key->down` / `dev->button->down`).
    // After the frozen check: queued events come back through here on
    // thaw, so updating at queue time would double-count them.
    if is_press || is_release {
        let is_key = q.evcode == first + XI_DEVICE_KEY_PRESS_OFFSET
            || q.evcode == first + XI_DEVICE_KEY_RELEASE_OFFSET;
        let entry = state.xi1_device_input_state.entry(q.deviceid).or_default();
        let bits = if is_key {
            &mut entry.keys_down
        } else {
            &mut entry.buttons_down
        };
        let (byte, bit) = (usize::from(q.detail) / 8, q.detail % 8);
        if is_press {
            bits[byte] |= 1 << bit;
        } else {
            bits[byte] &= !(1 << bit);
        }
    }

    // 2. Active grab.
    if let Some(grab) = state.xi1_active_grabs.get(&q.deviceid).copied() {
        // owner_events: natural delivery when the natural target's
        // selection walk would report to the grab owner anyway.
        let natural = compute_xi1_route_targets(state, &q);
        let (targets, event_window) = match natural {
            Some((clients, w)) if grab.owner_events && clients.contains(&grab.owner) => {
                (vec![grab.owner], w)
            }
            _ => (vec![grab.owner], grab.grab_window),
        };
        log::debug!(
            "xi1_route: evcode={} GRAB owner={} window=0x{:x} detail={}",
            q.evcode,
            grab.owner.0,
            event_window.0,
            q.detail,
        );
        let deliver = !grab.implicit || grab.device_mask & xi1_event_mask_bit(q.evcode) != 0;
        let dropped = if deliver {
            xi1_fan_device_event(state, &targets, event_window, &q)
        } else {
            Vec::new()
        };
        // Sync-state transition on a delivered key/button event (Xorg
        // FreezeThisEventIfNeededForSyncGrab) — the FreezeNextEvent /
        // FreezeBothNextEvent armed states trip to FrozenWithEvent
        // here. A plain sync grab does NOT re-freeze on every press:
        // CheckGrabForSyncs froze it once at activation.
        if allow_freeze && (is_press || is_release) {
            xi1_freeze_this_event_if_needed(state, q.deviceid, grab.owner, &q);
        }
        let implicit_grab_ended = grab.implicit
            && is_button_release
            && state
                .xi1_device_input_state
                .get(&q.deviceid)
                .is_none_or(|device| device.buttons_down.iter().all(|byte| *byte == 0));
        if (is_release && grab.passive_detail == Some(q.detail)) || implicit_grab_ended {
            xi1_deactivate_device_grab(state, q.deviceid);
        }
        return dropped;
    }

    // 3. Passive grab activation on press. A ReplayThisDevice
    // reprocessing pass skips grabs at or above the released grab's
    // window — "as though they were not present" (the replay floor).
    if is_press {
        let matched = state
            .xi1_passive_grabs
            .iter()
            .find(|g| {
                g.deviceid == q.deviceid
                    && (g.detail == 0 || g.detail == q.detail)
                    && (g.modifiers == 0x8000 || g.modifiers == q.state_mask & 0x00ff)
                    && xi1_window_in_chain(state, q.natural_target, g.grab_window)
                    && !q
                        .replay_floor
                        .is_some_and(|floor| xi1_window_in_chain(state, floor, g.grab_window))
            })
            .copied();
        if let Some(g) = matched {
            state.xi1_active_grabs.insert(
                q.deviceid,
                crate::server::Xi1ActiveGrab {
                    owner: g.owner,
                    deviceid: q.deviceid,
                    grab_window: g.grab_window,
                    owner_events: g.owner_events,
                    this_mode: g.this_mode,
                    other_mode: g.other_mode,
                    implicit: false,
                    device_mask: 0,
                    passive_detail: Some(q.detail),
                },
            );
            state.xi1_last_grab_time = q.time;
            // CheckGrabForSyncs at activation, then deliver the
            // activating press; a sync grab stores it for Replay
            // (FROZEN_NO_EVENT → FROZEN_WITH_EVENT, Xorg
            // DeliverGrabbedEvent / ActivateGrabNoDelivery tail).
            if allow_freeze {
                xi1_check_grab_for_syncs(
                    state,
                    q.deviceid,
                    g.owner,
                    g.this_mode == 0,
                    g.other_mode == 0,
                );
            }
            let dropped = xi1_fan_device_event(state, &[g.owner], g.grab_window, &q);
            if allow_freeze {
                let sync = state.xi1_frozen.entry(q.deviceid).or_default();
                if sync.state == crate::server::Xi1SyncState::FrozenNoEvent {
                    sync.state = crate::server::Xi1SyncState::FrozenWithEvent;
                    sync.stored = Some(crate::server::QueuedInputEvent::Xi1Routed(q));
                }
            }
            return dropped;
        }
    }

    // 4. Selection delivery, gated by the device-focus route
    // (DeliverFocusedEvent for keyboard devices; plain walk for
    // pointer devices).
    let (selection_filter, _direct_grab_route) = xi1_selection_filter_for_slave_grab(state, &q);
    let mut dropped = Vec::new();
    let hit = if is_button_press
        && let Some(delivery) = slave_press_delivery
        && matches!(selection_filter, Xi1SelectionFilter::Unrestricted)
    {
        match delivery {
            SlavePressDelivery::Xi1 { window, recipients } => Some((recipients.clone(), *window)),
            SlavePressDelivery::Xi2 { .. } | SlavePressDelivery::None => None,
        }
    } else {
        match selection_filter {
            Xi1SelectionFilter::Suppress => None,
            Xi1SelectionFilter::Unrestricted => compute_xi1_route_targets(state, &q),
            Xi1SelectionFilter::Client(client) => {
                compute_xi1_route_targets(state, &q).and_then(|(targets, window)| {
                    targets.contains(&client).then_some((vec![client], window))
                })
            }
        }
    };
    let xi1_implicit_press = is_button_press
        && !state.xi1_active_grabs.contains_key(&q.deviceid)
        && !state.xi2_pointer_grabs.contains_key(&q.deviceid)
        && match slave_press_delivery {
            Some(SlavePressDelivery::Xi1 { recipients, .. }) => !recipients.is_empty(),
            Some(SlavePressDelivery::Xi2 { .. } | SlavePressDelivery::None) => false,
            None => matches!(selection_filter, Xi1SelectionFilter::Unrestricted) && hit.is_some(),
        };
    log::debug!(
        "xi1_route: evcode={} target=0x{:x} route={:?} hit={:?}",
        q.evcode,
        q.natural_target.0,
        q.focus_route,
        hit.as_ref()
            .map(|(t, w)| (t.iter().map(|c| c.0).collect::<Vec<_>>(), w.0)),
    );
    let xi1_dropped = hit.as_ref().map_or_else(Vec::new, |(targets, w)| {
        xi1_fan_device_event(state, targets, *w, &q)
    });
    merge_dropped(&mut dropped, xi1_dropped);
    if xi1_implicit_press && let Some((targets, grab_window)) = hit.as_ref() {
        // ../xserver/dix/events.c:2277-2310 walks every delivering node,
        // including duplicate clients. The first node with ButtonGrab wins;
        // otherwise the last delivering node owns the implicit grab.
        let owner = xi1_window_selectors_with_grab_class(state, *grab_window, q.evcode, q.deviceid)
            .iter()
            .find_map(|(client, has_grab_class)| {
                (*has_grab_class && targets.contains(client)).then_some(*client)
            })
            .or_else(|| targets.last().copied());
        if let Some(owner) = owner {
            let device_mask = xi1_device_mask_on_window(state, *grab_window, q.deviceid);
            state.xi1_active_grabs.insert(
                q.deviceid,
                crate::server::Xi1ActiveGrab {
                    owner,
                    deviceid: q.deviceid,
                    grab_window: *grab_window,
                    owner_events: false,
                    this_mode: 1,
                    other_mode: 1,
                    implicit: true,
                    device_mask,
                    passive_detail: None,
                },
            );
            state.xi1_last_grab_time = q.time;
        }
    }
    // A BRIDGED core grab (no XI1 grab, but the core pointer/keyboard
    // grab controls this device) must still trip an armed
    // FreezeNextEvent / FreezeBothNextEvent on key/button events —
    // Xorg delivers through that one grab slot and trips there.
    if allow_freeze
        && (is_press || is_release)
        && let Some(owner) = xi1_device_grab_owner(state, q.deviceid)
    {
        xi1_freeze_this_event_if_needed(state, q.deviceid, owner, &q);
    }
    dropped
}

/// Apply the event's [`Xi1FocusRoute`] to find selection-delivery
/// targets — the XI1 analogue of Xorg `DeliverFocusedEvent`
/// (dix/events.c:4202): unbounded walk, walk bounded at the focus
/// window, focus-window-only, or none (focus = None).
fn compute_xi1_route_targets(
    state: &ServerState,
    q: &crate::server::Xi1QueuedEvent,
) -> Option<(Vec<ClientId>, ResourceId)> {
    match q.focus_route {
        crate::server::Xi1FocusRoute::Walk => {
            compute_xi1_targets_bounded(state, q.natural_target, q.evcode, q.deviceid, None)
        }
        crate::server::Xi1FocusRoute::WalkUpTo(stop) => {
            compute_xi1_targets_bounded(state, q.natural_target, q.evcode, q.deviceid, Some(stop))
        }
        crate::server::Xi1FocusRoute::WindowOnly(w) => {
            let targets = xi1_window_selectors(state, w, q.evcode, q.deviceid);
            if targets.is_empty() {
                None
            } else {
                Some((targets, w))
            }
        }
        crate::server::Xi1FocusRoute::Drop => None,
    }
}

fn resolve_slave_press_delivery(
    state: &ServerState,
    target: ResourceId,
    deviceid: u16,
    xi1_evcode: u8,
) -> SlavePressDelivery {
    let mut window = target;
    for _ in 0..256 {
        let xi2_recipients = xi2_form_targets_for_source(
            state,
            window,
            Xi2PointerForm::Slave,
            xi2_evtype(PointerEventKind::ButtonPress),
            Some(deviceid),
        );
        if !xi2_recipients.is_empty() {
            return SlavePressDelivery::Xi2 {
                window,
                recipients: xi2_recipients,
            };
        }

        let xi1_recipients = xi1_window_selectors(state, window, xi1_evcode, deviceid);
        if !xi1_recipients.is_empty() {
            return SlavePressDelivery::Xi1 {
                window,
                recipients: xi1_recipients,
            };
        }

        if window == ROOT_WINDOW {
            break;
        }
        let Some(parent) = state.resources.parent_of(window) else {
            break;
        };
        window = parent;
    }
    SlavePressDelivery::None
}

/// True when `grab_window` is `target` or one of its ancestors.
fn xi1_window_in_chain(state: &ServerState, target: ResourceId, grab_window: ResourceId) -> bool {
    let mut window = target;
    loop {
        if window == grab_window {
            return true;
        }
        if window == ROOT_WINDOW {
            return false;
        }
        match state.resources.window(window).map(|w| w.parent) {
            Some(parent) if parent != window => window = parent,
            _ => return false,
        }
    }
}

fn xi1_fan_device_event(
    state: &mut ServerState,
    targets: &[ClientId],
    event_window: ResourceId,
    q: &crate::server::Xi1QueuedEvent,
) -> Vec<ClientId> {
    // DeviceMotionNotify MUST be a MORE_EVENTS chain with a trailing
    // deviceValuator: libXi's XInputWireToEvent returns DONT_ENQUEUE
    // unconditionally for the leading motion event and only enqueues
    // when the valuator continuation lands (XExtInt.c) — a bare motion
    // event silently vanishes inside every libXi client. Key/button
    // events enqueue standalone. The valuator payload mirrors the
    // device's X/Y axes (the sprite position), matching Xorg's
    // getValuatorEvents for a 2-axis motion.
    let is_motion =
        q.evcode == crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_MOTION_NOTIFY_OFFSET;
    // Xorg FixUpEventFromWindow: coordinates relative to the event
    // window's inside origin; child per FindChildForEvent from the
    // sprite window (the pointer hit for pointer events, the deepest
    // window under the pointer for key events).
    let is_key = q.evcode
        == crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_KEY_PRESS_OFFSET
        || q.evcode == crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_KEY_RELEASE_OFFSET;
    let targets: Vec<ClientId> = if q.synthetic_repeat
        && q.evcode == crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_KEY_RELEASE_OFFSET
    {
        targets
            .iter()
            .copied()
            .filter(|client| {
                !state
                    .xkb_clients
                    .get(&client.0)
                    .is_some_and(|flags| flags.wants_detectable_auto_repeat())
            })
            .collect()
    } else {
        targets.to_vec()
    };
    if targets.is_empty() {
        return Vec::new();
    }
    let sprite = if is_key {
        crate::core_loop::key_fanout::deepest_window_at_pointer(state)
    } else {
        q.natural_target
    };
    let child = xi2_child_toward(state, event_window, sprite);
    let (ox, oy) = state.resources.window_absolute_position(event_window);
    let event_x = clamp_grab_coord(q.root_x, ox);
    let event_y = clamp_grab_coord(q.root_y, oy);
    let mut dropped = Vec::new();
    // The general fanout deduplicates client IDs. XI1 selection delivery
    // instead sends once per InputClients node (../xserver/dix/events.c:2279-2316).
    for client in targets {
        let failed = fanout_event_to_clients(state, &[client], |buf, seq, order| {
            crate::xinput::encode_xi1_device_input_event(
                buf,
                order,
                q.evcode,
                q.detail,
                seq,
                q.time,
                ROOT_WINDOW.0,
                event_window.0,
                child.0,
                q.root_x,
                q.root_y,
                event_x,
                event_y,
                q.state_mask,
                if is_motion {
                    q.deviceid | u16::from(crate::xinput::XI1_MORE_EVENTS)
                } else {
                    q.deviceid
                },
            );
            if is_motion {
                // Faked motion carries its explicit axis payload; real
                // motion reports the X/Y axes (= sprite position).
                let (num, first_v, values) = match q.axes {
                    Some(a) => (a.count.min(6), a.first, a.values),
                    None => (2, 0, [i32::from(q.root_x), i32::from(q.root_y), 0, 0, 0, 0]),
                };
                #[allow(clippy::cast_possible_truncation)]
                crate::xinput::encode_xi1_device_valuator(
                    buf,
                    order,
                    crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_VALUATOR_OFFSET,
                    q.deviceid as u8,
                    seq,
                    q.state_mask,
                    num,
                    first_v,
                    values,
                );
            }
        });
        merge_dropped(&mut dropped, failed);
    }
    dropped
}

/// Resolve the Xorg paired master from the requested device's current
/// attachment. Floating slaves have no paired device.
pub(crate) fn xi1_other_input_device(state: &ServerState, deviceid: u16) -> Option<u16> {
    state.xi_devices.paired_master(deviceid).or_else(|| {
        state
            .xi2_detached_masters
            .get(&deviceid)
            .and_then(|master| state.xi_devices.paired_master(*master))
    })
}

/// Force-thaw a device: reset its sync state outright and flush. Used
/// by teardown paths (client disconnect with no grabs left) where no
/// grab semantics apply — NOT by AllowDeviceEvents, which manipulates
/// the sync state per Xorg `AllowSome` and then calls
/// [`xi1_compute_freezes`].
pub(crate) fn xi1_thaw_device(
    state: &mut ServerState,
    backend: &mut dyn crate::backend::Backend,
    xid_map: &HostXidMap,
    deviceid: u16,
) {
    if let Some(freeze) = state.xi1_frozen.get_mut(&deviceid) {
        freeze.state = crate::server::Xi1SyncState::Thawed;
        freeze.other = None;
    }
    xi1_compute_freezes(state, backend, xid_map);
}

/// Port of Xorg `ComputeFreezes` (dix/events.c:1320) over the
/// two-device model: re-derive each device's frozen flag from its sync
/// state and flush the queued events of every no-longer-frozen device.
/// Re-freezing mid-flush (a queued press activating a sync passive
/// grab) leaves the remainder queued, exactly like Xorg's restart loop.
pub(crate) fn xi1_compute_freezes(
    state: &mut ServerState,
    backend: &mut dyn crate::backend::Backend,
    xid_map: &HostXidMap,
) {
    if state.playing_sync_events {
        return;
    }
    // SAFETY: local guard cannot outlive this mutable ServerState borrow.
    let _replay_guard =
        unsafe { crate::server::SyncReplayGuard::arm(&mut state.playing_sync_events) };
    // Xorg PlayReleasedEvents: after each delivery restart the search from
    // the head, because that delivery can change another device's freeze.
    loop {
        let index = state.sync_pending.iter().position(|pending| {
            !state
                .xi1_frozen
                .get(&pending.device)
                .is_some_and(crate::server::Xi1Freeze::frozen)
        });
        let Some(index) = index else { break };
        let pending = state
            .sync_pending
            .remove(index)
            .expect("pending index is valid");
        match pending.event {
            crate::server::QueuedInputEvent::HostPointer(event) => {
                let _ = pointer_event_fanout_to_state(state, backend, xid_map, event, false, false);
            }
            crate::server::QueuedInputEvent::HostPointerMaster(event, scroll_motion) => {
                let _ = play_queued_master_pointer_event(
                    state,
                    backend,
                    xid_map,
                    event,
                    pending.device,
                    scroll_motion,
                );
            }
            crate::server::QueuedInputEvent::HostKey(event) => {
                let _ = crate::core_loop::key_fanout::deliver_routed_key(state, event);
            }
            crate::server::QueuedInputEvent::HostKeyTransition(
                event,
                master_transition_accepted,
                synthetic_repeat,
            ) => {
                let _ = crate::core_loop::key_fanout::deliver_routed_key_after_transition(
                    state,
                    event,
                    master_transition_accepted,
                    synthetic_repeat,
                );
            }
            crate::server::QueuedInputEvent::Xi1Routed(event) => {
                let _ = xi1_route_device_event(state, event, true, None);
            }
            crate::server::QueuedInputEvent::RawKey(event) => {
                let _ = crate::core_loop::key_fanout::deliver_raw_key_master(state, event);
            }
        }
    }
}

/// End active grabs on a physical source, restore its saved attachments,
/// and replay input released by a dynamic XI2 grab's paired-device hold.
/// The caller keeps the source registered until held-state releases have
/// passed through the normal fanout path, then disables and unregisters it.
pub fn xi_cleanup_source(
    state: &mut ServerState,
    backend: &mut dyn crate::backend::Backend,
    source: crate::xinput::InputSourceId,
) {
    let device_ids: Vec<u16> = [
        state
            .xi_devices
            .facet(source, crate::xinput::XiFacetKind::Keyboard),
        state
            .xi_devices
            .facet(source, crate::xinput::XiFacetKind::PointerTouch),
    ]
    .into_iter()
    .flatten()
    .collect();

    for device_id in device_ids {
        xi1_deactivate_device_grab(state, device_id);

        let pointer_grab = state.xi2_pointer_grabs.get(&device_id).copied();
        let pointer_owner = pointer_grab
            .filter(|grab| !grab.implicit)
            .map(|grab| grab.owner);
        if pointer_grab.is_some_and(|grab| grab.implicit) {
            state.remove_implicit_xi2_pointer_grab(device_id);
        } else if pointer_grab.is_some() {
            state.xi2_pointer_grabs.remove(&device_id);
        }
        let keyboard_owner = state
            .xi2_keyboard_grabs
            .remove(&device_id)
            .map(|grab| grab.owner);
        for owner in [pointer_owner, keyboard_owner].into_iter().flatten() {
            if let Some(freeze) = state.xi1_frozen.get_mut(&device_id) {
                freeze.state = crate::server::Xi1SyncState::Thawed;
                freeze.stored = None;
            }
            xi1_core_grab_bridge_release(state, device_id, owner);
        }
        if pointer_owner.is_some()
            || keyboard_owner.is_some()
            || state.xi2_detached_masters.contains_key(&device_id)
        {
            state.reattach_xi2_slave(device_id);
        }
    }

    let xid_map = backend.xid_map().clone();
    xi1_compute_freezes(state, backend, &xid_map);
}

/// Port of Xorg `CheckGrabForSyncs` (dix/events.c:1424-1450): set the
/// sync state at grab activation. A sync `this_mode` freezes the
/// grabbed device ONCE (FrozenNoEvent); a sync `other_mode` holds the
/// paired device on behalf of this grab (`sync.other`). Async modes
/// release the same-client holds.
pub(crate) fn xi1_check_grab_for_syncs(
    state: &mut ServerState,
    deviceid: u16,
    owner: ClientId,
    this_sync: bool,
    other_sync: bool,
) {
    {
        let sync = state.xi1_frozen.entry(deviceid).or_default();
        if this_sync {
            sync.state = crate::server::Xi1SyncState::FrozenNoEvent;
        } else {
            sync.state = crate::server::Xi1SyncState::Thawed;
            if sync.other == Some(owner) {
                sync.other = None;
            }
        }
    }
    // Xorg CheckGrabForSyncs only synchronizes the paired device for a
    // master grab (dix/events.c:1439). Slave grabs force paired_device_mode
    // to Async and never freeze their paired master.
    if matches!(
        state.xi_devices.role(deviceid),
        Some(
            crate::xinput::XiDeviceRole::MasterPointer
                | crate::xinput::XiDeviceRole::MasterKeyboard
        )
    ) && let Some(other_dev) = xi1_other_input_device(state, deviceid)
    {
        let other = state.xi1_frozen.entry(other_dev).or_default();
        if other_sync {
            other.other = Some(owner);
        } else if other.other == Some(owner) {
            other.other = None;
        }
    }
}

/// Port of Xorg `FreezeThisEventIfNeededForSyncGrab`
/// (dix/events.c:4420-4447): after a key/button event is delivered
/// through the active grab, an armed FreezeNextEvent /
/// FreezeBothNextEvent state trips to FrozenWithEvent (storing the
/// event for Replay); FreezeBothNextEvent also re-holds the paired
/// device.
pub(crate) fn xi1_freeze_this_event_if_needed(
    state: &mut ServerState,
    deviceid: u16,
    owner: ClientId,
    q: &crate::server::Xi1QueuedEvent,
) {
    use crate::server::Xi1SyncState;
    let st = state
        .xi1_frozen
        .get(&deviceid)
        .map_or(Xi1SyncState::Thawed, |f| f.state);
    match st {
        Xi1SyncState::FreezeBothNextEvent => {
            let other_dev = xi1_other_input_device(state, deviceid);
            let Some(other_dev) = other_dev else {
                let sync = state.xi1_frozen.entry(deviceid).or_default();
                sync.state = Xi1SyncState::FrozenWithEvent;
                sync.stored = Some(crate::server::QueuedInputEvent::Xi1Routed(*q));
                return;
            };
            let other_owner = xi1_device_grab_owner(state, other_dev);
            let other = state.xi1_frozen.entry(other_dev).or_default();
            if other.state == Xi1SyncState::FreezeBothNextEvent && other_owner == Some(owner) {
                other.state = Xi1SyncState::FrozenNoEvent;
            } else {
                other.other = Some(owner);
            }
            let sync = state.xi1_frozen.entry(deviceid).or_default();
            sync.state = Xi1SyncState::FrozenWithEvent;
            sync.stored = Some(crate::server::QueuedInputEvent::Xi1Routed(*q));
        }
        Xi1SyncState::FreezeNextEvent => {
            let sync = state.xi1_frozen.entry(deviceid).or_default();
            sync.state = Xi1SyncState::FrozenWithEvent;
            sync.stored = Some(crate::server::QueuedInputEvent::Xi1Routed(*q));
        }
        _ => {}
    }
}

/// Deactivate the active XI1 grab on `deviceid` (UngrabDevice, passive
/// release auto-end, disconnect teardown): reset its sync state and
/// release a paired-device hold owned by this grab (Xorg
/// DeactivateKeyboard/PointerGrab clears only `sync.other` references to
/// the departing grab, `../xserver/dix/events.c:1698-1700,1799-1801`).
pub(crate) fn xi1_deactivate_device_grab(state: &mut ServerState, deviceid: u16) {
    let Some(grab) = state.xi1_active_grabs.remove(&deviceid) else {
        return;
    };
    if let Some(sync) = state.xi1_frozen.get_mut(&deviceid) {
        sync.state = crate::server::Xi1SyncState::Thawed;
        sync.stored = None;
    }
    // DeviceButtonPress implicit grabs are created on slave devices with
    // asynchronous paired-device mode. They never own a paired hold, so
    // an `other` hold from another same-client master grab must survive.
    let other_dev = (!grab.implicit)
        .then(|| xi1_other_input_device(state, deviceid))
        .flatten();
    if let Some(other) = other_dev.and_then(|id| state.xi1_frozen.get_mut(&id))
        && other.other == Some(grab.owner)
    {
        other.other = None;
    }
}

/// Release the core-grab bridge hold on `deviceid` at GRAB
/// DEACTIVATION (core UngrabPointer / UngrabKeyboard, passive key-grab
/// auto-release): thaw the device's sync state when no XI1 grab
/// controls it and release the paired device if held on `owner`'s
/// behalf — Xorg DeactivateKeyboard/PointerGrab clears every
/// `sync.other` pointing at the dying grab.
pub(crate) fn xi1_core_grab_bridge_release(
    state: &mut ServerState,
    deviceid: u16,
    owner: ClientId,
) {
    if !state.xi1_active_grabs.contains_key(&deviceid)
        && let Some(sync) = state.xi1_frozen.get_mut(&deviceid)
    {
        sync.state = crate::server::Xi1SyncState::Thawed;
        sync.stored = None;
    }
    let other_dev = xi1_other_input_device(state, deviceid);
    if let Some(other) = other_dev.and_then(|id| state.xi1_frozen.get_mut(&id))
        && other.other == Some(owner)
    {
        other.other = None;
    }
}

/// The client owning the grab that controls `deviceid`'s sync state:
/// the XI1 device grab, or the bridged core grab (core pointer grab ↔
/// slave pointer, core keyboard grab ↔ slave keyboard) — in Xorg these
/// are one `deviceGrab.grab` slot.
pub(crate) fn xi1_device_grab_owner(state: &ServerState, deviceid: u16) -> Option<ClientId> {
    if let Some(g) = state.xi1_active_grabs.get(&deviceid) {
        return Some(g.owner);
    }
    if let Some(g) = state.xi2_pointer_grabs.get(&deviceid) {
        return Some(g.owner);
    }
    if let Some(g) = state.xi2_keyboard_grabs.get(&deviceid) {
        return Some(g.owner);
    }
    match state.xi_devices.role(deviceid)? {
        crate::xinput::XiDeviceRole::MasterPointer => {
            state.active_pointer_grab.as_ref().map(|grab| grab.owner)
        }
        crate::xinput::XiDeviceRole::SlavePointer => {
            let master = state.xi_devices.attachment(deviceid)?;
            (master == crate::xinput::DEVICEID_MASTER_POINTER)
                .then(|| state.active_pointer_grab.as_ref().map(|grab| grab.owner))
                .flatten()
        }
        crate::xinput::XiDeviceRole::MasterKeyboard => {
            state.active_keyboard_grab.as_ref().map(|grab| grab.owner)
        }
        crate::xinput::XiDeviceRole::SlaveKeyboard => {
            let master = state.xi_devices.attachment(deviceid)?;
            (master == crate::xinput::DEVICEID_MASTER_KEYBOARD)
                .then(|| state.active_keyboard_grab.as_ref().map(|grab| grab.owner))
                .flatten()
        }
    }
}

/// Find the clients receiving an XI 1.x device input event: starting
/// at the hit target, walk up the ancestor chain; the first window
/// where at least one client selected `(deviceid << 8) | evcode`
/// becomes the event window, and all clients selecting there receive
/// it. Mirrors core propagation (Xorg dix DeliverDeviceEvents); the
/// XI1 dont-propagate list is not honoured yet.
///
/// `stop_at` is the last window checked (inclusive) — Xorg
/// `DeliverDeviceEvents`'s `stopAt` argument, used when a focus window
/// caps the walk (dix/events.c:4220).
fn compute_xi1_targets_bounded(
    state: &ServerState,
    target: ResourceId,
    evcode: u8,
    deviceid: u16,
    stop_at: Option<ResourceId>,
) -> Option<(Vec<ClientId>, ResourceId)> {
    let mut window = target;
    loop {
        let targets = xi1_window_selectors(state, window, evcode, deviceid);
        if !targets.is_empty() {
            return Some((targets, window));
        }
        if window == ROOT_WINDOW || stop_at == Some(window) {
            return None;
        }
        let parent = state.resources.window(window).map(|w| w.parent)?;
        if parent == window {
            return None;
        }
        window = parent;
    }
}

fn translate_host_event(
    state: &ServerState,
    xid_map: &HostXidMap,
    event: HostPointerEvent,
) -> HostPointerEvent {
    let Some(top_level_id) = xid_map.get(&event.host_xid).copied() else {
        return event;
    };
    let Some((rx, ry)) = state
        .resources
        .window(top_level_id)
        .map(|w| (w.x + event.event_x, w.y + event.event_y))
    else {
        return event;
    };
    HostPointerEvent {
        origin: event.origin,
        root_x: rx,
        root_y: ry,
        ..event
    }
}

/// Recipients of a core Enter/Leave on `window` — Xorg `CoreEnterLeaveEvent`
/// (`dix/events.c:4748`). A crossing never propagates. Under a pointer grab
/// only the grab client gets it: on the grab window through the grab's mask,
/// and with `owner_events` through its own selection on `window`.
fn core_crossing_targets(state: &ServerState, window: ResourceId, mask_bit: u32) -> Vec<ClientId> {
    let Some((grab_window, grab_client, _, _, owner_events, via_xi2, grab_mask)) =
        active_grab_target(state)
    else {
        return crate::core_loop::fanout::subscribers_by_id(state, window, mask_bit);
    };
    let mut mask = if window == grab_window && !via_xi2 {
        grab_mask
    } else {
        0
    };
    if owner_events {
        mask |= state
            .clients
            .get(&grab_client.0)
            .and_then(|c| c.event_masks.get(&window).copied())
            .unwrap_or(0);
    }
    if mask & mask_bit == 0 {
        Vec::new()
    } else {
        vec![grab_client]
    }
}

fn active_grab_target(
    state: &ServerState,
) -> Option<(
    yserver_protocol::x11::ResourceId,
    ClientId,
    i32,
    i32,
    bool,
    bool,
    u32,
)> {
    let grab = state.active_pointer_grab?;
    active_pointer_grab_target(state, grab)
}

fn active_grab_for_source(
    state: &ServerState,
    source: PointerXiSource,
) -> Option<crate::server::ActivePointerGrab> {
    if let Some(device_id) = source.slave_deviceid {
        if let Some(grab) = state.xi2_pointer_grabs.get(&device_id) {
            return Some(*grab);
        }
        return (source.attached_master == Some(XI2_MASTER_POINTER_DEVICE_ID))
            .then_some(state.active_pointer_grab)
            .flatten();
    }
    (source.sourceid == XI2_MASTER_POINTER_DEVICE_ID)
        .then_some(state.active_pointer_grab)
        .flatten()
}

fn active_grab_for_device(
    state: &ServerState,
    device_id: u16,
) -> Option<crate::server::ActivePointerGrab> {
    // CheckDeviceGrabs checks the replay device's own grab slot (Xorg
    // `dix/events.c:1336,4203`); another form's source grab does not occupy it.
    match state.xi_devices.role(device_id) {
        Some(crate::xinput::XiDeviceRole::MasterPointer) => state.active_pointer_grab,
        Some(crate::xinput::XiDeviceRole::SlavePointer) => {
            state.xi2_pointer_grabs.get(&device_id).copied()
        }
        _ => None,
    }
}

fn active_pointer_grab_target(
    state: &ServerState,
    grab: crate::server::ActivePointerGrab,
) -> Option<(
    yserver_protocol::x11::ResourceId,
    ClientId,
    i32,
    i32,
    bool,
    bool,
    u32,
)> {
    let target = client_target_id(state, grab.owner)?;
    let (gx, gy) = state.resources.window_absolute_position(grab.grab_window);
    Some((
        grab.grab_window,
        target,
        gx,
        gy,
        grab.owner_events,
        grab.via_xi2,
        u32::from(grab.event_mask),
    ))
}

struct DeferredCorePointerEvent {
    target: ClientId,
    window: ResourceId,
    child: ResourceId,
    event: HostPointerEvent,
    event_x: i16,
    event_y: i16,
    focus: bool,
}

fn defer_core_pointer_event(
    deferred: &mut Vec<DeferredCorePointerEvent>,
    targets: &[ClientId],
    window: ResourceId,
    child: ResourceId,
    event: HostPointerEvent,
    event_x: i16,
    event_y: i16,
    focus: bool,
) {
    for target in targets {
        deferred.push(DeferredCorePointerEvent {
            target: *target,
            window,
            child,
            event,
            event_x,
            event_y,
            focus,
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn fanout_core_pointer_event_ordered(
    state: &mut ServerState,
    deferred: &mut Vec<DeferredCorePointerEvent>,
    targets: &[ClientId],
    window: ResourceId,
    child: ResourceId,
    event: HostPointerEvent,
    event_x: i16,
    event_y: i16,
    focus: bool,
    source: PointerXiSource,
    natural_target: ResourceId,
    top_level: ResourceId,
    is_replay: bool,
) -> Vec<ClientId> {
    let defer_slave_form = !is_replay
        && source.slave_deviceid.is_some()
        && !matches!(
            event.kind,
            PointerEventKind::EnterNotify | PointerEventKind::LeaveNotify
        );
    let mut immediate = Vec::with_capacity(targets.len());
    for target in targets {
        let has_slave_form = defer_slave_form
            && source.slave_deviceid.is_some_and(|slave_deviceid| {
                xi2_pointer_forms_for_source(
                    state,
                    *target,
                    natural_target,
                    top_level,
                    xi2_evtype(event.kind),
                    Some(slave_deviceid),
                    None,
                )
                .1
            });
        if has_slave_form {
            defer_core_pointer_event(
                deferred,
                std::slice::from_ref(target),
                window,
                child,
                event,
                event_x,
                event_y,
                focus,
            );
        } else {
            immediate.push(*target);
        }
    }
    fanout_event_to_clients(state, &immediate, |buf, seq, order| {
        encode_pointer_event(
            buf,
            order,
            event.kind,
            seq,
            event.detail,
            event.time,
            window,
            child,
            event,
            event_x,
            event_y,
            focus,
        );
    })
}

fn fanout_deferred_core_pointer_event(
    state: &mut ServerState,
    delivery: DeferredCorePointerEvent,
) -> Vec<ClientId> {
    fanout_event_to_clients(
        state,
        std::slice::from_ref(&delivery.target),
        |buf, seq, order| {
            encode_pointer_event(
                buf,
                order,
                delivery.event.kind,
                seq,
                delivery.event.detail,
                delivery.event.time,
                delivery.window,
                delivery.child,
                delivery.event,
                delivery.event_x,
                delivery.event_y,
                delivery.focus,
            );
        },
    )
}

fn flush_deferred_core_pointer_events_for(
    state: &mut ServerState,
    deferred: &mut Vec<DeferredCorePointerEvent>,
    target: ClientId,
) -> Vec<ClientId> {
    let mut dropped = Vec::new();
    let mut pending = Vec::with_capacity(deferred.len());
    for delivery in deferred.drain(..) {
        if delivery.target == target {
            merge_dropped(
                &mut dropped,
                fanout_deferred_core_pointer_event(state, delivery),
            );
        } else {
            pending.push(delivery);
        }
    }
    *deferred = pending;
    dropped
}

fn flush_deferred_core_pointer_events(
    state: &mut ServerState,
    deferred: Vec<DeferredCorePointerEvent>,
) -> Vec<ClientId> {
    let mut dropped = Vec::new();
    for delivery in deferred {
        merge_dropped(
            &mut dropped,
            fanout_deferred_core_pointer_event(state, delivery),
        );
    }
    dropped
}

fn release_passive_grab_on_button_release(
    state: &mut ServerState,
    kind: PointerEventKind,
    origin: crate::core_loop::InputOrigin,
) {
    if kind != PointerEventKind::ButtonRelease {
        return;
    }
    let Some(source) = resolve_pointer_xi_source(state, origin, false) else {
        return;
    };
    let device_id = source.slave_deviceid.unwrap_or(source.sourceid);
    let source_buttons_down = source
        .slave_deviceid
        .and_then(|id| {
            state
                .xi_devices
                .device(id)
                .map(|device| device.buttons_down)
        })
        .unwrap_or(state.buttons_down);
    if source_buttons_down == 0
        && state
            .xi2_pointer_grabs
            .get(&device_id)
            .is_some_and(|grab| grab.passive)
    {
        let grab = state
            .xi2_pointer_grabs
            .remove(&device_id)
            .expect("matched per-device passive grab exists");
        if let Some(freeze) = state.xi1_frozen.get_mut(&device_id) {
            freeze.stored = None;
        }
        xi1_core_grab_bridge_release(state, device_id, grab.owner);
        state.reattach_xi2_slave(device_id);
        return;
    }
    if source_buttons_down == 0
        && state
            .xi2_pointer_grabs
            .get(&device_id)
            .is_some_and(|grab| grab.implicit)
    {
        state.remove_implicit_xi2_pointer_grab(device_id);
    }
    // The core pointer grab belongs to the master device, whose aggregate
    // held-button state is `state.buttons_down`. A release generated by
    // another source (for example XTEST4) must not end that grab while an
    // attached physical slave still holds the same master button.
    if state.buttons_down == 0 && state.active_pointer_grab.is_some_and(|grab| grab.passive) {
        let grab = state
            .active_pointer_grab
            .map(|active| (active.owner, active.grab_window));
        state.clear_pointer_grab();
        if let Some(freeze) = state
            .xi1_frozen
            .get_mut(&crate::xinput::DEVICEID_MASTER_POINTER)
        {
            freeze.stored = None;
        }
        state.pointer_confine_to = yserver_protocol::x11::ResourceId(0);
        // Xorg DeactivatePointerGrab → DoEnterLeaveEvents(grab window →
        // sprite, NotifyUngrab): the symmetric partner of the NotifyGrab
        // chain emitted on activation. This is the tear-down path for an
        // ASYNC passive grab that ends on the physical release (a sync
        // grab is already gone via AllowEvents ReplayPointer by now, so
        // this block is skipped for it).
        if let Some((owner, grab_window)) = grab {
            let to_win = crate::core_loop::key_fanout::deepest_window_at_pointer(state);
            crate::core_loop::process_request::emit_core_pointer_grab_chain(
                state,
                grab_window,
                to_win,
                2, // NotifyUngrab
            );
            // Xorg DeactivatePointerGrab: releasing the grab also
            // releases the sync holds it placed (a sync keyboard_mode
            // froze the keyboard on this grab's behalf — XGrabButton-19).
            xi1_core_grab_bridge_release(state, crate::xinput::DEVICEID_MASTER_POINTER, owner);
        }
    }
}

fn clamp_grab_coord(root_coord: i16, grab_origin: i32) -> i16 {
    i32::from(root_coord)
        .saturating_sub(grab_origin)
        .clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

/// Resolve a pointer event's hit window (deepest mapped target under
/// the pointer).
///
/// For `ButtonPress`/`ButtonRelease` the target is the sprite **sampled
/// at event generation**: descend from the producer-stamped `host_xid`
/// (resolved by the backend when the press was generated, against the
/// then-current tree). A WM restack that lands between the press and its
/// delivery must NOT retarget the in-flight click — that is the
/// "click lands on the window below" bug (measured: producer resolved
/// the top window, delivery re-resolved to a window the WM raised above
/// it in between). This matches Xorg `dix`, where the event carries the
/// window it was generated against; a later restructure only affects
/// future events. Buttons fall back to the live root hit only if the
/// producer window is gone (`host_xid` not in `xid_map`).
///
/// Motion/crossings keep tracking the live pointer (`root_pointer_target_at`).
fn resolve_pointer_hit(
    state: &ServerState,
    xid_map: &HostXidMap,
    event: &HostPointerEvent,
) -> Option<(ResourceId, i16, i16)> {
    let live_hit = || state.root_pointer_target_at(event.root_x, event.root_y);
    let gen_hit = || {
        xid_map.get(&event.host_xid).copied().map(|tl| {
            // Descend from the producer-stamped window (locked at event
            // generation, so a restack between press and delivery can't
            // retarget) — but derive the local coordinates from the true
            // cursor position (`root_x/root_y`) minus the window's
            // SCREEN-ABSOLUTE origin. Using `event.event_x/event_y` directly
            // is wrong for a window reparented into a WM frame: those are
            // relative to the window's parent-relative origin, not its
            // absolute origin, so the click landed ~frame-offset off
            // (cinnamon framed nemo: clicks ~50px low). `live_hit` (motion)
            // already uses `root`, so this also keeps clicks and hover on
            // the same coordinate basis. No-op for top-levels (absolute ==
            // parent-relative).
            let (ax, ay) = state.resources.window_absolute_position(tl);
            let lx = (i32::from(event.root_x) - ax).clamp(i32::from(i16::MIN), i32::from(i16::MAX))
                as i16;
            let ly = (i32::from(event.root_y) - ay).clamp(i32::from(i16::MIN), i32::from(i16::MAX))
                as i16;
            state.pointer_target_at(tl, lx, ly).unwrap_or((tl, lx, ly))
        })
    };
    if matches!(
        event.kind,
        PointerEventKind::ButtonPress | PointerEventKind::ButtonRelease
    ) {
        gen_hit().or_else(live_hit)
    } else {
        live_hit()
    }
}

fn try_match_passive_grab(
    state: &ServerState,
    xid_map: &HostXidMap,
    event: HostPointerEvent,
    replay: Option<PointerReplay>,
    deferred_master: bool,
    master_transition_accepted: bool,
) -> Option<(
    crate::server::PassiveButtonGrab,
    yserver_protocol::x11::ResourceId,
    crate::server::PassiveGrabPass,
)> {
    // Same generation-time target rule as delivery (passive button
    // grabs are a button context): match the grab against the window
    // hit when the press was generated, not a window restacked on top
    // since. Keeps grab matching and delivery consistent.
    let (hit_window, _, _) = resolve_pointer_hit(state, xid_map, &event)?;
    let source = resolve_pointer_xi_source(state, event.origin, false)?;
    let device_id = replay.map_or_else(
        || source.slave_deviceid.unwrap_or(source.sourceid),
        |replay| replay.device,
    );
    let replay_pass = replay.and_then(|replay| match state.xi_devices.role(replay.device) {
        Some(crate::xinput::XiDeviceRole::SlavePointer) => {
            Some(crate::server::PassiveGrabPass::Slave(replay.device))
        }
        Some(crate::xinput::XiDeviceRole::MasterPointer) => {
            Some(crate::server::PassiveGrabPass::Master)
        }
        _ => None,
    });
    let replay_floor = replay
        .filter(|_| !deferred_master)
        .map(|replay| replay.grab_window);
    let find_pass = |pass| {
        state.find_passive_grab(
            hit_window,
            event.detail,
            event.state,
            device_id,
            source.attached_master,
            Some(pass),
            replay_floor,
        )
    };
    let candidate = if let Some(replay_pass) = replay_pass {
        find_pass(replay_pass)
    } else {
        // Xorg processes a slave before its master (`mi/mieq.c:502-508`),
        // and `UpdateDeviceState` can independently return DONT_PROCESS for
        // the already-held master button (`Xi/exevents.c:938-955`). Keep the
        // slave lookup tied to its accepted transition, but skip the master
        // pass when that master's transition was rejected.
        let slave_candidate = source
            .slave_deviceid
            .and_then(|slave| find_pass(crate::server::PassiveGrabPass::Slave(slave)));
        slave_candidate.or_else(|| {
            master_transition_accepted
                .then(|| find_pass(crate::server::PassiveGrabPass::Master))
                .flatten()
        })
    }?;
    let (grab, pass) = candidate;
    Some((grab, hit_window, pass))
}

fn pointer_mask_bit(kind: PointerEventKind, state_mask: u16) -> u32 {
    match kind {
        PointerEventKind::ButtonPress => 0x0000_0004,
        PointerEventKind::ButtonRelease => 0x0000_0008,
        PointerEventKind::MotionNotify => {
            let mut bits: u32 = 0x0000_0040;
            let buttons_held = (state_mask >> 8) & 0x1f;
            if buttons_held != 0 {
                bits |= 0x0000_2000;
                for n in 0..5 {
                    if buttons_held & (1 << n) != 0 {
                        bits |= 0x0000_0100 << n;
                    }
                }
            }
            bits
        }
        PointerEventKind::EnterNotify => 0x0000_0010,
        PointerEventKind::LeaveNotify => 0x0000_0020,
    }
}

fn xi2_evtype(kind: PointerEventKind) -> u16 {
    match kind {
        PointerEventKind::ButtonPress => 4,
        PointerEventKind::ButtonRelease => 5,
        PointerEventKind::MotionNotify => 6,
        PointerEventKind::EnterNotify => 7,
        PointerEventKind::LeaveNotify => 8,
    }
}

/// The XI2 evtype whose selection ABSORBS this event during the core
/// propagation walk, or None when the absorb rule does not apply.
///
/// Device events only. Crossing events are NOT routed by
/// `DeliverDeviceEvents`' flavour-ordered loop in Xorg — they are
/// delivered per window along the crossing chain — so an XI_Enter /
/// XI_Leave selection must not suppress core Enter/Leave anywhere.
fn xi2_absorbing_evtype(kind: PointerEventKind) -> Option<u16> {
    match kind {
        PointerEventKind::ButtonPress
        | PointerEventKind::ButtonRelease
        | PointerEventKind::MotionNotify => Some(xi2_evtype(kind)),
        PointerEventKind::EnterNotify | PointerEventKind::LeaveNotify => None,
    }
}

fn xi2_raw_evtype(kind: PointerEventKind) -> Option<u16> {
    match kind {
        PointerEventKind::ButtonPress => Some(15),
        PointerEventKind::ButtonRelease => Some(16),
        PointerEventKind::MotionNotify => Some(17),
        PointerEventKind::EnterNotify | PointerEventKind::LeaveNotify => None,
    }
}

/// The direct child of `ancestor` on the path toward `descendant`
/// (`ResourceId(0)` when `ancestor == descendant` or they are unrelated).
/// Fills the XI2 `child` field when an event is reported on an ancestor of
/// the hit window, matching Xorg's `DeliverDeviceEvents`.
pub(crate) fn xi2_child_toward(
    state: &ServerState,
    ancestor: ResourceId,
    descendant: ResourceId,
) -> ResourceId {
    if ancestor == descendant {
        return ResourceId(0);
    }
    let mut cur = descendant;
    for _ in 0..256 {
        match state.resources.parent_of(cur) {
            Some(p) if p == ancestor => return cur,
            Some(p) => cur = p,
            None => return ResourceId(0),
        }
    }
    ResourceId(0)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Xi2PointerForm {
    Slave,
    Master,
}

fn xi2_form_selected_on_for_source(
    client: &crate::server::ClientState,
    window: ResourceId,
    form: Xi2PointerForm,
    evtype: u16,
    slave_deviceid: Option<u16>,
) -> bool {
    let bit = 1u64 << evtype;
    let selected = |device: u16| {
        client
            .xi2_masks
            .get(&(window, device))
            .is_some_and(|mask| mask & bit != 0)
    };
    match form {
        Xi2PointerForm::Slave => slave_deviceid.is_some_and(selected) || selected(0),
        Xi2PointerForm::Master => {
            selected(XI2_MASTER_POINTER_DEVICE_ID) || selected(1) || selected(0)
        }
    }
}

/// Route one XI2 device form using Xorg's `DeliverDeviceEvents` rule: walk
/// leaf-to-root and stop at the first window with a matching selection.
/// Every selector on that window receives the form; selectors on higher
/// ancestors do not receive duplicate copies.
fn xi2_route_window_for_source(
    state: &ServerState,
    target: ResourceId,
    form: Xi2PointerForm,
    evtype: u16,
    slave_deviceid: Option<u16>,
) -> Option<ResourceId> {
    let mut window = target;
    for _ in 0..256 {
        if state.clients.values().any(|client| {
            xi2_form_selected_on_for_source(client, window, form, evtype, slave_deviceid)
        }) {
            return Some(window);
        }
        if window == ROOT_WINDOW {
            break;
        }
        window = state.resources.parent_of(window)?;
    }
    None
}

fn xi2_form_targets_for_source(
    state: &ServerState,
    window: ResourceId,
    form: Xi2PointerForm,
    evtype: u16,
    slave_deviceid: Option<u16>,
) -> Vec<ClientId> {
    state
        .clients
        .iter()
        .filter_map(|(id, client)| {
            xi2_form_selected_on_for_source(client, window, form, evtype, slave_deviceid)
                .then_some(ClientId(*id))
        })
        .collect()
}

/// The one XI2 form a crossing is delivered in. Xorg computes Enter/Leave
/// only for a device that owns a sprite: `ProcessDeviceEvent` calls
/// `CheckMotion` only `if (IsMaster(device) || IsFloating(device))`
/// (`Xi/exevents.c:1854`), and `CheckMotion` passes the slave as `sourceid`
/// to `DoEnterLeaveEvents` (`dix/events.c:3244-3252`). So a crossing caused by
/// an attached slave carries deviceid = master, sourceid = slave; only a
/// floating slave gets crossings stamped with its own id.
fn xi2_crossing_form(xi_source: PointerXiSource) -> (Xi2PointerForm, u16) {
    match xi_source.slave_deviceid {
        Some(slave) if xi_source.attached_master.is_none() => (Xi2PointerForm::Slave, slave),
        _ => (Xi2PointerForm::Master, XI2_MASTER_POINTER_DEVICE_ID),
    }
}

/// Recipients of an XI2 Enter/Leave on `window` — Xorg
/// `DeviceEnterLeaveEvent` (`dix/events.c:4866`): under an XI2 grab only the
/// grab client, through the grab's mask; otherwise the selections on
/// `window` for the crossing's one device form (`xi2_crossing_form`), never
/// propagated.
fn xi2_crossing_targets_for_source(
    state: &ServerState,
    window: ResourceId,
    evtype: u16,
    xi_source: PointerXiSource,
) -> Vec<ClientId> {
    match state.active_pointer_grab {
        Some(grab) if grab.via_xi2 => client_target_id(state, grab.owner)
            .filter(|_| grab.xi2_mask & (1 << evtype) != 0)
            .into_iter()
            .collect(),
        _ => {
            let (form, _) = xi2_crossing_form(xi_source);
            xi2_form_targets_for_source(state, window, form, evtype, xi_source.slave_deviceid)
        }
    }
}

/// The clients that selected the master pointer's XI2 `evtype` on `window`.
pub(crate) fn xi2_master_selectors(
    state: &ServerState,
    window: ResourceId,
    evtype: u16,
) -> Vec<ClientId> {
    xi2_form_targets_for_source(state, window, Xi2PointerForm::Master, evtype, None)
}

fn compute_xi2_targets_for_source(
    state: &ServerState,
    target: yserver_protocol::x11::ResourceId,
    _top_level_id: yserver_protocol::x11::ResourceId,
    xi2_evtype: u16,
    slave_deviceid: Option<u16>,
    slave_press_delivery: Option<&SlavePressDelivery>,
) -> Vec<ClientId> {
    let mut xi2_targets: Vec<ClientId> = Vec::new();
    if xi2_evtype == 0 {
        return xi2_targets;
    }
    let forms = if slave_deviceid.is_some() {
        [Some(Xi2PointerForm::Slave), Some(Xi2PointerForm::Master)]
    } else {
        [None, Some(Xi2PointerForm::Master)]
    };
    for form in forms.into_iter().flatten() {
        let route = if form == Xi2PointerForm::Slave {
            match slave_press_delivery {
                Some(SlavePressDelivery::Xi2 { window, recipients }) => {
                    Some((*window, recipients.clone()))
                }
                Some(SlavePressDelivery::Xi1 { .. } | SlavePressDelivery::None) => None,
                None => {
                    xi2_route_window_for_source(state, target, form, xi2_evtype, slave_deviceid)
                        .map(|window| {
                            (
                                window,
                                xi2_form_targets_for_source(
                                    state,
                                    window,
                                    form,
                                    xi2_evtype,
                                    slave_deviceid,
                                ),
                            )
                        })
                }
            }
        } else {
            xi2_route_window_for_source(state, target, form, xi2_evtype, slave_deviceid).map(
                |window| {
                    (
                        window,
                        xi2_form_targets_for_source(
                            state,
                            window,
                            form,
                            xi2_evtype,
                            slave_deviceid,
                        ),
                    )
                },
            )
        };
        if let Some((_window, recipients)) = route {
            for cid in recipients {
                if !xi2_targets.contains(&cid) {
                    xi2_targets.push(cid);
                }
            }
        }
    }
    xi2_targets
}

fn compute_xi2_raw_targets(
    state: &ServerState,
    raw_evtype: u16,
    xi_source: PointerXiSource,
) -> (Vec<ClientId>, Vec<ClientId>) {
    let mut slave_targets = Vec::new();
    let mut master_targets = Vec::new();
    for (cid_u32, c) in state.clients.iter() {
        let raw_bit = 1 << raw_evtype;
        if let Some(sourceid) = xi_source.slave_deviceid {
            let root_mask = xi2_mask_for_client(c, ROOT_WINDOW, ROOT_WINDOW, &[sourceid, 0]);
            if root_mask & raw_bit != 0 {
                slave_targets.push(ClientId(*cid_u32));
            }
        }
        // mieq.c::mieqCopyDeviceEvent returns without producing a master
        // copy for IsFloating(sdev) (xserver/mi/mieq.c:397-398). Keep raw
        // slave events for the floating device, but do not advertise them
        // through the former paired master.
        if xi_source.slave_deviceid.is_none() || xi_source.attached_master.is_some() {
            let root_mask = xi2_mask_for_client(
                c,
                ROOT_WINDOW,
                ROOT_WINDOW,
                &[XI2_MASTER_POINTER_DEVICE_ID, 1, 0],
            );
            if root_mask & raw_bit != 0 {
                master_targets.push(ClientId(*cid_u32));
            }
        }
    }
    (slave_targets, master_targets)
}

/// The XI2 `deviceid` an event delivered to `cid` carries, given the
/// device that client selected on for this `(target, top_level)` pair.
/// A specific-slave selection yields the slave id; a master /
/// `XIAllMasterDevices` / `XIAllDevices` selection yields the master id.
/// Mirrors `xi2_mask_for_client`'s window/device precedence.
///
/// Two uses: it picks the deviceid stamped on the delivered XI2 event,
/// and it decides whether that XI2 event *duplicates* the core event
/// (only the master form does) and must therefore shadow core delivery.
fn xi2_stamp_deviceid_for_source(
    state: &ServerState,
    cid: ClientId,
    target: ResourceId,
    _top_level: ResourceId,
    evtype: u16,
    slave_deviceid: Option<u16>,
) -> u16 {
    let Some(client) = state.clients.get(&cid.0) else {
        return XI2_MASTER_POINTER_DEVICE_ID;
    };
    if xi2_route_window_for_source(
        state,
        target,
        Xi2PointerForm::Master,
        evtype,
        slave_deviceid,
    )
    .is_some_and(|window| {
        xi2_form_selected_on_for_source(
            client,
            window,
            Xi2PointerForm::Master,
            evtype,
            slave_deviceid,
        )
    }) {
        return XI2_MASTER_POINTER_DEVICE_ID;
    }
    if let Some(slave_deviceid) = slave_deviceid
        && xi2_route_window_for_source(
            state,
            target,
            Xi2PointerForm::Slave,
            evtype,
            Some(slave_deviceid),
        )
        .is_some_and(|window| {
            xi2_form_selected_on_for_source(
                client,
                window,
                Xi2PointerForm::Slave,
                evtype,
                Some(slave_deviceid),
            )
        })
    {
        return slave_deviceid;
    }
    XI2_MASTER_POINTER_DEVICE_ID
}

/// Which stamped forms of a pointer XI2 event `cid` should receive for
/// `evtype`, mirroring Xorg's per-device delivery: the MASTER form if it
/// selected under the concrete master pointer, `XIAllMasterDevices(1)`,
/// or `XIAllDevices(0)`; the SLAVE form if it selected under the concrete
/// slave pointer or `XIAllDevices(0)`. Returns `(wants_master, wants_slave)`.
///
/// A single client can want BOTH (the `XIAllDevices(0)` idiom SDL3 uses):
/// SDL3 parses smooth scroll ONLY off the slave-stamped motion (its
/// handler gates on `deviceid == sourceid`), so a master-only delivery
/// leaves it unable to scroll (issue #72), while pointer position + the
/// GDK-style clients read the master form.
fn xi2_pointer_forms_for_source(
    state: &ServerState,
    cid: ClientId,
    target: ResourceId,
    _top_level: ResourceId,
    evtype: u16,
    slave_deviceid: Option<u16>,
    slave_press_delivery: Option<&SlavePressDelivery>,
) -> (bool, bool) {
    let Some(client) = state.clients.get(&cid.0) else {
        return (false, false);
    };
    let master = xi2_route_window_for_source(
        state,
        target,
        Xi2PointerForm::Master,
        evtype,
        slave_deviceid,
    )
    .is_some_and(|window| {
        xi2_form_selected_on_for_source(
            client,
            window,
            Xi2PointerForm::Master,
            evtype,
            slave_deviceid,
        )
    });
    let slave = match slave_press_delivery {
        Some(SlavePressDelivery::Xi2 { recipients, .. }) => recipients.contains(&cid),
        Some(SlavePressDelivery::Xi1 { .. } | SlavePressDelivery::None) => false,
        None => slave_deviceid.is_some_and(|device_id| {
            xi2_route_window_for_source(
                state,
                target,
                Xi2PointerForm::Slave,
                evtype,
                Some(device_id),
            )
            .is_some_and(|window| {
                xi2_form_selected_on_for_source(
                    client,
                    window,
                    Xi2PointerForm::Slave,
                    evtype,
                    Some(device_id),
                )
            })
        }),
    };
    (master, slave)
}

fn barrier_xi2_targets(state: &ServerState, window: ResourceId, evtype: u16) -> Vec<ClientId> {
    state
        .clients
        .iter()
        .filter_map(|(id, client)| {
            // Match the device-candidate order the normal pointer fanout
            // uses: a client may have selected under the concrete master
            // pointer OR the `XIAllMasterDevices` (1) / `XIAllDevices` (0)
            // wildcards. Querying only the concrete master id missed
            // wildcard selections (e.g. libXi's `XIAllMasterDevices`), so
            // BarrierHit/Leave never reached the client — leaving the
            // pointer pinned at the wall with no way to release it.
            let mask = xi2_mask_for_client(
                client,
                window,
                window,
                &[XI2_MASTER_POINTER_DEVICE_ID, 1, 0],
            );
            ((mask & (1 << evtype)) != 0).then_some(ClientId(*id))
        })
        .collect()
}

pub(crate) fn emit_barrier_event(
    state: &mut ServerState,
    barrier_xid: u32,
    barrier_owner: ClientId,
    barrier_window: ResourceId,
    evtype: u16,
    time: u32,
    eventid: u32,
    dtime: u32,
    flags: u32,
    sourceid: u16,
    root_x: i32,
    root_y: i32,
    dx: f64,
    dy: f64,
) -> Vec<ClientId> {
    if state.resources.window(barrier_window).is_none() {
        return Vec::new();
    }
    let mut flags = flags;
    if state.active_pointer_grab.is_some() {
        flags |= 0x0000_0002;
    }

    let grabbed_targets =
        active_grab_target(state).and_then(|(grab_window, grab_client, _, _, _, _, _)| {
            (grab_client == barrier_owner && grab_window == barrier_window)
                .then_some(vec![grab_client])
        });
    let targets =
        grabbed_targets.unwrap_or_else(|| barrier_xi2_targets(state, barrier_window, evtype));
    if targets.is_empty() {
        return Vec::new();
    }
    fanout_event_to_clients(state, &targets, |buf, seq, order| {
        let _ = x11::write_xi_barrier_event(
            buf,
            order,
            seq,
            XI2_MAJOR_OPCODE,
            evtype,
            XI2_MASTER_POINTER_DEVICE_ID,
            time,
            eventid,
            ROOT_WINDOW.0,
            barrier_window.0,
            barrier_xid,
            dtime,
            flags,
            sourceid,
            root_x,
            root_y,
            dx,
            dy,
        );
    })
}

#[allow(clippy::too_many_arguments)]
fn encode_pointer_event(
    buf: &mut Vec<u8>,
    order: yserver_protocol::x11::ClientByteOrder,
    kind: PointerEventKind,
    seq: SequenceNumber,
    detail: u8,
    time: u32,
    target_window: yserver_protocol::x11::ResourceId,
    child: yserver_protocol::x11::ResourceId,
    event: HostPointerEvent,
    event_x: i16,
    event_y: i16,
    focus: bool,
) {
    let pointer = x11::PointerEvent {
        sequence: seq,
        detail,
        time,
        root: ROOT_WINDOW,
        event: target_window,
        child,
        root_x: event.root_x,
        root_y: event.root_y,
        event_x,
        event_y,
        state: event.state,
    };
    match kind {
        PointerEventKind::ButtonPress => x11::encode_button_press_event(buf, order, pointer),
        PointerEventKind::ButtonRelease => x11::encode_button_release_event(buf, order, pointer),
        PointerEventKind::MotionNotify => x11::encode_motion_notify_event(
            buf,
            order,
            x11::PointerEvent {
                detail: 0,
                ..pointer
            },
        ),
        // For Crossing events, `child` and `detail` come from the
        // producer (HostPointerEvent), which has the spec-correct
        // values computed by `crossings::normal_mode_crossings` /
        // `implicit_grab_crossings`. The fanout-walk's
        // `propagation_child` is the right value for Button/Motion
        // (where it identifies the immediate descendant of the
        // propagation target on the path to the source), but NOT for
        // crossings — crossing `child` per X11 spec is per-event in
        // the chain (None on endpoints, the next inferior on virtual
        // intermediates) and the propagation walk can't know which is
        // which.
        PointerEventKind::EnterNotify => x11::encode_enter_notify_event(
            buf,
            order,
            x11::CrossingEvent {
                sequence: seq,
                time,
                root: ROOT_WINDOW,
                event: target_window,
                child: yserver_protocol::x11::ResourceId(event.child),
                root_x: event.root_x,
                root_y: event.root_y,
                event_x,
                event_y,
                state: event.state,
                detail: event.detail,
                mode: event.crossing_mode,
                focus,
            },
        ),
        PointerEventKind::LeaveNotify => x11::encode_leave_notify_event(
            buf,
            order,
            x11::CrossingEvent {
                sequence: seq,
                time,
                root: ROOT_WINDOW,
                event: target_window,
                child: yserver_protocol::x11::ResourceId(event.child),
                root_x: event.root_x,
                root_y: event.root_y,
                event_x,
                event_y,
                state: event.state,
                detail: event.detail,
                mode: event.crossing_mode,
                focus,
            },
        ),
    }
}

fn merge_dropped(into: &mut Vec<ClientId>, more: Vec<ClientId>) {
    for cid in more {
        if !into.contains(&cid) {
            into.push(cid);
        }
    }
}
