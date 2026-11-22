//! Held-button transitions of a pointer source and of its master.

use super::{
    PointerButtonTransition, PointerXiSource, ServerState, XI2_MASTER_POINTER_DEVICE_ID,
    XI2_XTEST_POINTER_DEVICE_ID,
};

pub(super) fn pointer_button_transition(
    state: &mut ServerState,
    origin: crate::core_loop::InputOrigin,
    xi_source: PointerXiSource,
    button: u8,
    pressed: bool,
    apply_master: bool,
) -> PointerButtonTransition {
    if !(1..=16).contains(&button) {
        return PointerButtonTransition {
            source_accepted: true,
            master_accepted: true,
            master_changed: false,
        };
    }

    let bit = 1u16 << (button - 1);
    let unpublished_source = match (origin, xi_source.slave_deviceid) {
        (crate::core_loop::InputOrigin::Physical(source_id), None) => Some(source_id),
        _ => None,
    };
    let had_source_state = if let Some(device_id) = xi_source.slave_deviceid {
        state
            .xi_devices
            .device(device_id)
            .is_some_and(|device| device.buttons_down & bit != 0)
    } else if let Some(source_id) = unpublished_source {
        state
            .unpublished_pointer_buttons_down
            .get(&source_id)
            .is_some_and(|buttons| buttons & bit != 0)
    } else {
        state.buttons_down & bit != 0
    };

    if pressed == had_source_state {
        return PointerButtonTransition {
            source_accepted: false,
            master_accepted: false,
            master_changed: false,
        };
    }

    if let Some(device_id) = xi_source.slave_deviceid {
        if let Some(device) = state.xi_devices.device_mut(device_id) {
            if pressed {
                device.buttons_down |= bit;
            } else {
                device.buttons_down &= !bit;
            }
        }
    } else if let Some(source_id) = unpublished_source {
        let buttons = state
            .unpublished_pointer_buttons_down
            .entry(source_id)
            .or_default();
        if pressed {
            *buttons |= bit;
        } else {
            *buttons &= !bit;
            if *buttons == 0 {
                state.unpublished_pointer_buttons_down.remove(&source_id);
            }
        }
    }

    if !apply_master {
        // The master is frozen: its half of the transition runs when its
        // queued part is processed (`master_button_transition`).
        return PointerButtonTransition {
            source_accepted: true,
            master_accepted: false,
            master_changed: false,
        };
    }
    master_button_transition(state, xi_source, bit, pressed, None)
}

/// The master's half of a button transition: the aggregate held-button
/// bitmap over every attached pointer (Xorg's master `ProcessPointerEvent`).
pub(super) fn master_button_transition(
    state: &mut ServerState,
    xi_source: PointerXiSource,
    bit: u16,
    pressed: bool,
    captured_master: Option<u16>,
) -> PointerButtonTransition {
    let attached_to_master = captured_master.map_or_else(
        || {
            xi_source.slave_deviceid.is_none_or(|device_id| {
                state.xi_devices.device(device_id).is_some_and(|device| {
                    device.attached_master == Some(XI2_MASTER_POINTER_DEVICE_ID)
                })
            })
        },
        |device_id| device_id == XI2_MASTER_POINTER_DEVICE_ID,
    );
    if !attached_to_master {
        return PointerButtonTransition {
            source_accepted: true,
            master_accepted: false,
            master_changed: false,
        };
    }

    if pressed {
        if state.buttons_down & bit != 0 {
            return PointerButtonTransition {
                source_accepted: true,
                master_accepted: false,
                master_changed: false,
            };
        }
        state.buttons_down |= bit;
        return PointerButtonTransition {
            source_accepted: true,
            master_accepted: true,
            master_changed: true,
        };
    }

    // Xorg's master button release is aggregated: a slave release reaches
    // that slave, but cannot release the master while any attached slave
    // still holds the mapped button.
    if attached_pointer_holds_button(state, bit) {
        PointerButtonTransition {
            source_accepted: true,
            master_accepted: false,
            master_changed: false,
        }
    } else if state.buttons_down & bit != 0 {
        state.buttons_down &= !bit;
        PointerButtonTransition {
            source_accepted: true,
            master_accepted: true,
            master_changed: true,
        }
    } else {
        PointerButtonTransition {
            source_accepted: true,
            master_accepted: false,
            master_changed: false,
        }
    }
}

fn attached_pointer_holds_button(state: &ServerState, bit: u16) -> bool {
    state.xi_devices.devices().iter().any(|device| {
        device.enabled
            && device.attached_master == Some(XI2_MASTER_POINTER_DEVICE_ID)
            && (device.id == XI2_XTEST_POINTER_DEVICE_ID
                || device.facet == Some(crate::xinput::XiFacetKind::PointerTouch))
            && device.buttons_down & bit != 0
    }) || state
        .unpublished_pointer_buttons_down
        .iter()
        .any(|(source_id, buttons)| {
            buttons & bit != 0
                && state
                    .xi_devices
                    .source(*source_id)
                    .is_some_and(|info| info.enabled && info.capabilities.pointer)
        })
}
