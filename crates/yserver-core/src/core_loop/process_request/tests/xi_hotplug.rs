use super::*;

// -----------------------------------------------------------------
// XI 1.x `SelectExtensionEvent` + `DevicePropertyNotify` fan-out.
// -----------------------------------------------------------------

const XI1_DEVICE_PRESENCE_CLASS: u32 = 256 << 8;
const XI1_DEVICE_PRESENCE_EVENT_TYPE: u8 =
    crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_PRESENCE_NOTIFY_OFFSET;

fn xi2_hierarchy_wire_event_type() -> u8 {
    u8::try_from(crate::xinput::XI2_HIERARCHY_CHANGED_EVENT_TYPE)
        .expect("XI2 event types fit in u8")
}

fn xi_hotplug_hierarchy_selection_body(window: ResourceId, selected: bool) -> Vec<u8> {
    let mut body = Vec::with_capacity(20);
    body.extend_from_slice(&window.0.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes()); // num_masks
    body.extend_from_slice(&0u16.to_le_bytes()); // pad
    body.extend_from_slice(&0u16.to_le_bytes()); // XIAllDevices
    body.extend_from_slice(&1u16.to_le_bytes()); // four-byte mask
    body.extend_from_slice(&[0, if selected { 0x08 } else { 0 }, 0, 0]);
    body
}

fn xi_hotplug_select_hierarchy(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    client: u32,
    sequence: u16,
    window: ResourceId,
    selected: bool,
) {
    let body = xi_hotplug_hierarchy_selection_body(window, selected);
    xi_hotplug_dispatch(state, backend, client, sequence, 137, 46, &body);
}

fn xi_hotplug_select_presence(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    client: u32,
    sequence: u16,
    window: ResourceId,
) {
    let body = xi1_select_extension_event_body(window.0, &[XI1_DEVICE_PRESENCE_CLASS]);
    xi_hotplug_dispatch(state, backend, client, sequence, 137, 6, &body);
}

fn xi_hotplug_set_enabled(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    client: u32,
    sequence: u16,
    device_id: u16,
    enabled: bool,
) {
    let body = xi2_change_property_body(
        device_id,
        crate::xinput::XI_PROP_MODE_REPLACE,
        8,
        state.xi_device_enabled_atom.0,
        crate::xinput::XA_INTEGER.0,
        &[u8::from(enabled)],
    );
    xi_hotplug_dispatch(state, backend, client, sequence, 137, 57, &body);
}

fn xi_hotplug_device_events(wire: &[u8], event_type: u8) -> Vec<&[u8]> {
    let mut events = Vec::new();
    let mut offset = 0;
    while wire.len().saturating_sub(offset) >= 32 {
        let event_size = if wire[offset] == 35 {
            let length =
                u32::from_le_bytes(wire[offset + 4..offset + 8].try_into().unwrap()) as usize;
            32usize.saturating_add(length.saturating_mul(4))
        } else {
            32
        };
        if event_size > wire.len() - offset {
            break;
        }
        let matches = if event_type == XI1_DEVICE_PRESENCE_EVENT_TYPE {
            wire[offset] == XI1_DEVICE_PRESENCE_EVENT_TYPE
        } else {
            wire[offset] == 35
                && u16::from_le_bytes([wire[offset + 8], wire[offset + 9]]) == u16::from(event_type)
        };
        if matches {
            events.push(&wire[offset..offset + event_size]);
        }
        offset += event_size;
    }
    events
}

#[derive(Debug, PartialEq)]
struct XiHotplugDeviceSnapshot {
    id: u16,
    role: crate::xinput::XiDeviceRole,
    source_id: Option<crate::xinput::InputSourceId>,
    facet: Option<crate::xinput::XiFacetKind>,
    enabled: bool,
    attachment: Option<u16>,
}

