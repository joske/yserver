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
/// `_devicePresence` class is selected on a window. Xorg's
/// `SendEventToAllWindows` walks the root and descendants and tests each
/// window's selection (`Xi/selectev.c:69-111,141-180`,
/// `Xi/exevents.c:3279-3312`, `dix/devices.c:333-346`).
pub fn emit_xi1_device_presence(
    state: &mut ServerState,
    id: u16,
    change: DevicePresenceChange,
) -> Vec<ClientId> {
    if !(6..=127).contains(&id) {
        return Vec::new();
    }
    let targets: Vec<(ClientId, ResourceId)> = state
        .clients
        .iter()
        .flat_map(|(client_id, client)| {
            client
                .xi1_window_event_classes
                .iter()
                .filter_map(move |(window, classes)| {
                    classes
                        .contains(&super::XI1_DEVICE_PRESENCE_CLASS)
                        .then_some((ClientId(*client_id), *window))
                })
        })
        .collect();
    if targets.is_empty() {
        return Vec::new();
    }

    let time = state.timestamp_now();
    let device_id = u8::try_from(id).expect("physical XI device ids fit in one byte");
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
                    x11::encode_xi1_device_presence_notify_event(
                        buf,
                        order,
                        sequence,
                        XI1_DEVICE_PRESENCE_EVENT_TYPE,
                        time,
                        change as u8,
                        device_id,
                    );
                },
            )
            .into_iter()
            .map(|client| client.0),
        );
    }
    disconnected.into_iter().map(ClientId).collect()
}

/// Store and publish one `Device Enabled` change through the same property
/// emitter used by XI1/XI2 property requests. Callers invoke this after
/// committing the registry's enabled fact and before the presence event.
pub fn emit_device_enabled_property_change(
    state: &mut ServerState,
    id: u16,
    enabled: bool,
) -> Vec<ClientId> {
    let Some(what) = state.xi_update_device_enabled_property(id, enabled) else {
        return Vec::new();
    };
    let property = state.xi_device_enabled_atom;
    crate::core_loop::process_request::emit_property_change(state, id, property, what)
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
    Pointer {
        data: crate::xinput::query::XiQueryClassData,
        sourceid: u16,
        shape: super::XiClassShape,
    },
    Keyboard {
        sourceid: u16,
    },
}

fn snapshot_device_classes(
    state: &mut ServerState,
    class_device_id: u16,
    data_device_id: u16,
) -> Option<DeviceClassSnapshot> {
    let device = state.xi_devices.device(class_device_id)?;
    let sourceid = device.class_sourceid;
    let shape = device.class_shape;
    match shape {
        super::XiClassShape::CorePointer | super::XiClassShape::PhysicalPointer => {
            Some(DeviceClassSnapshot::Pointer {
                data: crate::core_loop::fanout::pointer_class_data_for_device(
                    state,
                    data_device_id,
                ),
                sourceid,
                shape,
            })
        }
        super::XiClassShape::Keyboard => Some(DeviceClassSnapshot::Keyboard { sourceid }),
    }
}

fn encode_device_classes(
    classes: DeviceClassSnapshot,
    byte_order: ClientByteOrder,
) -> (Vec<u8>, u16) {
    match classes {
        DeviceClassSnapshot::Pointer {
            data,
            sourceid,
            shape,
        } => crate::xinput::query::build_pointer_classes_for_shape(
            byte_order, sourceid, data, shape, false,
        ),
        DeviceClassSnapshot::Keyboard { sourceid } => {
            crate::xinput::query::build_key_classes(byte_order, sourceid)
        }
    }
}

/// Build one master DeviceChanged class block from the classes retained on
/// that master. Xorg `ChangeMasterDeviceClasses` deep-copies classes before
/// sending its event (`Xi/exevents.c:765-795`); query serialization reads the
/// copied source IDs (`Xi/xiquerydevice.c:278,326`).
pub(crate) fn device_changed_class_block(
    state: &mut ServerState,
    master_id: u16,
    byte_order: ClientByteOrder,
) -> Option<(Vec<u8>, u16)> {
    let sourceid = state.xi_devices.device(master_id)?.class_sourceid;
    let data_device_id = state
        .xi_devices
        .device(sourceid)
        .map_or(master_id, |device| device.id);
    let snapshot = snapshot_device_classes(state, master_id, data_device_id)?;
    Some(encode_device_classes(snapshot, byte_order))
}

