use super::*;

fn device_enabled_xi_request(
    state: &mut yserver_core::server::ServerState,
    backend: &mut KmsBackend,
    minor: u8,
    sequence: u16,
    body: &[u8],
) {
    use yserver_core::{backend::Backend, core_loop::process_request};

    let outcome = process_request::process_request(
        state,
        backend as &mut dyn Backend,
        yserver_protocol::x11::ClientId(5),
        yserver_protocol::x11::SequenceNumber(sequence),
        yserver_protocol::x11::RequestHeader {
            opcode: 137,
            data: minor,
            length_units: u32::try_from(1 + body.len().div_ceil(4)).unwrap(),
        },
        body,
        None,
    )
    .expect("process XI request through the core dispatcher");
    assert!(
        matches!(outcome, process_request::RequestOutcome::Handled),
        "XI minor {minor} outcome: {outcome:?}"
    );
}

fn device_enabled_request_bytes(
    state: &mut yserver_core::server::ServerState,
    backend: &mut KmsBackend,
    peer: &mut std::os::unix::net::UnixStream,
    minor: u8,
    sequence: u16,
    body: &[u8],
) -> Vec<u8> {
    device_enabled_xi_request(state, backend, minor, sequence, body);
    kbd_map_drain(peer)
}

fn device_enabled_device_property_list_body(device_id: u16) -> [u8; 4] {
    let mut body = [0; 4];
    body[..2].copy_from_slice(&device_id.to_le_bytes());
    body
}

fn device_enabled_get_enabled_xi2(
    state: &mut yserver_core::server::ServerState,
    backend: &mut KmsBackend,
    peer: &mut std::os::unix::net::UnixStream,
    device_id: u16,
    property: u32,
    sequence: u16,
) -> u8 {
    let mut body = Vec::with_capacity(20);
    body.extend_from_slice(&device_id.to_le_bytes());
    body.extend_from_slice(&[0, 0]); // delete=false, pad
    body.extend_from_slice(&property.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // AnyPropertyType
    body.extend_from_slice(&0u32.to_le_bytes()); // offset
    body.extend_from_slice(&100u32.to_le_bytes()); // length
    let reply = device_enabled_request_bytes(state, backend, peer, 59, sequence, &body);
    assert_eq!(reply.len(), 36, "XIGetProperty reply with one byte");
    assert_eq!(reply[0], 1, "reply packet");
    assert_eq!(u32::from_le_bytes(reply[8..12].try_into().unwrap()), 19);
    assert_eq!(u32::from_le_bytes(reply[16..20].try_into().unwrap()), 1);
    assert_eq!(reply[20], 8);
    reply[32]
}

fn device_enabled_get_enabled_xi1(
    state: &mut yserver_core::server::ServerState,
    backend: &mut KmsBackend,
    peer: &mut std::os::unix::net::UnixStream,
    device_id: u16,
    property: u32,
    sequence: u16,
) -> u8 {
    let mut body = Vec::with_capacity(20);
    body.extend_from_slice(&property.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // AnyPropertyType
    body.extend_from_slice(&0u32.to_le_bytes()); // offset
    body.extend_from_slice(&100u32.to_le_bytes()); // length
    body.push(u8::try_from(device_id).unwrap());
    body.push(0); // delete=false
    body.extend_from_slice(&[0; 2]);
    let reply = device_enabled_request_bytes(state, backend, peer, 39, sequence, &body);
    assert_eq!(reply.len(), 36, "XI1 GetDeviceProperty reply with one byte");
    assert_eq!(reply[0], 1, "reply packet");
    assert_eq!(u32::from_le_bytes(reply[8..12].try_into().unwrap()), 19);
    assert_eq!(u32::from_le_bytes(reply[16..20].try_into().unwrap()), 1);
    assert_eq!(reply[20], 8);
    assert_eq!(reply[21], u8::try_from(device_id).unwrap());
    reply[32]
}

fn device_enabled_event_kinds(bytes: &[u8]) -> Vec<&'static str> {
    let mut kinds = Vec::new();
    let mut offset = 0;
    while offset + 32 <= bytes.len() {
        let event_type = bytes[offset] & 0x7f;
        if event_type == 35 {
            let words =
                u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
            let event_len = 32 + words * 4;
            assert!(offset + event_len <= bytes.len(), "complete XI2 event");
            let evtype = u16::from_le_bytes(bytes[offset + 8..offset + 10].try_into().unwrap());
            kinds.push(match evtype {
                11 => "hierarchy",
                12 => "xi2-property",
                _ => "other-xi2",
            });
            offset += event_len;
        } else {
            kinds.push(match event_type {
                81 => "presence",
                82 => "xi1-property",
                _ => "other-xi1",
            });
            offset += 32;
        }
    }
    assert_eq!(offset, bytes.len(), "complete event stream");
    kinds
}

#[test]
fn xi_device_enabled_is_listed_and_read_through_xi1_xi2() {
    use yserver_core::{
        backend::Backend,
        core_loop::{HostInputEvent, process_disconnect::process_disconnect},
        server::ServerState,
        xinput::{
            DEVICEID_MASTER_KEYBOARD, DEVICEID_MASTER_POINTER, DEVICEID_XTEST_KEYBOARD,
            DEVICEID_XTEST_POINTER, InputSourceId, XiFacetKind,
        },
    };

    // Kills physical-only seeding and continuation refreshes that reset Device Enabled.
    const SOURCE: InputSourceId = InputSourceId(0xD3_01);
    let mut state = ServerState::new();
    let original_core_properties =
        [2, 3, 4, 5].map(|id| state.xi_devices.device(id).unwrap().properties.clone());
    let enabled_atom = state.atoms.intern("Device Enabled", false);
    let mut backend = KmsBackend::for_tests();
    let mut peer = kbd_map_client_id(&mut state, 5);

    let mut info = dynamic_test_device(SOURCE, true, false);
    info.enabled = false;
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceAdded(info.clone()),
    );
    let device_id = state
        .xi_devices
        .facet(SOURCE, XiFacetKind::Keyboard)
        .expect("DeviceAdded publishes a disabled keyboard facet");

    let all_devices = [
        DEVICEID_MASTER_POINTER,
        DEVICEID_MASTER_KEYBOARD,
        DEVICEID_XTEST_POINTER,
        DEVICEID_XTEST_KEYBOARD,
        device_id,
    ];
    let mut sequence = 1;
    for id in all_devices {
        let list = device_enabled_device_property_list_body(id);
        let xi2 =
            device_enabled_request_bytes(&mut state, &mut backend, &mut peer, 56, sequence, &list);
        sequence += 1;
        let xi2_count = usize::from(u16::from_le_bytes([xi2[8], xi2[9]]));
        let xi2_atoms = xi2[32..32 + xi2_count * 4]
            .chunks_exact(4)
            .map(|atom| u32::from_le_bytes(atom.try_into().unwrap()))
            .collect::<Vec<_>>();
        assert!(
            xi2_atoms.contains(&enabled_atom.0),
            "XIListProperties lists Device Enabled on {id}"
        );

        let xi1_list = [u8::try_from(id).unwrap(), 0, 0, 0];
        let xi1 = device_enabled_request_bytes(
            &mut state,
            &mut backend,
            &mut peer,
            36,
            sequence,
            &xi1_list,
        );
        sequence += 1;
        let xi1_count = usize::from(u16::from_le_bytes([xi1[8], xi1[9]]));
        let xi1_atoms = xi1[32..32 + xi1_count * 4]
            .chunks_exact(4)
            .map(|atom| u32::from_le_bytes(atom.try_into().unwrap()))
            .collect::<Vec<_>>();
        assert!(
            xi1_atoms.contains(&enabled_atom.0),
            "ListDeviceProperties lists Device Enabled on {id}"
        );

        let expected = u8::from(id != device_id);
        assert_eq!(
            device_enabled_get_enabled_xi2(
                &mut state,
                &mut backend,
                &mut peer,
                id,
                enabled_atom.0,
                sequence
            ),
            expected,
            "XIGetProperty on {id}"
        );
        sequence += 1;
        assert_eq!(
            device_enabled_get_enabled_xi1(
                &mut state,
                &mut backend,
                &mut peer,
                id,
                enabled_atom.0,
                sequence
            ),
            expected,
            "GetDeviceProperty on {id}"
        );
        sequence += 1;
    }

    let physical_properties = state
        .xi_devices
        .device(device_id)
        .unwrap()
        .properties
        .clone();
    // Repeated DeviceAdded for the retained source exercises the live
    // continuation refresh; Device Enabled must remain present exactly
    // once with its disabled byte.
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceAdded(info.clone()),
    );
    assert_eq!(
        state.xi_devices.device(device_id).unwrap().properties,
        physical_properties,
        "continuation refresh preserves the property map"
    );
    assert_eq!(
        device_enabled_get_enabled_xi2(
            &mut state,
            &mut backend,
            &mut peer,
            device_id,
            enabled_atom.0,
            sequence,
        ),
        0,
        "continuation refresh preserves the disabled value"
    );
    sequence += 1;

    let mut resumed = info.clone();
    resumed.enabled = true;
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceResumed(resumed),
    );
    let enabled_properties = state
        .xi_devices
        .device(device_id)
        .unwrap()
        .properties
        .clone();
    let mut resumed_info = info.clone();
    resumed_info.enabled = true;
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceAdded(resumed_info),
    );
    assert_eq!(
        state.xi_devices.device(device_id).unwrap().properties,
        enabled_properties,
        "enabled continuation refresh preserves the property map"
    );
    assert_eq!(
        device_enabled_get_enabled_xi2(
            &mut state,
            &mut backend,
            &mut peer,
            device_id,
            enabled_atom.0,
            sequence
        ),
        1
    );
    sequence += 1;
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceSuspended { source_id: SOURCE },
    );
    assert_eq!(
        device_enabled_get_enabled_xi1(
            &mut state,
            &mut backend,
            &mut peer,
            device_id,
            enabled_atom.0,
            sequence
        ),
        0
    );

    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceRemoved { source_id: SOURCE },
    );
    process_disconnect(&mut state, &mut backend, yserver_protocol::x11::ClientId(5));
    assert!(
        state.clients.is_empty(),
        "disconnect restores the client-selection baseline"
    );
    assert!(state.xi_devices.source_ids().is_empty());
    assert_eq!(
        state
            .xi_devices
            .devices()
            .iter()
            .map(|device| device.id)
            .collect::<Vec<_>>(),
        [2, 3, 4, 5]
    );
    assert_eq!(
        [2, 3, 4, 5].map(|id| state.xi_devices.device(id).unwrap().properties.clone()),
        original_core_properties
    );
    assert!(state.keys_down.iter().all(|byte| *byte == 0));
    assert_eq!(state.buttons_down, 0);
    assert!(state.key_down_by_device.is_empty());
    assert!(state.xi2_detached_masters.is_empty());
    assert!(state.floating_pointer_positions.is_empty());
}