#[derive(Debug, PartialEq)]
struct XiHotplugInputStateSnapshot {
    device: u16,
    keys_down: [u8; 32],
    buttons_down: [u8; 32],
    valuator_mode: u8,
    valuators: [i32; 4],
}

#[derive(Debug, PartialEq)]
struct XiHotplugClientSelectionsSnapshot {
    client: u32,
    xi2: Vec<((ResourceId, u16), u64)>,
    xi1: Vec<(ResourceId, Vec<u32>)>,
}

#[derive(Debug, PartialEq)]
struct XiHotplugStateSnapshot {
    registry: Vec<XiHotplugDeviceSnapshot>,
    source_ids: Vec<crate::xinput::InputSourceId>,
    properties: Vec<(
        u16,
        std::collections::BTreeMap<AtomId, crate::xinput::XiProperty>,
    )>,
    buttons_down: u16,
    device_buttons_down: Vec<(u16, u16)>,
    key_down_by_device: Vec<(u16, Vec<(u8, crate::core_loop::InputOrigin)>)>,
    xi1_input_state: Vec<XiHotplugInputStateSnapshot>,
    detached_masters: HashMap<u16, u16>,
    floating_pointer_positions: HashMap<u16, (f32, f32)>,
    client_selections: Vec<XiHotplugClientSelectionsSnapshot>,
    selections: HashMap<AtomId, (ResourceId, u32)>,
}

fn xi_hotplug_state_snapshot(state: &ServerState) -> XiHotplugStateSnapshot {
    let registry = state
        .xi_devices
        .devices()
        .iter()
        .map(|device| XiHotplugDeviceSnapshot {
            id: device.id,
            role: state.xi_devices.role(device.id).expect("registered role"),
            source_id: device.source_id,
            facet: device.facet,
            enabled: device.enabled,
            attachment: state.xi_devices.attachment(device.id),
        })
        .collect();
    let mut key_down_by_device = state
        .key_down_by_device
        .iter()
        .map(|(device, keys)| {
            let mut keys: Vec<_> = keys.iter().map(|(key, origin)| (*key, *origin)).collect();
            keys.sort_unstable_by_key(|(key, _)| *key);
            (*device, keys)
        })
        .collect::<Vec<_>>();
    key_down_by_device.sort_unstable_by_key(|(device, _)| *device);
    let mut xi1_input_state = state
        .xi1_device_input_state
        .iter()
        .map(|(device, input)| XiHotplugInputStateSnapshot {
            device: *device,
            keys_down: input.keys_down,
            buttons_down: input.buttons_down,
            valuator_mode: input.valuator_mode,
            valuators: input.valuators,
        })
        .collect::<Vec<_>>();
    xi1_input_state.sort_unstable_by_key(|input| input.device);
    let mut client_selections = state
        .clients
        .iter()
        .filter_map(|(client_id, client)| {
            let mut xi2: Vec<_> = client
                .xi2_masks
                .iter()
                .map(|(key, mask)| (*key, *mask))
                .collect();
            xi2.sort_unstable_by_key(|((window, device), _)| (window.0, *device));
            let mut xi1: Vec<_> = client
                .xi1_window_event_classes
                .iter()
                .map(|(window, classes)| {
                    let mut classes: Vec<_> = classes.iter().copied().collect();
                    classes.sort_unstable();
                    (*window, classes)
                })
                .collect();
            xi1.sort_unstable_by_key(|(window, _)| window.0);
            (!xi2.is_empty() || !xi1.is_empty()).then_some(XiHotplugClientSelectionsSnapshot {
                client: *client_id,
                xi2,
                xi1,
            })
        })
        .collect::<Vec<_>>();
    client_selections.sort_unstable_by_key(|client| client.client);
    XiHotplugStateSnapshot {
        registry,
        source_ids: state.xi_devices.source_ids(),
        properties: xi_property_snapshot(state),
        buttons_down: state.buttons_down,
        device_buttons_down: state
            .xi_devices
            .devices()
            .iter()
            .map(|device| (device.id, device.buttons_down))
            .collect(),
        key_down_by_device,
        xi1_input_state,
        detached_masters: state.xi2_detached_masters.clone(),
        floating_pointer_positions: state.floating_pointer_positions.clone(),
        client_selections,
        selections: state.selections.clone(),
    }
}

