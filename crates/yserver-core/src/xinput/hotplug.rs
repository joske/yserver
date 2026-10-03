//! XI2 hierarchy and device-change notifications for physical sources.

use std::collections::HashSet;

use yserver_protocol::x11::{self, ClientByteOrder, ClientId, ResourceId};

use crate::{core_loop::fanout::fanout_event_to_clients, server::ServerState};

use super::{XI2_DEVICE_CHANGED_MASK, XI2_HIERARCHY_CHANGED_MASK, XiDevice, XiDeviceRole};

const XI2_MAJOR_OPCODE: u8 = 137;
const XI_HIERARCHY_CHANGED_MASK_WIDE: u64 = XI2_HIERARCHY_CHANGED_MASK as u64;
const XI_DEVICE_CHANGED_MASK_WIDE: u64 = XI2_DEVICE_CHANGED_MASK as u64;
const XI_SLAVE_SWITCH: u8 = 1;
const XI_DEVICE_CHANGE: u8 = 2;
const XI1_DEVICE_PRESENCE_EVENT_TYPE: u8 =
    crate::server::XI_FIRST_EVENT + super::XI_DEVICE_PRESENCE_NOTIFY_OFFSET;

/// One XI1 device-list transition, matching Xorg's `DevicePresenceNotify`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DevicePresenceChange {
    Added = 0,
    Removed = 1,
    Enabled = 2,
    Disabled = 3,
}

/// Emit an XI1 `DevicePresenceNotify` for one physical facet. Device 256's
/// `_devicePresence` class is a global selection, retained canonically per
/// window and aggregated on `xi1_event_classes` like other server-wide XI1
/// notifications.
pub fn emit_xi1_device_presence(
    state: &mut ServerState,
    id: u16,
    change: DevicePresenceChange,
) -> Vec<ClientId> {
    if !(6..=127).contains(&id) {
        return Vec::new();
    }
    let targets: Vec<ClientId> = state
        .clients
        .iter()
        .filter_map(|(client_id, client)| {
            client
                .xi1_event_classes
                .contains(&super::XI1_DEVICE_PRESENCE_CLASS)
                .then_some(ClientId(*client_id))
        })
        .collect();
    if targets.is_empty() {
        return Vec::new();
    }

    let time = state.timestamp_now();
    let device_id = u8::try_from(id).expect("physical XI device ids fit in one byte");
    crate::core_loop::fanout::fanout_event_to_clients(state, &targets, |buf, sequence, order| {
        x11::encode_xi1_device_presence_notify_event(
            buf,
            order,
            sequence,
            XI1_DEVICE_PRESENCE_EVENT_TYPE,
            time,
            change as u8,
            device_id,
        );
    })
}

const XI_SLAVE_ADDED: u32 = 1 << 2;
const XI_SLAVE_REMOVED: u32 = 1 << 3;
const XI_DEVICE_ENABLED: u32 = 1 << 6;
const XI_DEVICE_DISABLED: u32 = 1 << 7;

/// One Xorg-compatible hierarchy transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XiHierarchyStep {
    SlaveAdded,
    DeviceEnabled,
    DeviceDisabled,
    SlaveRemoved,
}

/// XI2.h:107-108 (`XISlaveSwitch = 1`, `XIDeviceChange = 2`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum XiDeviceChangeReason {
    DeviceChange = XI_DEVICE_CHANGE,
    SlaveSwitch = XI_SLAVE_SWITCH,
}

#[derive(Clone, Copy)]
enum DeviceClassSnapshot {
    Pointer(crate::xinput::query::XiQueryClassData),
    Keyboard,
}

fn snapshot_device_classes(state: &mut ServerState, sourceid: u16) -> Option<DeviceClassSnapshot> {
    match state.xi_devices.role(sourceid)? {
        XiDeviceRole::MasterPointer | XiDeviceRole::SlavePointer => {
            Some(DeviceClassSnapshot::Pointer(
                crate::core_loop::fanout::pointer_class_data_for_device(state, sourceid),
            ))
        }
        XiDeviceRole::MasterKeyboard | XiDeviceRole::SlaveKeyboard => {
            Some(DeviceClassSnapshot::Keyboard)
        }
    }
}