#[test]
fn xi_device_enabled_transitions_notify_before_presence_and_hierarchy() {
    use yserver_core::{
        backend::Backend,
        core_loop::{HostInputEvent, process_disconnect::process_disconnect},
        resources::ROOT_WINDOW,
        server::ServerState,
        xinput::{
            InputSourceId, XI1_DEVICE_PRESENCE_CLASS, XI2_HIERARCHY_CHANGED_MASK,
            XI2_PROPERTY_EVENT_MASK, XiFacetKind,
        },
    };
    use yserver_protocol::x11::ClientId;

    // Kills either transition mutation that omits the Device Enabled property event.
    const SOURCE: InputSourceId = InputSourceId(0xD3_02);
    let mut state = ServerState::new();
    let original_core_properties =
        [2, 3, 4, 5].map(|id| state.xi_devices.device(id).unwrap().properties.clone());
    let mut backend = KmsBackend::for_tests();
    let mut peer = kbd_map_client_id(&mut state, 5);
    let mut sequence = 1u16;
    let select = |state: &mut ServerState,
                  backend: &mut KmsBackend,
                  sequence: u16,
                  minor: u8,
                  body: &[u8]| {
        device_enabled_xi_request(state, backend, minor, sequence, body);
    };

    let mut xi2_select = Vec::new();
    xi2_select.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    xi2_select.extend_from_slice(&1u16.to_le_bytes());
    xi2_select.extend_from_slice(&[0; 2]);
    xi2_select.extend_from_slice(&0u16.to_le_bytes()); // XIAllDevices
    xi2_select.extend_from_slice(&1u16.to_le_bytes()); // one 32-bit mask word
    xi2_select
        .extend_from_slice(&(XI2_PROPERTY_EVENT_MASK | XI2_HIERARCHY_CHANGED_MASK).to_le_bytes());
    select(&mut state, &mut backend, sequence, 46, &xi2_select);
    sequence += 1;

    let mut xi1_presence = Vec::new();
    xi1_presence.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    xi1_presence.extend_from_slice(&1u16.to_le_bytes());
    xi1_presence.extend_from_slice(&[0; 2]);
    xi1_presence.extend_from_slice(&XI1_DEVICE_PRESENCE_CLASS.to_le_bytes());
    select(&mut state, &mut backend, sequence, 6, &xi1_presence);
    sequence += 1;
    assert!(
        kbd_map_drain(&mut peer).is_empty(),
        "selection requests emit no events"
    );

    let mut info = dynamic_test_device(SOURCE, true, false);
    info.enabled = false;
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceAdded(info.clone()),
    );
    let device_id = state
        .xi_devices
        .facet(SOURCE, XiFacetKind::Keyboard)
        .unwrap();
    let added = device_enabled_event_kinds(&kbd_map_drain(&mut peer));
    assert_eq!(
        added,
        ["presence", "hierarchy"],
        "seeding is silent, add still publishes its two lifecycle events"
    );

    let class = (u32::from(device_id) << 8) | 82; // XI_FIRST_EVENT (66) + DevicePropertyNotify offset (16)
    let mut xi1_property = Vec::new();
    xi1_property.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    xi1_property.extend_from_slice(&1u16.to_le_bytes());
    xi1_property.extend_from_slice(&[0; 2]);
    xi1_property.extend_from_slice(&class.to_le_bytes());
    select(&mut state, &mut backend, sequence, 6, &xi1_property);

    let mut resumed = info.clone();
    resumed.enabled = true;
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceResumed(resumed),
    );
    let enabled = device_enabled_event_kinds(&kbd_map_drain(&mut peer));
    assert_eq!(
        enabled,
        ["xi2-property", "xi1-property", "presence", "hierarchy"]
    );

    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceSuspended { source_id: SOURCE },
    );
    let disabled = device_enabled_event_kinds(&kbd_map_drain(&mut peer));
    assert_eq!(
        disabled,
        ["xi2-property", "xi1-property", "presence", "hierarchy"]
    );

    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceRemoved { source_id: SOURCE },
    );
    let _ = kbd_map_drain(&mut peer);
    process_disconnect(&mut state, &mut backend, ClientId(5));
    assert!(
        state.clients.is_empty(),
        "disconnect clears XI1 and XI2 selections"
    );
    assert!(state.xi_devices.source_ids().is_empty());
    assert_eq!(
        state
            .xi_devices
            .devices()
            .iter()
            .map(|device| device.id)
            .collect::<Vec<_>>(),
        [2, 3, 4, 5]
    );
    assert_eq!(
        [2, 3, 4, 5].map(|id| state.xi_devices.device(id).unwrap().properties.clone()),
        original_core_properties
    );
    assert!(state.keys_down.iter().all(|byte| *byte == 0));
    assert_eq!(state.buttons_down, 0);
    assert!(state.key_down_by_device.is_empty());
    assert!(state.xi2_detached_masters.is_empty());
    assert!(state.floating_pointer_positions.is_empty());
}