fn xi_hotplug_create_child(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    client: u32,
    sequence: u16,
    child: ResourceId,
) {
    let body = xi_hotplug_create_window_body(child.0, ROOT_WINDOW.0);
    xi_hotplug_dispatch(state, backend, client, sequence, 1, 0, &body);
}

fn xi_hotplug_destroy_child(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    client: u32,
    sequence: u16,
    child: ResourceId,
) {
    xi_hotplug_dispatch(
        state,
        backend,
        client,
        sequence,
        4,
        0,
        &child.0.to_le_bytes(),
    );
}

fn xi_hotplug_finish_clients(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    client_ids: &[u32],
) {
    for client_id in client_ids {
        crate::core_loop::process_disconnect::process_disconnect(
            state,
            backend,
            ClientId(*client_id),
        );
    }
}

#[test]
fn xi_hotplug_per_window_hierarchy_selection_delivers_each_window_copy() {
    // Mutation killed: collect XI_HierarchyChanged targets once per
    // client instead of once per selected window. Xorg walks the root
    // and descendants and delivers separately at each one
    // (Xi/exevents.c:3279-3312; Xi/xichangehierarchy.c:119).
    let mut state = ServerState::new();
    let mut peer1 = install_capture_client(&mut state, 1);
    let mut peer2 = install_capture_client(&mut state, 2);
    let mut backend = RecordingBackend::new();
    let device_id = seed_pointer_for_t3(&mut state);
    // The registry fixture constructs an enabled source directly, so
    // mirror the Device Enabled=1 write that completes production add.
    xi_hotplug_set_enabled(&mut state, &mut backend, 1, 1, device_id, true);
    let before = xi_hotplug_state_snapshot(&state);
    let child = ResourceId(0x10_0001);

    xi_hotplug_create_child(&mut state, &mut backend, 1, 2, child);
    xi_hotplug_select_hierarchy(&mut state, &mut backend, 1, 3, ROOT_WINDOW, true);
    xi_hotplug_select_hierarchy(&mut state, &mut backend, 1, 4, child, true);
    xi_hotplug_select_hierarchy(&mut state, &mut backend, 2, 1, ROOT_WINDOW, true);
    assert!(read_all_available(&mut peer1).is_empty());
    assert!(read_all_available(&mut peer2).is_empty());

    xi_hotplug_set_enabled(&mut state, &mut backend, 1, 5, device_id, false);
    let disabled1 = read_all_available(&mut peer1);
    let disabled2 = read_all_available(&mut peer2);
    let events1 = xi_hotplug_device_events(&disabled1, xi2_hierarchy_wire_event_type());
    let events2 = xi_hotplug_device_events(&disabled2, xi2_hierarchy_wire_event_type());
    assert_eq!(events1.len(), 2, "client 1 selected root and child");
    assert_eq!(events2.len(), 1, "client 2 selected only the root");
    assert_eq!(events1[0], events1[1], "copies carry no window field");

    xi_hotplug_set_enabled(&mut state, &mut backend, 1, 6, device_id, true);
    let enabled1 = read_all_available(&mut peer1);
    let enabled2 = read_all_available(&mut peer2);
    let events1 = xi_hotplug_device_events(&enabled1, xi2_hierarchy_wire_event_type());
    let events2 = xi_hotplug_device_events(&enabled2, xi2_hierarchy_wire_event_type());
    assert_eq!(events1.len(), 2);
    assert_eq!(events2.len(), 1);
    assert_eq!(events1[0], events1[1]);
    assert_eq!(
        state.xi_devices.device(device_id).unwrap().properties[&state.xi_device_enabled_atom].data,
        [1],
        "reenabling restores the Device Enabled property"
    );

    crate::core_loop::process_disconnect::process_disconnect(&mut state, &mut backend, ClientId(1));
    assert_eq!(
        state.xi_devices.device(device_id).unwrap().properties[&state.xi_device_enabled_atom].data,
        [1],
        "client disconnect does not change Device Enabled"
    );
    crate::core_loop::process_disconnect::process_disconnect(&mut state, &mut backend, ClientId(2));
    assert!(state.clients.is_empty());
    assert!(state.resources.window(child).is_none());
    assert_eq!(xi_hotplug_state_snapshot(&state), before);
}