fn encode_device_classes(
    classes: DeviceClassSnapshot,
    sourceid: u16,
    byte_order: ClientByteOrder,
) -> (Vec<u8>, u16) {
    match classes {
        DeviceClassSnapshot::Pointer(data) => {
            crate::xinput::query::build_pointer_classes(byte_order, sourceid, data)
        }
        DeviceClassSnapshot::Keyboard => {
            crate::xinput::query::build_key_classes(byte_order, sourceid)
        }
    }
}

/// Build one DeviceChanged class block using the same encoders as
/// XIQueryDevice. The caller chooses the event device and reason.
pub(crate) fn device_changed_class_block(
    state: &mut ServerState,
    sourceid: u16,
    byte_order: ClientByteOrder,
) -> Option<(Vec<u8>, u16)> {
    let snapshot = snapshot_device_classes(state, sourceid)?;
    Some(encode_device_classes(snapshot, sourceid, byte_order))
}

/// Emit an XI2 `XI_DeviceChanged` event on `id`, using the classes and
/// source identity of `sourceid`. Selection may be on any window for the
/// exact event device, XIAllDevices, or XIAllMasterDevices when `id` is a
/// master, matching Xorg's `SendEventToAllWindows` (Xi/exevents.c:760).
/// Returns clients whose output buffers overflowed.
pub fn emit_xi2_device_changed(
    state: &mut ServerState,
    id: u16,
    reason: XiDeviceChangeReason,
    sourceid: u16,
) -> Vec<ClientId> {
    let Some(id_role) = state.xi_devices.role(id) else {
        return Vec::new();
    };
    let Some(source_classes) = snapshot_device_classes(state, sourceid) else {
        return Vec::new();
    };
    let is_master = matches!(
        id_role,
        XiDeviceRole::MasterPointer | XiDeviceRole::MasterKeyboard
    );
    // Xorg SendEventToAllWindows delivers at the root and recursively at
    // each selected child (Xi/exevents.c:3283-3292). DeviceChanged has no
    // window field, but each selected window still produces a delivery.
    let targets: Vec<(ClientId, ResourceId)> = state
        .clients
        .iter()
        .flat_map(|(client_id, client)| {
            let selected_windows: HashSet<ResourceId> = client
                .xi2_masks
                .iter()
                .filter_map(|(&(window, device_id), &mask)| {
                    ((device_id == id || device_id == 0 || (is_master && device_id == 1))
                        && mask & XI_DEVICE_CHANGED_MASK_WIDE != 0)
                        .then_some(window)
                })
                .collect();
            selected_windows
                .into_iter()
                .map(|window| (ClientId(*client_id), window))
                .collect::<Vec<_>>()
        })
        .collect();
    if targets.is_empty() {
        return Vec::new();
    }

    let time = state.timestamp_now();
    let mut disconnected = HashSet::new();
    for (client_id, _selected_window) in targets {
        if disconnected.contains(&client_id.0) {
            continue;
        }
        disconnected.extend(
            crate::core_loop::fanout::fanout_event_to_clients(
                state,
                std::slice::from_ref(&client_id),
                |buf, sequence, order| {
                    let (classes, num_classes) =
                        encode_device_classes(source_classes, sourceid, order);
                    x11::encode_xi2_device_changed_event(
                        buf,
                        order,
                        sequence,
                        XI2_MAJOR_OPCODE,
                        id,
                        time,
                        num_classes,
                        sourceid,
                        reason as u8,
                        &classes,
                    );
                },
            )
            .into_iter()
            .map(|client| client.0),
        );
    }
    disconnected.into_iter().map(ClientId).collect()
}

/// Record and announce the first attached-slave event after the master
/// changes source. The state guard rejects disabled, floating, mismatched,
/// or master-only device identities.
pub fn announce_xi2_slave_switch(
    state: &mut ServerState,
    master_id: u16,
    sourceid: u16,
) -> Vec<ClientId> {
    if state.xi_record_last_slave(master_id, sourceid) {
        emit_xi2_device_changed(
            state,
            master_id,
            XiDeviceChangeReason::SlaveSwitch,
            sourceid,
        )
    } else {
        Vec::new()
    }
}

