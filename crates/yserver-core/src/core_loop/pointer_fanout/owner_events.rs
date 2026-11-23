//! Grab-owner selection, XI1 selection order, and fallback routing.

use super::{
    ClientId, PointerReplay, PointerXiSource, ResourceId, ServerState,
    XI2_MASTER_POINTER_DEVICE_ID, Xi2PointerForm, clamp_grab_coord, xi2_child_toward,
    xi2_form_targets_for_source, xi2_route_window_for_source,
};

/// Resolves XI1 event selectors in the shared per-window extension-entry
/// order so routing and implicit-grab owner selection follow Xorg.
pub(super) fn xi1_window_selectors(
    state: &ServerState,
    window: ResourceId,
    evcode: u8,
    deviceid: u16,
) -> Vec<ClientId> {
    xi1_window_selectors_with_grab_class(state, window, evcode, deviceid)
        .into_iter()
        .map(|(client, _)| client)
        .collect()
}

/// Returns the canonical input-client entries that select an XI1 event and
/// whether that same delivering entry selects DeviceButtonGrab.
pub(super) fn xi1_window_selectors_with_grab_class(
    state: &ServerState,
    window: ResourceId,
    evcode: u8,
    deviceid: u16,
) -> Vec<(ClientId, bool)> {
    let class = (u32::from(deviceid) << 8) | u32::from(evcode);
    let grab_class =
        (u32::from(deviceid) << 8) | u32::from(crate::xinput::XI1_DEVICE_BUTTON_GRAB_CLASS);
    state
        .xi_input_clients
        .get(&window)
        .into_iter()
        .flatten()
        .filter(|entry| {
            !entry.server_owned_placeholder && state.clients.contains_key(&entry.client.0)
        })
        .filter_map(|entry| {
            let classes = entry.xi1_masks.get(&deviceid)?;
            classes
                .contains(&class)
                .then_some((entry.client, classes.contains(&grab_class)))
        })
        .collect()
}

/// Converts an XI1 event type into the bit used by an implicit-grab mask.
pub(super) fn xi1_event_mask_bit(event_type: u8) -> u32 {
    let Some(offset) = event_type.checked_sub(crate::server::XI_FIRST_EVENT) else {
        return 0;
    };
    if offset < 32 { 1_u32 << offset } else { 0 }
}

/// Returns Xorg's sticky `inputEvents` aggregate for the implicit-grab window.
pub(super) fn xi1_device_mask_on_window(
    state: &ServerState,
    window: ResourceId,
    deviceid: u16,
) -> u32 {
    state
        .xi1_window_input_events
        .get(&window)
        .and_then(|per_device| per_device.get(&deviceid))
        .copied()
        .map(|mask| u32::try_from(mask).expect("XI1 event mask fits 32 bits"))
        .unwrap_or_default()
}

/// One recipient/form decision for smooth-scroll update and stop motions.
pub(super) struct Xi2MotionTarget {
    pub(super) client: ClientId,
    pub(super) form: Xi2PointerForm,
    pub(super) deviceid: u16,
    pub(super) window: ResourceId,
    pub(super) child: ResourceId,
    pub(super) x: i16,
    pub(super) y: i16,
}