fn device_enabled_write_body_xi2(
    device_id: u16,
    property: u32,
    format: u8,
    type_atom: u32,
    data: &[u8],
) -> Vec<u8> {
    let mut body = Vec::with_capacity(16 + data.len());
    body.extend_from_slice(&device_id.to_le_bytes());
    body.push(yserver_core::xinput::XI_PROP_MODE_REPLACE);
    body.push(format);
    body.extend_from_slice(&property.to_le_bytes());
    body.extend_from_slice(&type_atom.to_le_bytes());
    let item_size = usize::from(format / 8);
    body.extend_from_slice(&u32::try_from(data.len() / item_size).unwrap().to_le_bytes());
    body.extend_from_slice(data);
    body
}

fn device_enabled_write_body_xi1(
    device_id: u16,
    format: u8,
    mode: u8,
    property: u32,
    type_atom: u32,
    data: &[u8],
) -> Vec<u8> {
    let mut body = Vec::with_capacity(16 + data.len());
    body.extend_from_slice(&property.to_le_bytes());
    body.extend_from_slice(&type_atom.to_le_bytes());
    body.push(u8::try_from(device_id).unwrap());
    body.push(format);
    body.push(mode);
    body.push(0);
    let item_size = usize::from(format / 8);
    body.extend_from_slice(&u32::try_from(data.len() / item_size).unwrap().to_le_bytes());
    body.extend_from_slice(data);
    body
}

fn select_device_enabled_transition_events(
    state: &mut yserver_core::server::ServerState,
    backend: &mut KmsBackend,
    device_id: u16,
    sequence: u16,
) {
    let mask = (1u32 << 2)
        | (1u32 << 3)
        | (1u32 << 4)
        | (1u32 << 5)
        | yserver_core::xinput::XI2_HIERARCHY_CHANGED_MASK
        | yserver_core::xinput::XI2_PROPERTY_EVENT_MASK;
    let mut xi2 = Vec::with_capacity(16);
    xi2.extend_from_slice(&yserver_core::resources::ROOT_WINDOW.0.to_le_bytes());
    xi2.extend_from_slice(&1u16.to_le_bytes());
    xi2.extend_from_slice(&[0; 2]);
    xi2.extend_from_slice(&0u16.to_le_bytes()); // XIAllDevices
    xi2.extend_from_slice(&1u16.to_le_bytes());
    xi2.extend_from_slice(&mask.to_le_bytes());
    device_enabled_xi_request(state, backend, 46, sequence, &xi2);

    let presence_class = yserver_core::xinput::XI1_DEVICE_PRESENCE_CLASS;
    let mut presence = Vec::with_capacity(12);
    presence.extend_from_slice(&yserver_core::resources::ROOT_WINDOW.0.to_le_bytes());
    presence.extend_from_slice(&1u16.to_le_bytes());
    presence.extend_from_slice(&[0; 2]);
    presence.extend_from_slice(&presence_class.to_le_bytes());
    device_enabled_xi_request(state, backend, 6, sequence + 1, &presence);

    let property_class = (u32::from(device_id) << 8) | 82;
    let mut property = Vec::with_capacity(12);
    property.extend_from_slice(&yserver_core::resources::ROOT_WINDOW.0.to_le_bytes());
    property.extend_from_slice(&1u16.to_le_bytes());
    property.extend_from_slice(&[0; 2]);
    property.extend_from_slice(&property_class.to_le_bytes());
    device_enabled_xi_request(state, backend, 6, sequence + 2, &property);
}

fn assert_device_enabled_write_cleanup(
    state: &mut yserver_core::server::ServerState,
    backend: &mut KmsBackend,
    source_id: yserver_core::xinput::InputSourceId,
    original_core_properties: &[std::collections::BTreeMap<yserver_protocol::x11::AtomId, yserver_core::xinput::XiProperty>;
         4],
) {
    use yserver_core::core_loop::{HostInputEvent, process_disconnect::process_disconnect};

    Backend::on_host_input(backend, state, HostInputEvent::DeviceRemoved { source_id });
    process_disconnect(state, backend, yserver_protocol::x11::ClientId(5));
    assert!(state.clients.is_empty(), "XI1/XI2 selections are released");
    assert!(state.xi_devices.source_ids().is_empty());
    assert_eq!(
        state
            .xi_devices
            .devices()
            .iter()
            .map(|device| device.id)
            .collect::<Vec<_>>(),
        [2, 3, 4, 5]
    );
    assert_eq!(
        [2, 3, 4, 5].map(|id| state.xi_devices.device(id).unwrap().properties.clone()),
        *original_core_properties,
        "base-device property maps return to baseline"
    );
    assert!(state.keys_down.iter().all(|byte| *byte == 0));
    assert_eq!(state.buttons_down, 0);
    assert!(state.key_down_by_device.is_empty());
    assert!(state.key_repeats.is_empty());
    assert!(state.unpublished_keyboard_keys_down.is_empty());
    assert!(state.unpublished_pointer_buttons_down.is_empty());
    assert!(state.xi2_detached_masters.is_empty());
    assert!(state.floating_pointer_positions.is_empty());
    assert!(backend.floating_keyboard_states.is_empty());
    assert!(backend.core.pending_pointer_events.is_empty());
}