impl XiHierarchyStep {
    const fn flag(self) -> u32 {
        match self {
            Self::SlaveAdded => XI_SLAVE_ADDED,
            Self::DeviceEnabled => XI_DEVICE_ENABLED,
            Self::DeviceDisabled => XI_DEVICE_DISABLED,
            Self::SlaveRemoved => XI_SLAVE_REMOVED,
        }
    }
}

/// Publish one XI2 hierarchy step to clients selecting it on XIAllDevices.
///
/// The event contains every currently live device, with flags set only on
/// `changed_ids`. A removal event additionally appends the removed facet
/// descriptors with Xorg's removed-device wire fields (`enabled = false`,
/// `use = 0`, and attachment zero). Callers must remove facets before the
/// `SlaveRemoved` step and retain their snapshots until this function queues
/// the notification.
pub fn emit_xi_hierarchy_changed(
    state: &mut ServerState,
    change: XiHierarchyStep,
    changed_ids: &[u16],
    removed: &[XiDevice],
) -> Vec<ClientId> {
    if changed_ids.is_empty() {
        return Vec::new();
    }

    let changed: HashSet<u16> = changed_ids.iter().copied().collect();
    let step_flag = change.flag();
    let mut infos = Vec::with_capacity(
        state.xi_devices.devices().len()
            + if change == XiHierarchyStep::SlaveRemoved {
                removed.len()
            } else {
                0
            },
    );

    for device in state.xi_devices.devices() {
        let (use_, attachment) = hierarchy_descriptor(state, device.id);
        infos.push(x11::XiHierarchyInfo {
            device_id: device.id,
            attachment,
            use_,
            enabled: device.enabled,
            flags: if changed.contains(&device.id) {
                step_flag
            } else {
                0
            },
        });
    }

    if change == XiHierarchyStep::SlaveRemoved {
        let mut removed_in_id_order: Vec<&XiDevice> = removed
            .iter()
            .filter(|device| changed.contains(&device.id))
            .collect();
        removed_in_id_order.sort_unstable_by_key(|device| device.id);
        for device in removed_in_id_order {
            infos.push(x11::XiHierarchyInfo {
                device_id: device.id,
                attachment: 0,
                use_: 0,
                enabled: false,
                flags: step_flag,
            });
        }
    }

    let targets: Vec<ClientId> = state
        .clients
        .iter()
        .filter_map(|(id, client)| {
            client
                .xi2_masks
                .iter()
                .any(|((_, device_id), mask)| {
                    *device_id == 0 && (mask & XI_HIERARCHY_CHANGED_MASK_WIDE) != 0
                })
                .then_some(ClientId(*id))
        })
        .collect();
    if targets.is_empty() {
        return Vec::new();
    }

    let time = state.timestamp_now();
    fanout_event_to_clients(state, &targets, |buf, sequence, byte_order| {
        x11::encode_xi2_hierarchy_changed_event(
            buf,
            byte_order,
            sequence,
            XI2_MAJOR_OPCODE,
            time,
            &infos,
        );
    })
}

/// Return the same use/attachment pair as Xorg's GetDeviceUse
/// (Xi/xiquerydevice.c:518-529), shared by XIQueryDevice and hierarchy data.
fn hierarchy_descriptor(state: &ServerState, device_id: u16) -> (u8, u16) {
    match state.xi_devices.role(device_id) {
        Some(XiDeviceRole::MasterPointer) => (
            1, // XIMasterPointer
            crate::xinput::DEVICEID_MASTER_KEYBOARD,
        ),
        Some(XiDeviceRole::MasterKeyboard) => (
            2, // XIMasterKeyboard
            crate::xinput::DEVICEID_MASTER_POINTER,
        ),
        Some(XiDeviceRole::SlavePointer) => state
            .xi_devices
            .attachment(device_id)
            .map_or((5, 0), |master| (3, master)), // XISlavePointer / XIFloatingSlave
        Some(XiDeviceRole::SlaveKeyboard) => state
            .xi_devices
            .attachment(device_id)
            .map_or((5, 0), |master| (4, master)), // XISlaveKeyboard / XIFloatingSlave
        None => (0, 0),
    }
}