#[test]
fn xi_hotplug_per_window_presence_selection_delivers_each_window_copy() {
    // Mutation killed: fan out XI1 DevicePresenceNotify once per client
    // using the global class set. Xorg records the presence mask on a
    // window (Xi/selectev.c:69-111, 141-180) and walks each window
    // (dix/devices.c:333-346; Xi/exevents.c:3279-3312).
    let mut state = ServerState::new();
    let mut peer1 = install_capture_client(&mut state, 1);
    let mut peer2 = install_capture_client(&mut state, 2);
    let mut backend = RecordingBackend::new();
    let device_id = seed_pointer_for_t3(&mut state);
    xi_hotplug_set_enabled(&mut state, &mut backend, 1, 1, device_id, true);
    let before = xi_hotplug_state_snapshot(&state);
    let child = ResourceId(0x10_0002);

    xi_hotplug_create_child(&mut state, &mut backend, 1, 2, child);
    xi_hotplug_select_presence(&mut state, &mut backend, 1, 3, ROOT_WINDOW);
    xi_hotplug_select_presence(&mut state, &mut backend, 1, 4, child);
    xi_hotplug_select_presence(&mut state, &mut backend, 2, 1, ROOT_WINDOW);
    assert!(read_all_available(&mut peer1).is_empty());
    assert!(read_all_available(&mut peer2).is_empty());

    xi_hotplug_set_enabled(&mut state, &mut backend, 1, 5, device_id, false);
    let disabled1 = read_all_available(&mut peer1);
    let disabled2 = read_all_available(&mut peer2);
    let events1 = xi_hotplug_device_events(&disabled1, XI1_DEVICE_PRESENCE_EVENT_TYPE);
    let events2 = xi_hotplug_device_events(&disabled2, XI1_DEVICE_PRESENCE_EVENT_TYPE);
    assert_eq!(events1.len(), 2, "client 1 selected root and child");
    assert_eq!(events2.len(), 1, "client 2 selected only the root");
    assert_eq!(events1[0], events1[1], "copies carry no window field");

    xi_hotplug_set_enabled(&mut state, &mut backend, 1, 6, device_id, true);
    let enabled1 = read_all_available(&mut peer1);
    let enabled2 = read_all_available(&mut peer2);
    let events1 = xi_hotplug_device_events(&enabled1, XI1_DEVICE_PRESENCE_EVENT_TYPE);
    let events2 = xi_hotplug_device_events(&enabled2, XI1_DEVICE_PRESENCE_EVENT_TYPE);
    assert_eq!(events1.len(), 2);
    assert_eq!(events2.len(), 1);
    assert_eq!(events1[0], events1[1]);

    xi_hotplug_finish_clients(&mut state, &mut backend, &[1, 2]);
    assert!(state.clients.is_empty());
    assert!(state.resources.window(child).is_none());
    assert_eq!(xi_hotplug_state_snapshot(&state), before);
}