#[test]
fn xi_client_disabled_facet_stays_disabled_across_vt_round_trip() {
    use yserver_core::{
        backend::Backend,
        core_loop::HostInputEvent,
        server::ServerState,
        xinput::{InputSourceId, XiFacetKind},
    };

    // Mutation killed: clear client_disabled when DeviceResumed arrives,
    // thereby enabling every facet on VT entry. Xorg preserves the
    // XI86_DEVICE_DISABLED flag and skips EnableDevice for that device
    // (xf86Events.c:307-320).
    const SOURCE: InputSourceId = InputSourceId(0xD4_06);
    let mut state = ServerState::new();
    let original_core_properties =
        [2, 3, 4, 5].map(|id| state.xi_devices.device(id).unwrap().properties.clone());
    let mut backend = KmsBackend::for_tests();
    let mut peer = kbd_map_client_id(&mut state, 5);
    let info = dynamic_test_device(SOURCE, true, true);
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceAdded(info.clone()),
    );
    let pointer = state
        .xi_devices
        .facet(SOURCE, XiFacetKind::PointerTouch)
        .expect("mixed source pointer facet");
    let keyboard = state
        .xi_devices
        .facet(SOURCE, XiFacetKind::Keyboard)
        .expect("mixed source keyboard facet");
    select_device_enabled_transition_events(&mut state, &mut backend, pointer, 1);
    let _ = kbd_map_drain(&mut peer);

    let disable = device_enabled_write_body_xi2(
        pointer,
        state.xi_device_enabled_atom.0,
        8,
        yserver_core::xinput::XA_INTEGER.0,
        &[0],
    );
    let disabled =
        device_enabled_request_bytes(&mut state, &mut backend, &mut peer, 57, 4, &disable);
    assert_eq!(
        device_enabled_event_kinds(&disabled),
        [
            "xi2-property",
            "xi1-property",
            "presence",
            "hierarchy",
            "xi2-property",
            "xi1-property"
        ],
        "the real XIChangeProperty request disables the pointer facet"
    );
    assert!(state.xi_devices.device(pointer).unwrap().client_disabled);

    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceSuspended { source_id: SOURCE },
    );
    let suspended = kbd_map_drain(&mut peer);
    assert_eq!(
        device_enabled_event_kinds(&suspended),
        ["xi2-property", "presence", "hierarchy"],
        "VT leave publishes the still-enabled keyboard only"
    );
    let pointer_device = state.xi_devices.device(pointer).unwrap();
    assert!(!pointer_device.enabled);
    assert!(pointer_device.client_disabled);
    assert_eq!(pointer_device.attached_master, None);

    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceResumed(info),
    );
    let resumed = kbd_map_drain(&mut peer);
    assert_eq!(
        device_enabled_event_kinds(&resumed),
        ["xi2-property", "presence", "hierarchy"],
        "VT entry enables the keyboard only, with no pointer-facet events"
    );
    let pointer_device = state.xi_devices.device(pointer).unwrap();
    assert!(!pointer_device.enabled);
    assert!(pointer_device.client_disabled);
    assert_eq!(pointer_device.attached_master, None);
    assert_eq!(
        pointer_device.properties[&state.xi_device_enabled_atom].data,
        [0]
    );
    assert!(state.xi_devices.device(keyboard).unwrap().enabled);

    assert_device_enabled_write_cleanup(
        &mut state,
        &mut backend,
        SOURCE,
        &original_core_properties,
    );
}

#[test]
fn xi_session_disable_client_disabled_facet_reenables_only_after_resume() {
    use yserver_core::{
        backend::Backend,
        core_loop::HostInputEvent,
        server::ServerState,
        xinput::{InputSourceId, XiFacetKind},
    };

    // Mutation killed: return before recording session_enabled=false for
    // an already client-disabled facet, so Device Enabled=1 reattaches it
    // during suspension.
    const SOURCE: InputSourceId = InputSourceId(0xD4_07);
    let mut state = ServerState::new();
    let original_core_properties =
        [2, 3, 4, 5].map(|id| state.xi_devices.device(id).unwrap().properties.clone());
    let mut backend = KmsBackend::for_tests();
    let mut peer = kbd_map_client_id(&mut state, 5);
    let info = dynamic_test_device(SOURCE, false, true);
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceAdded(info.clone()),
    );
    let pointer = state
        .xi_devices
        .facet(SOURCE, XiFacetKind::PointerTouch)
        .expect("physical pointer facet");
    select_device_enabled_transition_events(&mut state, &mut backend, pointer, 1);
    assert!(kbd_map_drain(&mut peer).is_empty());
    assert_eq!(state.xi_devices.source_ids(), [SOURCE]);
    let registry_ids_before = state
        .xi_devices
        .devices()
        .iter()
        .map(|device| device.id)
        .collect::<Vec<_>>();
    let properties_before = state
        .xi_devices
        .devices()
        .iter()
        .map(|device| (device.id, device.properties.clone()))
        .collect::<Vec<_>>();
    let selections_before = (
        state.clients[&5].xi2_masks.clone(),
        state.clients[&5].xi1_event_classes.clone(),
        state.clients[&5].xi1_window_event_classes.clone(),
        state.clients[&5].event_masks.clone(),
    );

    let disable = device_enabled_write_body_xi2(
        pointer,
        state.xi_device_enabled_atom.0,
        8,
        yserver_core::xinput::XA_INTEGER.0,
        &[0],
    );
    let disabled =
        device_enabled_request_bytes(&mut state, &mut backend, &mut peer, 57, 4, &disable);
    assert_eq!(
        device_enabled_event_kinds(&disabled),
        [
            "xi2-property",
            "xi1-property",
            "presence",
            "hierarchy",
            "xi2-property",
            "xi1-property"
        ]
    );
    assert!(state.xi_devices.device(pointer).unwrap().client_disabled);

    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceSuspended { source_id: SOURCE },
    );
    assert!(kbd_map_drain(&mut peer).is_empty());
    let suspended_device = state.xi_devices.device(pointer).unwrap();
    assert!(!suspended_device.session_enabled);
    assert!(suspended_device.client_disabled);
    assert!(!suspended_device.enabled);
    assert_eq!(suspended_device.attached_master, None);
    assert_eq!(
        suspended_device.properties[&state.xi_device_enabled_atom].data,
        [0]
    );
    assert!(state.xi2_detached_masters.is_empty());
    assert!(state.floating_pointer_positions.is_empty());

    let enable_while_suspended = device_enabled_write_body_xi2(
        pointer,
        state.xi_device_enabled_atom.0,
        8,
        yserver_core::xinput::XA_INTEGER.0,
        &[1],
    );
    let suspended_enable_events = device_enabled_request_bytes(
        &mut state,
        &mut backend,
        &mut peer,
        57,
        5,
        &enable_while_suspended,
    );
    assert_eq!(
        device_enabled_event_kinds(&suspended_enable_events),
        ["xi2-property", "xi1-property"],
        "a client preference write while away has no Enabled transition events",
    );
    let suspended_device = state.xi_devices.device(pointer).unwrap();
    assert!(!suspended_device.session_enabled);
    assert!(!suspended_device.client_disabled);
    assert!(!suspended_device.enabled);
    assert_eq!(suspended_device.attached_master, None);
    assert_eq!(
        suspended_device.properties[&state.xi_device_enabled_atom].data,
        [1],
        "the client's nonzero value is stored while the session remains away",
    );
    assert!(state.xi2_detached_masters.is_empty());
    assert!(state.floating_pointer_positions.is_empty());

    let mut resumed = info;
    resumed.enabled = true;
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceResumed(resumed),
    );
    assert_eq!(
        device_enabled_event_kinds(&kbd_map_drain(&mut peer)),
        ["xi2-property", "xi1-property", "presence", "hierarchy"],
        "resume publishes one normal enable sequence",
    );
    let enabled_device = state.xi_devices.device(pointer).unwrap();
    assert!(enabled_device.session_enabled);
    assert!(!enabled_device.client_disabled);
    assert!(enabled_device.enabled);
    assert_eq!(enabled_device.attached_master, Some(2));
    assert_eq!(
        enabled_device.properties[&state.xi_device_enabled_atom].data,
        [1]
    );
    assert_eq!(
        state
            .xi_devices
            .devices()
            .iter()
            .map(|device| device.id)
            .collect::<Vec<_>>(),
        registry_ids_before,
    );
    assert_eq!(state.xi_devices.source_ids(), [SOURCE]);
    assert_eq!(
        state
            .xi_devices
            .devices()
            .iter()
            .map(|device| (device.id, device.properties.clone()))
            .collect::<Vec<_>>(),
        properties_before
            .into_iter()
            .map(|(id, mut properties)| {
                if id == pointer {
                    properties.insert(
                        state.xi_device_enabled_atom,
                        enabled_device.properties[&state.xi_device_enabled_atom].clone(),
                    );
                }
                (id, properties)
            })
            .collect::<Vec<_>>(),
    );
    assert!(state.keys_down.iter().all(|byte| *byte == 0));
    assert_eq!(state.buttons_down, 0);
    assert!(state.key_down_by_device.is_empty());
    assert!(state.xi2_detached_masters.is_empty());
    assert!(state.floating_pointer_positions.is_empty());
    assert_eq!(
        (
            state.clients[&5].xi2_masks.clone(),
            state.clients[&5].xi1_event_classes.clone(),
            state.clients[&5].xi1_window_event_classes.clone(),
            state.clients[&5].event_masks.clone(),
        ),
        selections_before,
    );

    assert_device_enabled_write_cleanup(
        &mut state,
        &mut backend,
        SOURCE,
        &original_core_properties,
    );
}