/// Emit an XI2 `XI_DeviceChanged` event on `id`. For a master, use its stored
/// class shape/source ID and the indicated source's current class values; for
/// a slave, use its own classes. Xorg copies classes before sending the
/// master event (`Xi/exevents.c:765-795`). Selection may be on any window for the
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
    let is_master = matches!(
        id_role,
        XiDeviceRole::MasterPointer | XiDeviceRole::MasterKeyboard
    );
    let (class_device_id, event_sourceid, data_device_id) = if is_master {
        let Some(master) = state.xi_devices.device(id) else {
            return Vec::new();
        };
        (id, master.class_sourceid, sourceid)
    } else {
        (sourceid, sourceid, sourceid)
    };
    let Some(source_classes) = snapshot_device_classes(state, class_device_id, data_device_id)
    else {
        return Vec::new();
    };
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
                    let (classes, num_classes) = encode_device_classes(source_classes, order);
                    x11::encode_xi2_device_changed_event(
                        buf,
                        order,
                        sequence,
                        XI2_MAJOR_OPCODE,
                        id,
                        time,
                        num_classes,
                        event_sourceid,
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

/// Publish one XI2 hierarchy step to every window selecting it on
/// XIAllDevices. Xorg tests selection at each root/descendant window during
/// `SendEventToAllWindows` (`Xi/xichangehierarchy.c:119`,
/// `Xi/exevents.c:3279-3312`).
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
    changed_id: u16,
    removed: Option<&XiDevice>,
    descriptor_snapshot: Option<x11::XiHierarchyInfo>,
) -> Vec<ClientId> {
    if state.xi_devices.device(changed_id).is_none()
        && removed.is_none_or(|device| device.id != changed_id)
    {
        return Vec::new();
    }

    let step_flag = change.flag();
    let mut infos =
        Vec::with_capacity(state.xi_devices.devices().len() + usize::from(removed.is_some()));

    for device in state.xi_devices.devices() {
        if device.id == changed_id && change == XiHierarchyStep::SlaveRemoved {
            continue;
        }
        let info = if device.id == changed_id {
            descriptor_snapshot.unwrap_or_else(|| {
                let (use_, attachment) = hierarchy_descriptor(state, device.id);
                x11::XiHierarchyInfo {
                    device_id: device.id,
                    attachment,
                    use_,
                    enabled: device.enabled,
                    flags: 0,
                }
            })
        } else {
            let (use_, attachment) = hierarchy_descriptor(state, device.id);
            x11::XiHierarchyInfo {
                device_id: device.id,
                attachment,
                use_,
                enabled: device.enabled,
                flags: 0,
            }
        };
        infos.push(x11::XiHierarchyInfo {
            flags: if device.id == changed_id {
                step_flag
            } else {
                0
            },
            ..info
        });
    }

    if let Some(device) = removed {
        infos.push(x11::XiHierarchyInfo {
            device_id: device.id,
            attachment: 0,
            use_: 0,
            enabled: false,
            flags: step_flag,
        });
    }

    let targets: Vec<(ClientId, ResourceId)> = state
        .clients
        .iter()
        .flat_map(|(id, client)| {
            client
                .xi2_masks
                .iter()
                .filter_map(move |(&(window, device_id), &mask)| {
                    (device_id == 0 && (mask & XI_HIERARCHY_CHANGED_MASK_WIDE) != 0)
                        .then_some((ClientId(*id), window))
                })
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
            fanout_event_to_clients(
                state,
                std::slice::from_ref(&client_id),
                |buf, sequence, order| {
                    x11::encode_xi2_hierarchy_changed_event(
                        buf,
                        order,
                        sequence,
                        XI2_MAJOR_OPCODE,
                        time,
                        &infos,
                    );
                },
            )
            .into_iter()
            .map(|client| client.0),
        );
    }
    disconnected.into_iter().map(ClientId).collect()
}

/// Capture one live hierarchy descriptor for the lifecycle publisher to pass
/// back explicitly when emitting XIDeviceDisabled. The caller marks the
/// snapshot disabled after committing that fact and before floating the live
/// facet.
pub fn hierarchy_device_snapshot(
    state: &ServerState,
    device_id: u16,
) -> Option<x11::XiHierarchyInfo> {
    let device = state.xi_devices.device(device_id)?;
    let (use_, attachment) = hierarchy_descriptor(state, device_id);
    Some(x11::XiHierarchyInfo {
        device_id,
        attachment,
        use_,
        enabled: device.enabled,
        flags: 0,
    })
}