#[test]
fn xi_hotplug_per_window_destroyed_child_selection_is_removed() {
    // Mutation killed: retain XI1/XI2 selections when DestroyWindow
    // tears down a selected child. The event before destruction has two
    // copies; afterward only the still-selected root gets one. Xorg
    // removes a window's extension selections during teardown
    // (dix/window.c:998-1000; Xi/exevents.c:3103-3123).
    let mut state = ServerState::new();
    let mut peer1 = install_capture_client(&mut state, 1);
    let mut peer2 = install_capture_client(&mut state, 2);
    let mut backend = RecordingBackend::new();
    let device_id = seed_pointer_for_t3(&mut state);
    xi_hotplug_set_enabled(&mut state, &mut backend, 1, 1, device_id, true);
    let before = xi_hotplug_state_snapshot(&state);
    let child = ResourceId(0x10_0003);

    xi_hotplug_create_child(&mut state, &mut backend, 1, 2, child);
    for (sequence, window) in [(3, ROOT_WINDOW), (4, child)] {
        xi_hotplug_select_hierarchy(&mut state, &mut backend, 1, sequence, window, true);
        xi_hotplug_select_presence(&mut state, &mut backend, 1, sequence + 2, window);
    }
    xi_hotplug_select_hierarchy(&mut state, &mut backend, 2, 1, ROOT_WINDOW, true);
    xi_hotplug_select_presence(&mut state, &mut backend, 2, 2, ROOT_WINDOW);
    assert!(read_all_available(&mut peer1).is_empty());
    assert!(read_all_available(&mut peer2).is_empty());

    xi_hotplug_set_enabled(&mut state, &mut backend, 1, 7, device_id, false);
    let disabled1 = read_all_available(&mut peer1);
    let disabled2 = read_all_available(&mut peer2);
    for (wire, expected) in [(&disabled1, 2), (&disabled2, 1)] {
        assert_eq!(
            xi_hotplug_device_events(wire, xi2_hierarchy_wire_event_type()).len(),
            expected,
            "hierarchy copies match the selected window count"
        );
        assert_eq!(
            xi_hotplug_device_events(wire, XI1_DEVICE_PRESENCE_EVENT_TYPE).len(),
            expected,
            "presence copies match the selected window count"
        );
    }
    let hierarchy_copies = xi_hotplug_device_events(&disabled1, xi2_hierarchy_wire_event_type());
    let presence_copies = xi_hotplug_device_events(&disabled1, XI1_DEVICE_PRESENCE_EVENT_TYPE);
    assert_eq!(hierarchy_copies[0], hierarchy_copies[1]);
    assert_eq!(presence_copies[0], presence_copies[1]);

    xi_hotplug_destroy_child(&mut state, &mut backend, 1, 8, child);
    assert!(state.resources.window(child).is_none());
    assert!(
        !state.clients[&1]
            .xi2_masks
            .keys()
            .any(|(window, _)| *window == child),
        "DestroyWindow drops the child's XI2 selection"
    );
    assert!(
        !state.clients[&1]
            .xi1_window_event_classes
            .contains_key(&child),
        "DestroyWindow drops the child's XI1 selection"
    );
    assert!(read_all_available(&mut peer1).is_empty());
    assert!(read_all_available(&mut peer2).is_empty());

    xi_hotplug_set_enabled(&mut state, &mut backend, 1, 9, device_id, true);
    let enabled1 = read_all_available(&mut peer1);
    let enabled2 = read_all_available(&mut peer2);
    for wire in [&enabled1, &enabled2] {
        assert_eq!(
            xi_hotplug_device_events(wire, xi2_hierarchy_wire_event_type()).len(),
            1,
            "only the root selection remains"
        );
        assert_eq!(
            xi_hotplug_device_events(wire, XI1_DEVICE_PRESENCE_EVENT_TYPE).len(),
            1,
            "only the root selection remains"
        );
    }

    xi_hotplug_finish_clients(&mut state, &mut backend, &[1, 2]);
    assert!(state.clients.is_empty());
    assert_eq!(xi_hotplug_state_snapshot(&state), before);
}