#[test]
fn xi1_button_shape_set_mapping_is_busy_while_changed_button_held() {
    use yserver_core::{
        backend::Backend,
        core_loop::{HostInputEvent, InputOrigin},
        server::ServerState,
        xinput::{InputSourceId, XiFacetKind},
    };
    // Mutation killed: skip ApplyPointerMapping's changed-held-button check,
    // so SetDeviceButtonMapping incorrectly succeeds while Button1 is down.
    let mut state = ServerState::new();
    let mut backend = KmsBackend::for_tests();
    let mut peer = kbd_map_client_id(&mut state, 5);
    let source = InputSourceId(0xB071);
    let info = dynamic_test_device(source, false, true);
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceAdded(info.clone()),
    );
    let device = state
        .xi_devices
        .facet(source, XiFacetKind::PointerTouch)
        .expect("dynamic pointer facet");
    assert!(state.xi_devices.device(device).unwrap().enabled);
    let source_ids_before = state.xi_devices.source_ids();
    let device_ids_before: Vec<_> = state
        .xi_devices
        .devices()
        .iter()
        .map(|device| device.id)
        .collect();
    let properties_before: Vec<_> = state
        .xi_devices
        .devices()
        .iter()
        .map(|device| (device.id, device.properties.clone()))
        .collect();
    let detached_before = state.xi2_detached_masters.clone();
    let floating_before = state.floating_pointer_positions.clone();
    let selections_before = (
        state.clients[&5].xi2_masks.clone(),
        state.clients[&5].xi1_event_classes.clone(),
        state.clients[&5].xi1_window_event_classes.clone(),
        state.clients[&5].event_masks.clone(),
    );

    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::PointerButton {
            origin: InputOrigin::Physical(source),
            button: 0x110, // BTN_LEFT -> Button1
            pressed: true,
            time: 1,
        },
    );
    assert_eq!(state.xi_devices.device(device).unwrap().buttons_down & 1, 1);
    let held = state
        .xi1_device_input_state
        .get(&device)
        .expect("host ButtonPress updates XI1 held state");
    assert_eq!(
        held.buttons_down[0] & 0b10,
        0b10,
        "Button1 held through input path"
    );
    assert_eq!(state.xi_devices.device(device).unwrap().buttons_down & 1, 1);

    let mut map_body = vec![u8::try_from(device).unwrap(), 7, 0, 0];
    map_body.extend_from_slice(&[2, 1, 3, 4, 5, 6, 7]);
    device_enabled_xi_request(&mut state, &mut backend, 29, 2, &map_body);
    let response = kbd_map_drain(&mut peer);
    assert_eq!(response.len(), 32, "MappingBusy is only the reply");
    assert_eq!((response[0], response[8]), (1, 1), "MappingBusy");

    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::PointerButton {
            origin: InputOrigin::Physical(source),
            button: 0x110,
            pressed: false,
            time: 2,
        },
    );
    assert_eq!(state.xi_devices.device(device).unwrap().buttons_down, 0);
    assert_eq!(
        state
            .xi1_device_input_state
            .get(&device)
            .unwrap()
            .buttons_down[0],
        0
    );
    assert!(
        !state.xi1_button_map.contains_key(&device),
        "busy map is not committed"
    );
    assert_eq!(state.xi_devices.source_ids(), source_ids_before);
    assert_eq!(
        state
            .xi_devices
            .devices()
            .iter()
            .map(|device| device.id)
            .collect::<Vec<_>>(),
        device_ids_before,
        "no registry device was added or lost",
    );
    assert_eq!(
        state
            .xi_devices
            .devices()
            .iter()
            .map(|device| (device.id, device.properties.clone()))
            .collect::<Vec<_>>(),
        properties_before,
        "mapping refusal leaves property maps unchanged",
    );
    assert_eq!(state.buttons_down, 0);
    assert!(
        state
            .xi_devices
            .devices()
            .iter()
            .all(|device| device.buttons_down == 0)
    );
    assert!(
        state
            .xi1_device_input_state
            .values()
            .all(|input| input.buttons_down.iter().all(|byte| *byte == 0))
    );
    assert_eq!(state.xi2_detached_masters, detached_before);
    assert_eq!(state.floating_pointer_positions, floating_before);
    assert_eq!(
        (
            &state.clients[&5].xi2_masks,
            &state.clients[&5].xi1_event_classes,
            &state.clients[&5].xi1_window_event_classes,
            &state.clients[&5].event_masks,
        ),
        (
            &selections_before.0,
            &selections_before.1,
            &selections_before.2,
            &selections_before.3,
        ),
        "no client selection state changed",
    );
}