/// Publish the Xorg-ordered disabled transition for a physical facet whose
/// enabled fact has already been committed. The still-attached descriptor is
/// captured before the live facet is floated.
pub fn publish_facet_disabled(state: &mut ServerState, device_id: u16) {
    let Some(snapshot) = hierarchy_device_snapshot(state, device_id) else {
        return;
    };

    let _dropped = emit_device_enabled_property_change(state, device_id, false);
    let _dropped = emit_xi1_device_presence(state, device_id, DevicePresenceChange::Disabled);
    let _dropped = emit_xi_hierarchy_changed(
        state,
        XiHierarchyStep::DeviceDisabled,
        device_id,
        None,
        Some(snapshot),
    );
    state.xi_float_disabled_device(device_id);
}

/// Publish the Xorg-ordered enabled transition for a physical facet whose
/// enabled fact has already been committed.
pub fn publish_facet_enabled(state: &mut ServerState, device_id: u16) {
    let _dropped = emit_device_enabled_property_change(state, device_id, true);
    let _dropped = emit_xi1_device_presence(state, device_id, DevicePresenceChange::Enabled);
    let _dropped =
        emit_xi_hierarchy_changed(state, XiHierarchyStep::DeviceEnabled, device_id, None, None);
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

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, HashSet, VecDeque},
        io::Read,
        os::unix::net::UnixStream,
        sync::{Arc, Mutex, atomic::AtomicU16},
    };

    use super::*;
    use crate::{
        core_loop::{DeviceInfo, client_io},
        resources::ROOT_WINDOW,
        server::ClientState,
        xinput::{InputCapabilities, InputSourceId, XI1_DEVICE_PRESENCE_CLASS},
    };

    /// A client selecting XI2 hierarchy (XIAllDevices) and XI1
    /// DevicePresence on the root.
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
                xi2_masks: HashMap::from([(
                    (ROOT_WINDOW, 0),
                    u64::from(XI2_HIERARCHY_CHANGED_MASK),
                )]),
                xi1_event_classes: HashSet::new(),
                xi1_window_event_classes: HashMap::from([(
                    ROOT_WINDOW,
                    HashSet::from([XI1_DEVICE_PRESENCE_CLASS]),
                )]),
                outbound: VecDeque::new(),
                watching_writable: false,
                write_failed: false,
                output_held: false,
                focused_window: ROOT_WINDOW,
                reader_control: None,
                is_local: true,
                fd_passing: true,
            },
        );
        b
    }

    /// Hotplug family: a hierarchy / DevicePresence subscriber over
    /// `OUTBOUND_CAP` must not stay connected having missed a removal
    /// (the id is reused by the next device), so it is flagged for the
    /// core loop to disconnect; the reading subscriber gets both events.
    #[test]
    fn overflowing_hierarchy_subscriber_is_flagged_on_device_removal() {
        let mut state = ServerState::new();
        let source = InputSourceId(0xA71);
        let info = DeviceInfo {
            source_id: source,
            enabled: true,
            resume_key: None,
            capabilities: InputCapabilities {
                keyboard: false,
                pointer: true,
                touch: false,
            },
            name: "test mouse".to_owned(),
            device_node: "/dev/input/event-test".to_owned(),
            sysname: "event-test".to_owned(),
            vendor_id: 1,
            product_id: 2,
            is_touchpad: false,
            config: Default::default(),
        };
        let id = state.xi_register_source(&info)[0];
        let _slow = install_client(&mut state, 1);
        let mut fast = install_client(&mut state, 2);
        client_io::saturate_for_test(state.clients.get_mut(&1).unwrap());

        let removed = state.xi_unregister_facet(id).expect("registered facet");
        let presence = emit_xi1_device_presence(&mut state, id, DevicePresenceChange::Removed);
        let hierarchy = emit_xi_hierarchy_changed(
            &mut state,
            XiHierarchyStep::SlaveRemoved,
            id,
            Some(&removed),
            None,
        );
        assert_eq!(presence, [ClientId(1)]);
        assert_eq!(hierarchy, [ClientId(1)]);
        assert_eq!(state.clients[&1].outbound.len(), client_io::OUTBOUND_CAP);
        assert_eq!(client_io::failed_writers(&state.clients), [ClientId(1)]);

        fast.set_nonblocking(true).unwrap();
        let mut bytes = vec![0u8; 4096];
        let n = fast.read(&mut bytes).unwrap();
        assert!(n > 64, "presence + hierarchy, got {n} bytes");
        assert_eq!(bytes[32], 35, "the hierarchy GenericEvent follows");
    }
}
