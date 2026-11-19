//! Grab-owner selection and fallback routing shared by pointer fanout and
//! XI 1.x direct FakeInput delivery.

use super::{
    ClientId, PointerXiSource, ROOT_WINDOW, ResourceId, ServerState, XI2_MAJOR_OPCODE,
    XI2_MASTER_POINTER_DEVICE_ID, Xi2PointerForm, clamp_grab_coord, fanout_event_to_clients, x11,
    xi1_window_selectors, xi2_child_toward, xi2_form_targets_for_source,
    xi2_route_window_for_source,
};

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
) -> Vec<Xi2MotionTarget> {
    const XI_MOTION: u16 = 6;
    if !source_accepted {
        return Vec::new();
    }

    let mut forms = Vec::with_capacity(2);
    if let Some(deviceid) = source.slave_deviceid {
        forms.push((Xi2PointerForm::Slave, deviceid));
    }
    if source.attached_master == Some(XI2_MASTER_POINTER_DEVICE_ID) {
        forms.push((Xi2PointerForm::Master, XI2_MASTER_POINTER_DEVICE_ID));
    }

    let mut targets = Vec::new();
    for (form, deviceid) in forms {
        if let Some(grab) = grab_for_form(state, source, form) {
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

pub(super) fn xi2_direct_xtest_motion(
    state: &mut ServerState,
    q: &crate::server::Xi1QueuedEvent,
    grab: crate::server::ActivePointerGrab,
    route: OwnerEventsRoute,
) -> Vec<ClientId> {
    const XI_MOTION: u16 = 6;
    // Xorg's FakeInput path writes explicit values at their global axis
    // index, then copies the range from a zero-based slice (C18;
    // `../xserver/Xext/xtest.c:248-284`,
    // `../xserver/dix/inpututils.c:452-462`). Match the resulting wire
    // valuators, including its nonzero-first-axis behavior.
    let motion_valuators = q.axes.map(|axes| {
        let count = usize::from(axes.count.min(6));
        let first = usize::from(axes.first);
        let values = (0..count)
            .map(|output_offset| {
                output_offset
                    .checked_sub(first)
                    .filter(|request_offset| *request_offset < count)
                    .map_or(0, |request_offset| axes.values[request_offset])
            })
            .collect::<Vec<_>>();
        (axes.first, values)
    });
    let (window, child, event_x, event_y) = match route {
        OwnerEventsRoute::Natural {
            window,
            x,
            y,
            child,
            protocol: OwnerProtocol::Xi2,
        } => (window, child, x, y),
        OwnerEventsRoute::Fallback if grab.xi2_mask & (1_u64 << XI_MOTION) != 0 => {
            let (window_x, window_y) = state.resources.window_absolute_position(grab.grab_window);
            let child = xi2_child_toward(state, grab.grab_window, q.natural_target);
            (
                grab.grab_window,
                child,
                clamp_grab_coord(q.root_x, window_x),
                clamp_grab_coord(q.root_y, window_y),
            )
        }
        OwnerEventsRoute::Natural { .. } | OwnerEventsRoute::Fallback => return Vec::new(),
    };
    fanout_event_to_clients(
        state,
        std::slice::from_ref(&grab.owner),
        |buf, seq, order| {
            x11::encode_xi2_device_event(
                buf,
                order,
                seq,
                XI2_MAJOR_OPCODE,
                XI_MOTION,
                q.deviceid,
                q.time,
                ROOT_WINDOW,
                window,
                child,
                q.root_x,
                q.root_y,
                event_x,
                event_y,
                q.state_mask,
                0,
                q.deviceid,
                0,
                motion_valuators
                    .as_ref()
                    .map(|(first, values)| (*first, values.as_slice())),
            );
        },
    )
}

/// Return the grab attached to this device form. A master grab controls the
/// master copy; only a grab keyed by the slave id controls the slave copy.
pub(super) fn grab_for_form(
    state: &ServerState,
    source: PointerXiSource,
    form: Xi2PointerForm,
) -> Option<crate::server::ActivePointerGrab> {
    match form {
        Xi2PointerForm::Master => (source.sourceid == XI2_MASTER_POINTER_DEVICE_ID
            || source.attached_master == Some(XI2_MASTER_POINTER_DEVICE_ID))
        .then_some(state.active_pointer_grab)
        .flatten(),
        Xi2PointerForm::Slave => source
            .slave_deviceid
            .and_then(|device_id| state.xi2_pointer_grabs.get(&device_id).copied()),
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