#[test]
fn xi_device_enabled_write_disables_only_requested_facet_and_drops_its_input() {
    use yserver_core::{
        backend::Backend,
        core_loop::{HostInputEvent, InputOrigin},
        host_x11::HostKeyEvent,
        server::ServerState,
        xinput::{InputSourceId, XiFacetKind},
    };

    // Kills: changing the source-wide bit, treating only byte value 1 as enabled, or omitting the outer property notification.
    const SOURCE: InputSourceId = InputSourceId(0xD4_01);
    const KEYCODE: u8 = 38;
    let mut state = ServerState::new();
    let original_core_properties =
        [2, 3, 4, 5].map(|id| state.xi_devices.device(id).unwrap().properties.clone());
    let mut backend = KmsBackend::for_tests();
    let mut peer = kbd_map_client_id(&mut state, 5);
    let info = dynamic_test_device(SOURCE, true, true);
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceAdded(info.clone()),
    );
    let pointer_id = state
        .xi_devices
        .facet(SOURCE, XiFacetKind::PointerTouch)
        .unwrap();
    let keyboard_id = state
        .xi_devices
        .facet(SOURCE, XiFacetKind::Keyboard)
        .unwrap();
    select_device_enabled_transition_events(&mut state, &mut backend, pointer_id, 1);
    let _ = kbd_map_drain(&mut peer);

    let disable = device_enabled_write_body_xi2(
        pointer_id,
        state.xi_device_enabled_atom.0,
        8,
        yserver_core::xinput::XA_INTEGER.0,
        &[0],
    );
    let disabled_events =
        device_enabled_request_bytes(&mut state, &mut backend, &mut peer, 57, 4, &disable);
    assert_eq!(
        device_enabled_event_kinds(&disabled_events),
        [
            "xi2-property",
            "xi1-property",
            "presence",
            "hierarchy",
            "xi2-property",
            "xi1-property"
        ]
    );
    assert!(
        !state.xi_devices.device(pointer_id).unwrap().enabled,
        "requested pointer facet is disabled"
    );
    assert_eq!(
        state.xi_devices.device(pointer_id).unwrap().attached_master,
        None,
        "disabled pointer facet floats"
    );
    assert!(state.xi_devices.device(keyboard_id).unwrap().enabled);
    assert_eq!(
        state
            .xi_devices
            .device(keyboard_id)
            .unwrap()
            .attached_master,
        Some(3),
        "sibling keyboard stays attached"
    );

    let old_cursor = (backend.core.cursor_x, backend.core.cursor_y);
    let old_buttons = state.buttons_down;
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::PointerMotion {
            origin: InputOrigin::Physical(SOURCE),
            x: 55,
            y: 61,
            time: 5,
            relative: false,
            dx: 0,
            dy: 0,
            motion_delta: None,
        },
    );
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::PointerButton {
            origin: InputOrigin::Physical(SOURCE),
            button: 0x110,
            pressed: true,
            time: 6,
        },
    );
    assert_eq!((backend.core.cursor_x, backend.core.cursor_y), old_cursor);
    assert_eq!(state.buttons_down, old_buttons);
    assert_eq!(state.xi_devices.device(pointer_id).unwrap().buttons_down, 0);

    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::Key(HostKeyEvent {
            origin: InputOrigin::Physical(SOURCE),
            pressed: true,
            keycode: KEYCODE,
            time: 7,
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            state: 0,
        }),
    );
    let keyboard_events = xi2_events(&kbd_map_drain(&mut peer));
    assert!(keyboard_events.iter().any(|event| {
        event.0 == 2
            && event.1 == keyboard_id
            && event.2 == keyboard_id
            && event.3 == u32::from(KEYCODE)
    }));
    assert!(state.xi_devices.device(keyboard_id).unwrap().enabled);
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::Key(HostKeyEvent {
            origin: InputOrigin::Physical(SOURCE),
            pressed: false,
            keycode: KEYCODE,
            time: 8,
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            state: 0,
        }),
    );
    let _ = kbd_map_drain(&mut peer);

    let enable_nonzero = device_enabled_write_body_xi2(
        pointer_id,
        state.xi_device_enabled_atom.0,
        8,
        yserver_core::xinput::XA_INTEGER.0,
        &[2],
    );
    let enabled_events =
        device_enabled_request_bytes(&mut state, &mut backend, &mut peer, 57, 9, &enable_nonzero);
    assert_eq!(
        device_enabled_event_kinds(&enabled_events),
        [
            "xi2-property",
            "xi1-property",
            "presence",
            "hierarchy",
            "xi2-property",
            "xi1-property"
        ]
    );
    assert!(
        state.xi_devices.device(pointer_id).unwrap().enabled,
        "any nonzero byte re-enables the physical facet"
    );
    assert_eq!(
        state.xi_devices.device(pointer_id).unwrap().attached_master,
        Some(2)
    );

    assert_device_enabled_write_cleanup(
        &mut state,
        &mut backend,
        SOURCE,
        &original_core_properties,
    );
}

#[test]
fn xi_device_enabled_write_drains_held_pointer_button_before_disable() {
    use yserver_core::{
        backend::Backend,
        core_loop::{HostInputEvent, InputOrigin},
        server::ServerState,
        xinput::{InputSourceId, XiFacetKind},
    };

    // Kills: omitting the per-facet release_buttons path before the disabled fact is committed.
    const SOURCE: InputSourceId = InputSourceId(0xD4_02);
    let mut state = ServerState::new();
    let original_core_properties =
        [2, 3, 4, 5].map(|id| state.xi_devices.device(id).unwrap().properties.clone());
    let mut backend = KmsBackend::for_tests();
    let mut peer = kbd_map_client_id(&mut state, 5);
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceAdded(dynamic_test_device(SOURCE, false, true)),
    );
    let pointer_id = state
        .xi_devices
        .facet(SOURCE, XiFacetKind::PointerTouch)
        .unwrap();
    select_device_enabled_transition_events(&mut state, &mut backend, pointer_id, 1);
    let _ = kbd_map_drain(&mut peer);
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::PointerButton {
            origin: InputOrigin::Physical(SOURCE),
            button: 0x110,
            pressed: true,
            time: 2,
        },
    );
    let presses = xi2_events(&kbd_map_drain(&mut peer));
    assert!(presses.iter().any(|event| {
        event.0 == 4 && event.1 == pointer_id && event.2 == pointer_id && event.3 == 1
    }));
    assert_eq!(state.xi_devices.device(pointer_id).unwrap().buttons_down, 1);
    assert_ne!(state.buttons_down & 1, 0);

    let disable = device_enabled_write_body_xi1(
        pointer_id,
        8,
        yserver_core::xinput::XI_PROP_MODE_REPLACE,
        state.xi_device_enabled_atom.0,
        yserver_core::xinput::XA_INTEGER.0,
        &[0],
    );
    let releases =
        device_enabled_request_bytes(&mut state, &mut backend, &mut peer, 37, 4, &disable);
    let events = xi2_events(&releases);
    assert!(events.iter().any(|event| {
        event.0 == 5 && event.1 == pointer_id && event.2 == pointer_id && event.3 == 1
    }));
    assert!(
        events
            .iter()
            .any(|event| { event.0 == 5 && event.1 == 2 && event.2 == pointer_id && event.3 == 1 })
    );
    assert_eq!(state.xi_devices.device(pointer_id).unwrap().buttons_down, 0);
    assert_eq!(state.buttons_down & 1, 0);
    assert!(!state.xi_devices.device(pointer_id).unwrap().enabled);
    assert!(state.xi2_detached_masters.is_empty());
    assert!(state.floating_pointer_positions.is_empty());

    assert_device_enabled_write_cleanup(
        &mut state,
        &mut backend,
        SOURCE,
        &original_core_properties,
    );
}