/// Resolve smooth-scroll Motion recipients independently for each XI2 device
/// form. A grab owns its form's decision; an ungrabbed form keeps normal
/// selection propagation.
pub(super) fn xi2_motion_targets_for_source(
    state: &ServerState,
    source: PointerXiSource,
    target: ResourceId,
    nested: ResourceId,
    event_x: i16,
    event_y: i16,
    root_x: i16,
    root_y: i16,
    source_accepted: bool,
    master_accepted: bool,
    replay: Option<PointerReplay>,
) -> Vec<Xi2MotionTarget> {
    const XI_MOTION: u16 = 6;
    if !source_accepted && !master_accepted {
        return Vec::new();
    }

    let mut forms = Vec::with_capacity(2);
    if let Some(replay) = replay {
        match state.xi_devices.role(replay.device) {
            Some(crate::xinput::XiDeviceRole::SlavePointer) if source_accepted => {
                forms.push((Xi2PointerForm::Slave, replay.device));
            }
            Some(crate::xinput::XiDeviceRole::MasterPointer) if master_accepted => {
                forms.push((Xi2PointerForm::Master, replay.device));
            }
            _ => {}
        }
    } else {
        if source_accepted && let Some(deviceid) = source.slave_deviceid {
            forms.push((Xi2PointerForm::Slave, deviceid));
        }
        if master_accepted && source.attached_master == Some(XI2_MASTER_POINTER_DEVICE_ID) {
            forms.push((Xi2PointerForm::Master, XI2_MASTER_POINTER_DEVICE_ID));
        }
    }

    let mut targets = Vec::new();
    for (form, deviceid) in forms {
        if let Some(grab) = grab_for_form(state, source, form, replay) {
            let natural = if grab.owner_events {
                owner_events_route(
                    state,
                    form,
                    source.slave_deviceid,
                    grab.owner,
                    target,
                    event_x,
                    event_y,
                    0,
                    Some(XI_MOTION),
                    None,
                )
            } else {
                OwnerEventsRoute::Fallback
            };
            if let OwnerEventsRoute::Natural {
                protocol: OwnerProtocol::Xi2,
                window,
                x,
                y,
                child,
            } = natural
            {
                targets.push(Xi2MotionTarget {
                    client: grab.owner,
                    form,
                    deviceid,
                    window,
                    child,
                    x,
                    y,
                });
                continue;
            }
            if grab.via_xi2
                && grab.xi2_mask & (1_u64 << XI_MOTION) != 0
                && let Some((grab_window, _, gx, gy, ..)) =
                    super::active_pointer_grab_target(state, grab)
            {
                let child = xi2_child_toward(state, grab_window, target);
                targets.push(Xi2MotionTarget {
                    client: grab.owner,
                    form,
                    deviceid,
                    window: grab_window,
                    child,
                    x: clamp_grab_coord(root_x, gx),
                    y: clamp_grab_coord(root_y, gy),
                });
            }
            // A grab suppresses foreign natural recipients for this form,
            // even when its mask does not include Motion.
            continue;
        }

        let Some(window) =
            xi2_route_window_for_source(state, target, form, XI_MOTION, source.slave_deviceid)
        else {
            continue;
        };
        let child = xi2_child_toward(state, window, target);
        let (x, y) = if window == nested {
            (event_x, event_y)
        } else {
            let (window_x, window_y) = state.resources.window_absolute_position(window);
            (
                clamp_grab_coord(root_x, window_x),
                clamp_grab_coord(root_y, window_y),
            )
        };
        for client in
            xi2_form_targets_for_source(state, window, form, XI_MOTION, source.slave_deviceid)
        {
            targets.push(Xi2MotionTarget {
                client,
                form,
                deviceid,
                window,
                child,
                x,
                y,
            });
        }
    }
    targets
}

/// Filtering action for XI1 selections under an XI2 slave grab.
#[derive(Clone, Copy)]
pub(super) enum Xi1SelectionFilter {
    Unrestricted,
    Client(ClientId),
    Suppress,
}