#[test]
fn xi_device_enabled_write_validates_virtual_devices_format_and_delete() {
    use yserver_core::{
        backend::Backend,
        core_loop::HostInputEvent,
        server::ServerState,
        xinput::{InputSourceId, XiFacetKind},
    };

    // Kills: generic handling of Device Enabled writes, and forcing a recreated property to ignore Xorg's deletable default.
    const SOURCE: InputSourceId = InputSourceId(0xD4_03);
    let mut state = ServerState::new();
    let original_core_properties =
        [2, 3, 4, 5].map(|id| state.xi_devices.device(id).unwrap().properties.clone());
    let mut backend = KmsBackend::for_tests();
    let mut peer = kbd_map_client_id(&mut state, 5);
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceAdded(dynamic_test_device(SOURCE, false, true)),
    );
    let pointer_id = state
        .xi_devices
        .facet(SOURCE, XiFacetKind::PointerTouch)
        .unwrap();
    let enabled_atom = state.xi_device_enabled_atom.0;
    let before =
        [2, 3, 4, 5, pointer_id].map(|id| state.xi_devices.device(id).unwrap().properties.clone());

    for device_id in [2, 3, 4, 5] {
        let body = device_enabled_write_body_xi2(
            device_id,
            enabled_atom,
            8,
            yserver_core::xinput::XA_INTEGER.0,
            &[0],
        );
        let wire = device_enabled_request_bytes(
            &mut state,
            &mut backend,
            &mut peer,
            57,
            10 + device_id,
            &body,
        );
        assert_eq!(wire.len(), 32);
        assert_eq!(
            wire[1], 10,
            "zero is BadAccess for virtual device {device_id}"
        );
    }
    for device_id in [2u16, 3, 4, 5] {
        let body = device_enabled_write_body_xi1(
            device_id,
            8,
            yserver_core::xinput::XI_PROP_MODE_REPLACE,
            enabled_atom,
            yserver_core::xinput::XA_INTEGER.0,
            &[1],
        );
        let wire = device_enabled_request_bytes(
            &mut state,
            &mut backend,
            &mut peer,
            37,
            20 + device_id,
            &body,
        );
        assert!(
            wire.is_empty(),
            "one succeeds on virtual device {device_id}"
        );
    }

    let malformed = device_enabled_write_body_xi2(
        pointer_id,
        enabled_atom,
        16,
        yserver_core::xinput::XA_INTEGER.0,
        &[1, 0],
    );
    let wire =
        device_enabled_request_bytes(&mut state, &mut backend, &mut peer, 57, 30, &malformed);
    assert_eq!(wire.len(), 32);
    assert_eq!(wire[1], 2, "format 16 is BadValue");

    let wrong_type_atom = state.atoms.intern("STRING", false);
    let wrong_type = device_enabled_write_body_xi1(
        pointer_id,
        8,
        yserver_core::xinput::XI_PROP_MODE_REPLACE,
        enabled_atom,
        wrong_type_atom.0,
        &[1],
    );
    let wire =
        device_enabled_request_bytes(&mut state, &mut backend, &mut peer, 37, 33, &wrong_type);
    assert_eq!(wire.len(), 32);
    assert_eq!(wire[1], 2, "non-INTEGER type is BadValue");

    let wrong_size = device_enabled_write_body_xi2(
        pointer_id,
        enabled_atom,
        8,
        yserver_core::xinput::XA_INTEGER.0,
        &[1, 0],
    );
    let wire =
        device_enabled_request_bytes(&mut state, &mut backend, &mut peer, 57, 34, &wrong_size);
    assert_eq!(wire.len(), 32);
    assert_eq!(wire[1], 2, "two items are BadValue");

    for (minor, body, sequence) in [
        (
            58,
            {
                let mut body = Vec::with_capacity(8);
                body.extend_from_slice(&pointer_id.to_le_bytes());
                body.extend_from_slice(&0u16.to_le_bytes());
                body.extend_from_slice(&enabled_atom.to_le_bytes());
                body
            },
            31,
        ),
        (
            38,
            {
                let mut body = Vec::with_capacity(8);
                body.extend_from_slice(&enabled_atom.to_le_bytes());
                body.push(u8::try_from(pointer_id).unwrap());
                body.extend_from_slice(&[0; 3]);
                body
            },
            32,
        ),
    ] {
        let wire = device_enabled_request_bytes(
            &mut state,
            &mut backend,
            &mut peer,
            minor,
            sequence,
            &body,
        );
        assert_eq!(wire.len(), 32);
        assert_eq!(wire[1], 10, "delete via XI minor {minor} is BadAccess");
    }
    for device_id in [2u16, 3, 4, 5] {
        let mut body = Vec::with_capacity(8);
        body.extend_from_slice(&device_id.to_le_bytes());
        body.extend_from_slice(&0u16.to_le_bytes());
        body.extend_from_slice(&enabled_atom.to_le_bytes());
        let wire = device_enabled_request_bytes(
            &mut state,
            &mut backend,
            &mut peer,
            58,
            50 + device_id,
            &body,
        );
        assert_eq!(wire.len(), 32);
        assert_eq!(
            wire[1], 10,
            "Device Enabled on {device_id} is not deletable"
        );
    }

    for (index, id) in [2, 3, 4, 5, pointer_id].into_iter().enumerate() {
        assert_eq!(
            state.xi_devices.device(id).unwrap().properties,
            before[index],
            "invalid request leaves device {id}'s property map unchanged"
        );
    }

    let mut get_delete = Vec::with_capacity(20);
    get_delete.extend_from_slice(&pointer_id.to_le_bytes());
    get_delete.extend_from_slice(&[1, 0]); // delete after complete read
    get_delete.extend_from_slice(&enabled_atom.to_le_bytes());
    get_delete.extend_from_slice(&0u32.to_le_bytes()); // AnyPropertyType
    get_delete.extend_from_slice(&0u32.to_le_bytes()); // offset
    get_delete.extend_from_slice(&100u32.to_le_bytes()); // length
    let reply =
        device_enabled_request_bytes(&mut state, &mut backend, &mut peer, 59, 40, &get_delete);
    assert_eq!(
        reply.len(),
        36,
        "GetProperty(delete) still replies with its value"
    );
    assert!(
        !state
            .xi_devices
            .device(pointer_id)
            .unwrap()
            .properties
            .contains_key(&state.xi_device_enabled_atom),
        "GetProperty(delete) unlinks Device Enabled"
    );
    let recreate = device_enabled_write_body_xi1(
        pointer_id,
        8,
        yserver_core::xinput::XI_PROP_MODE_REPLACE,
        enabled_atom,
        yserver_core::xinput::XA_INTEGER.0,
        &[1],
    );
    let no_reply =
        device_enabled_request_bytes(&mut state, &mut backend, &mut peer, 37, 41, &recreate);
    assert!(no_reply.is_empty());
    let recreated = state
        .xi_devices
        .device(pointer_id)
        .unwrap()
        .properties
        .get(&state.xi_device_enabled_atom)
        .unwrap();
    assert_eq!(recreated.data, [1]);
    assert!(recreated.deletable);
    // Xorg's XICreateDeviceProperty defaults new properties to deletable
    // (xiproperty.c:575-592). This exposes the narrow addendum conflict:
    // DeleteProperty is BadAccess on the seeded property, but succeeds
    // after GetProperty(delete) removes it and a client recreates it.
    let mut delete_recreated = Vec::with_capacity(8);
    delete_recreated.extend_from_slice(&pointer_id.to_le_bytes());
    delete_recreated.extend_from_slice(&0u16.to_le_bytes());
    delete_recreated.extend_from_slice(&enabled_atom.to_le_bytes());
    let wire = device_enabled_request_bytes(
        &mut state,
        &mut backend,
        &mut peer,
        58,
        42,
        &delete_recreated,
    );
    assert!(
        wire.is_empty(),
        "recreated property follows Xorg deletability"
    );
    assert!(
        !state
            .xi_devices
            .device(pointer_id)
            .unwrap()
            .properties
            .contains_key(&state.xi_device_enabled_atom)
    );
    assert!(state.xi_devices.device(pointer_id).unwrap().enabled);
    assert_eq!(
        state.xi_devices.device(pointer_id).unwrap().attached_master,
        Some(2)
    );
    assert!(state.xi2_detached_masters.is_empty());
    assert!(state.floating_pointer_positions.is_empty());
    assert_device_enabled_write_cleanup(
        &mut state,
        &mut backend,
        SOURCE,
        &original_core_properties,
    );
}