pub(super) fn xi1_selection_filter_for_slave_grab(
    state: &ServerState,
    q: &crate::server::Xi1QueuedEvent,
) -> (
    Xi1SelectionFilter,
    Option<(crate::server::ActivePointerGrab, OwnerEventsRoute)>,
) {
    let Some(grab) = state.xi2_pointer_grabs.get(&q.deviceid).copied() else {
        return (Xi1SelectionFilter::Unrestricted, None);
    };
    if !grab.via_xi2 {
        return (Xi1SelectionFilter::Unrestricted, None);
    }
    let first = crate::server::XI_FIRST_EVENT;
    let evtype = match q.evcode {
        code if code == first + crate::xinput::XI_DEVICE_BUTTON_PRESS_OFFSET => 4,
        code if code == first + crate::xinput::XI_DEVICE_BUTTON_RELEASE_OFFSET => 5,
        code if code == first + crate::xinput::XI_DEVICE_MOTION_NOTIFY_OFFSET => 6,
        _ => return (Xi1SelectionFilter::Unrestricted, None),
    };
    let class = (u32::from(q.deviceid) << 8) | u32::from(q.evcode);
    let (window_x, window_y) = state.resources.window_absolute_position(q.natural_target);
    let route = if grab.owner_events {
        owner_events_route(
            state,
            Xi2PointerForm::Slave,
            Some(q.deviceid),
            grab.owner,
            q.natural_target,
            clamp_grab_coord(q.root_x, window_x),
            clamp_grab_coord(q.root_y, window_y),
            0,
            Some(evtype),
            Some(class),
        )
    } else {
        OwnerEventsRoute::Fallback
    };
    let filter = if !grab.owner_events {
        Xi1SelectionFilter::Suppress
    } else {
        match route {
            OwnerEventsRoute::Natural {
                protocol: OwnerProtocol::Xi1,
                ..
            } => Xi1SelectionFilter::Client(grab.owner),
            OwnerEventsRoute::Natural { .. } | OwnerEventsRoute::Fallback => {
                Xi1SelectionFilter::Suppress
            }
        }
    };
    (filter, Some((grab, route)))
}

pub(in crate::core_loop) fn xtest_wire_valuators(
    axes: crate::server::Xi1MotionAxes,
    completed_position: Option<(i16, i16)>,
) -> (u8, Vec<i32>) {
    // Xorg's FakeInput path writes explicit values at their global axis
    // index, then copies the range from a zero-based slice (C18;
    // `../xserver/Xext/xtest.c:248-284`,
    // `../xserver/dix/inpututils.c:452-462`). Match the resulting wire
    // valuators, including its nonzero-first-axis behavior. Xorg then writes
    // the completed pointer position back to present X/Y axes after motion
    // accumulation and confinement (`../xserver/dix/getevents.c:786,976`,
    // `../xserver/dix/eventconvert.c:738`).
    let count = usize::from(axes.count.min(6));
    let first = usize::from(axes.first);
    let values = (0..count)
        .map(
            |output_offset| match (first + output_offset, completed_position) {
                (0, Some((x, _))) => i32::from(x),
                (1, Some((_, y))) => i32::from(y),
                _ => output_offset
                    .checked_sub(first)
                    .filter(|request_offset| *request_offset < count)
                    .map_or(0, |request_offset| axes.values[request_offset]),
            },
        )
        .collect();
    (axes.first, values)
}

/// Return the grab attached to this device form. A master grab controls the
/// master copy; only a grab keyed by the slave id controls the slave copy.
pub(super) fn grab_for_form(
    state: &ServerState,
    source: PointerXiSource,
    form: Xi2PointerForm,
    replay: Option<PointerReplay>,
) -> Option<crate::server::ActivePointerGrab> {
    let slave_grab = |device_id| {
        state
            .xi2_pointer_grabs
            .get(&device_id)
            .copied()
            .or_else(|| {
                state
                    .xi1_active_grabs
                    .get(&device_id)
                    .filter(|grab| grab.implicit)
                    .map(|grab| crate::server::ActivePointerGrab {
                        owner: grab.owner,
                        grab_window: grab.grab_window,
                        event_mask: 0,
                        cursor: ResourceId(0),
                        time: state.xi1_last_grab_time,
                        owner_events: grab.owner_events,
                        via_xi2: false,
                        implicit: true,
                        passive: false,
                        xi2_mask: 0,
                    })
            })
    };
    if let Some(replay) = replay {
        return match (state.xi_devices.role(replay.device), form) {
            (Some(crate::xinput::XiDeviceRole::MasterPointer), Xi2PointerForm::Master) => {
                state.active_pointer_grab
            }
            (Some(crate::xinput::XiDeviceRole::SlavePointer), Xi2PointerForm::Slave) => {
                slave_grab(replay.device)
            }
            _ => None,
        };
    }
    match form {
        Xi2PointerForm::Master => (source.sourceid == XI2_MASTER_POINTER_DEVICE_ID
            || source.attached_master == Some(XI2_MASTER_POINTER_DEVICE_ID))
        .then_some(state.active_pointer_grab)
        .flatten(),
        Xi2PointerForm::Slave => source.slave_deviceid.and_then(slave_grab),
    }
}