#[test]
fn xi_device_enabled_write_while_session_disabled_stays_floating_until_enabled() {
    use yserver_core::{
        backend::Backend,
        core_loop::HostInputEvent,
        server::ServerState,
        xinput::{InputSourceId, XiFacetKind},
    };

    // Kills: recording a client-disabled preference for zero written while only session-disabled.
    const SOURCE: InputSourceId = InputSourceId(0xD4_04);
    let mut state = ServerState::new();
    let original_core_properties =
        [2, 3, 4, 5].map(|id| state.xi_devices.device(id).unwrap().properties.clone());
    let mut backend = KmsBackend::for_tests();
    let mut peer = kbd_map_client_id(&mut state, 5);
    let mut info = dynamic_test_device(SOURCE, false, true);
    info.enabled = false;
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceAdded(info.clone()),
    );
    let pointer_id = state
        .xi_devices
        .facet(SOURCE, XiFacetKind::PointerTouch)
        .unwrap();
    select_device_enabled_transition_events(&mut state, &mut backend, pointer_id, 1);
    let _ = kbd_map_drain(&mut peer);

    let disable = device_enabled_write_body_xi2(
        pointer_id,
        state.xi_device_enabled_atom.0,
        8,
        yserver_core::xinput::XA_INTEGER.0,
        &[0],
    );
    let write_events =
        device_enabled_request_bytes(&mut state, &mut backend, &mut peer, 57, 4, &disable);
    assert_eq!(
        device_enabled_event_kinds(&write_events),
        ["xi2-property", "xi1-property"],
        "a no-transition write sends only the outer property notification"
    );
    assert!(!state.xi_devices.device(pointer_id).unwrap().client_disabled);
    assert!(!state.xi_devices.device(pointer_id).unwrap().enabled);
    assert_eq!(
        state.xi_devices.device(pointer_id).unwrap().attached_master,
        None
    );
    assert_eq!(
        state.xi_devices.device(pointer_id).unwrap().properties[&state.xi_device_enabled_atom].data,
        [0]
    );

    let mut resumed = info;
    resumed.enabled = true;
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceResumed(resumed),
    );
    assert!(
        state.xi_devices.device(pointer_id).unwrap().enabled,
        "session resume enables a facet not client-disabled"
    );
    assert_eq!(
        state.xi_devices.device(pointer_id).unwrap().attached_master,
        Some(2)
    );
    assert!(!state.xi_devices.device(pointer_id).unwrap().client_disabled);
    assert!(state.xi2_detached_masters.is_empty());
    assert!(state.floating_pointer_positions.is_empty());
    assert_eq!(
        device_enabled_event_kinds(&kbd_map_drain(&mut peer)),
        ["xi2-property", "xi1-property", "presence", "hierarchy"],
        "resume emits its normal enable sequence"
    );
    assert_eq!(
        state.xi_devices.device(pointer_id).unwrap().properties[&state.xi_device_enabled_atom].data,
        [1]
    );

    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceSuspended { source_id: SOURCE },
    );
    let _ = kbd_map_drain(&mut peer);
    assert_device_enabled_write_cleanup(
        &mut state,
        &mut backend,
        SOURCE,
        &original_core_properties,
    );
}

#[test]
fn xi_device_enabled_write_preserves_transition_order_outer_notification_and_value() {
    use yserver_core::{
        backend::Backend,
        core_loop::HostInputEvent,
        server::ServerState,
        xinput::{InputSourceId, XiFacetKind},
    };

    // Kills: normalizing nonzero bytes, omitting the outer notification, or transitioning on zero while disabled or one while enabled.
    const SOURCE: InputSourceId = InputSourceId(0xD4_05);
    let mut state = ServerState::new();
    let original_core_properties =
        [2, 3, 4, 5].map(|id| state.xi_devices.device(id).unwrap().properties.clone());
    let mut backend = KmsBackend::for_tests();
    let mut peer = kbd_map_client_id(&mut state, 5);
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceAdded(dynamic_test_device(SOURCE, false, true)),
    );
    let pointer_id = state
        .xi_devices
        .facet(SOURCE, XiFacetKind::PointerTouch)
        .unwrap();
    select_device_enabled_transition_events(&mut state, &mut backend, pointer_id, 1);
    let _ = kbd_map_drain(&mut peer);

    let disable = device_enabled_write_body_xi1(
        pointer_id,
        8,
        yserver_core::xinput::XI_PROP_MODE_REPLACE,
        state.xi_device_enabled_atom.0,
        yserver_core::xinput::XA_INTEGER.0,
        &[0],
    );
    let disabled =
        device_enabled_request_bytes(&mut state, &mut backend, &mut peer, 37, 4, &disable);
    assert_eq!(
        device_enabled_event_kinds(&disabled),
        [
            "xi2-property",
            "xi1-property",
            "presence",
            "hierarchy",
            "xi2-property",
            "xi1-property"
        ],
        "A3 property/presence/hierarchy sequence precedes the outer property event"
    );
    assert!(!state.xi_devices.device(pointer_id).unwrap().enabled);
    assert_eq!(
        state.xi_devices.device(pointer_id).unwrap().attached_master,
        None
    );
    assert_eq!(
        state.xi_devices.device(pointer_id).unwrap().properties[&state.xi_device_enabled_atom].data,
        [0]
    );
    assert!(state.xi2_detached_masters.is_empty());
    assert!(state.floating_pointer_positions.is_empty());

    let disabled_again = device_enabled_write_body_xi2(
        pointer_id,
        state.xi_device_enabled_atom.0,
        8,
        yserver_core::xinput::XA_INTEGER.0,
        &[0],
    );
    let unchanged_disabled =
        device_enabled_request_bytes(&mut state, &mut backend, &mut peer, 57, 5, &disabled_again);
    assert_eq!(
        device_enabled_event_kinds(&unchanged_disabled),
        ["xi2-property", "xi1-property"],
        "zero on an already disabled facet emits one property notification and no transition"
    );
    assert!(state.xi_devices.device(pointer_id).unwrap().client_disabled);
    assert!(!state.xi_devices.device(pointer_id).unwrap().enabled);
    assert_eq!(
        state.xi_devices.device(pointer_id).unwrap().attached_master,
        None
    );

    let enable_five = device_enabled_write_body_xi2(
        pointer_id,
        state.xi_device_enabled_atom.0,
        8,
        yserver_core::xinput::XA_INTEGER.0,
        &[5],
    );
    let enabled =
        device_enabled_request_bytes(&mut state, &mut backend, &mut peer, 57, 6, &enable_five);
    assert_eq!(
        device_enabled_event_kinds(&enabled),
        [
            "xi2-property",
            "xi1-property",
            "presence",
            "hierarchy",
            "xi2-property",
            "xi1-property"
        ],
        "A4 property/presence/hierarchy sequence precedes the outer property event"
    );
    assert!(state.xi_devices.device(pointer_id).unwrap().enabled);
    assert_eq!(
        state.xi_devices.device(pointer_id).unwrap().attached_master,
        Some(2)
    );
    assert_eq!(
        state.xi_devices.device(pointer_id).unwrap().properties[&state.xi_device_enabled_atom].data,
        [5]
    );
    let enabled_atom = state.xi_device_enabled_atom.0;
    assert_eq!(
        device_enabled_get_enabled_xi2(
            &mut state,
            &mut backend,
            &mut peer,
            pointer_id,
            enabled_atom,
            7,
        ),
        5,
        "XIGetProperty returns the byte that the client wrote"
    );
    assert!(state.xi2_detached_masters.is_empty());
    assert!(state.floating_pointer_positions.is_empty());

    let enable_one = device_enabled_write_body_xi1(
        pointer_id,
        8,
        yserver_core::xinput::XI_PROP_MODE_REPLACE,
        enabled_atom,
        yserver_core::xinput::XA_INTEGER.0,
        &[1],
    );
    let unchanged_enabled =
        device_enabled_request_bytes(&mut state, &mut backend, &mut peer, 37, 8, &enable_one);
    assert_eq!(
        device_enabled_event_kinds(&unchanged_enabled),
        ["xi2-property", "xi1-property"],
        "one on an enabled facet emits one property notification and no transition"
    );
    assert!(state.xi_devices.device(pointer_id).unwrap().enabled);
    assert_eq!(
        state.xi_devices.device(pointer_id).unwrap().attached_master,
        Some(2)
    );
    assert_eq!(
        state.xi_devices.device(pointer_id).unwrap().properties[&state.xi_device_enabled_atom].data,
        [1]
    );

    assert_device_enabled_write_cleanup(
        &mut state,
        &mut backend,
        SOURCE,
        &original_core_properties,
    );
}