#[derive(Clone, Copy)]
pub(super) enum OwnerProtocol {
    Xi2,
    Xi1,
    Core,
}

#[derive(Clone, Copy)]
pub(super) enum OwnerEventsRoute {
    Natural {
        window: ResourceId,
        x: i16,
        y: i16,
        child: ResourceId,
        protocol: OwnerProtocol,
    },
    Fallback,
}

/// Resolve one owner-events attempt at each window in Xorg's protocol
/// order: XI2, XI1, then core (`../xserver/dix/events.c:2279-2313,
/// 2895-2930`). A rejected selector blocks propagation only after all
/// protocols have had an attempt on that window.
pub(super) fn owner_events_route(
    state: &ServerState,
    form: Xi2PointerForm,
    slave_deviceid: Option<u16>,
    grab_client: ClientId,
    start: ResourceId,
    start_x: i16,
    start_y: i16,
    core_mask: u32,
    xi2_evtype: Option<u16>,
    xi1_mask: Option<u32>,
) -> OwnerEventsRoute {
    let mut current = start;
    let mut x = start_x;
    let mut y = start_y;
    let mut child: Option<ResourceId> = None;
    for _ in 0..256 {
        let mut rejected = false;
        if let Some(evtype) = xi2_evtype {
            let subscribers =
                xi2_form_targets_for_source(state, current, form, evtype, slave_deviceid);
            if !subscribers.is_empty() {
                if subscribers.contains(&grab_client) {
                    return OwnerEventsRoute::Natural {
                        window: current,
                        x,
                        y,
                        child: child.unwrap_or(ResourceId(0)),
                        protocol: OwnerProtocol::Xi2,
                    };
                }
                rejected = true;
            }
        }
        if form == Xi2PointerForm::Slave
            && let (Some(device_id), Some(class)) = (slave_deviceid, xi1_mask)
        {
            let class_device = (class >> 8) as u16;
            if class_device == device_id {
                let subscribers =
                    xi1_window_selectors(state, current, (class & 0xff) as u8, device_id);
                if !subscribers.is_empty() {
                    if subscribers.contains(&grab_client) {
                        return OwnerEventsRoute::Natural {
                            window: current,
                            x,
                            y,
                            child: child.unwrap_or(ResourceId(0)),
                            protocol: OwnerProtocol::Xi1,
                        };
                    }
                    rejected = true;
                }
            }
        }
        if form == Xi2PointerForm::Master && core_mask != 0 {
            let subscribers =
                crate::core_loop::fanout::subscribers_by_id(state, current, core_mask);
            if !subscribers.is_empty() {
                if subscribers.contains(&grab_client) {
                    return OwnerEventsRoute::Natural {
                        window: current,
                        x,
                        y,
                        child: child.unwrap_or(ResourceId(0)),
                        protocol: OwnerProtocol::Core,
                    };
                }
                rejected = true;
            }
        }
        if rejected {
            return OwnerEventsRoute::Fallback;
        }
        let Some(window) = state.resources.window(current) else {
            return OwnerEventsRoute::Fallback;
        };
        if window.parent == current {
            return OwnerEventsRoute::Fallback;
        }
        // #133 step 8: the inverse of the hit-test walk's
        // `to_content_coords`, border term included.
        (x, y) = window.to_parent_coords(x, y);
        child = Some(current);
        current = window.parent;
    }
    OwnerEventsRoute::Fallback
}
