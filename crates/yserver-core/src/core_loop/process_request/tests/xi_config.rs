use super::*;

// -----------------------------------------------------------------
// XI 1.x device-property dispatch (minors 36/37/38/39) — byte-level.
// These are the path MATE's settings daemon actually uses
// (OpenDevice + GetDeviceProperty), distinct from the XI2 path above.
// -----------------------------------------------------------------

/// Build an `xGetDevicePropertyReq` body (bytes after the 4-byte
/// generic header): property(4), type(4), longOffset(4), longLength(4),
/// deviceid(1, CARD8), delete(1), pad(2).
fn xi1_get_property_body(
    property: u32,
    type_atom: u32,
    offset: u32,
    len: u32,
    deviceid: u8,
    delete: u8,
) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&property.to_le_bytes());
    body.extend_from_slice(&type_atom.to_le_bytes());
    body.extend_from_slice(&offset.to_le_bytes());
    body.extend_from_slice(&len.to_le_bytes());
    body.push(deviceid);
    body.push(delete);
    body.extend_from_slice(&0u16.to_le_bytes());
    body
}

#[test]
fn xi1_get_xtest_marker_property_wire() {
    // Reads the XTEST Device INTEGER/8/[1] marker from virtual XTEST pointer 4.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let marker_atom = state
        .atoms
        .intern(crate::xinput::PROP_XTEST_DEVICE, false)
        .0;
    let integer_atom = crate::xinput::XA_INTEGER.0;

    let body = xi1_get_property_body(marker_atom, 0 /* AnyPropertyType */, 0, 100, 4, 0);
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(0x1234),
        xi2_header(39),
        &body,
    )
    .unwrap();
    let wire = read_all_available(&mut peer);
    // 32-byte header + 1 data byte padded to 4 → 36.
    assert_eq!(wire.len(), 36);
    assert_eq!(wire[0], 1, "X_Reply");
    assert_eq!(u16::from_le_bytes([wire[2], wire[3]]), 0x1234, "sequence");
    assert_eq!(
        u32::from_le_bytes(wire[4..8].try_into().unwrap()),
        1,
        "length = ceil(1/4)"
    );
    assert_eq!(
        u32::from_le_bytes(wire[8..12].try_into().unwrap()),
        integer_atom,
        "type = INTEGER"
    );
    assert_eq!(
        u32::from_le_bytes(wire[12..16].try_into().unwrap()),
        0,
        "bytes_after = 0"
    );
    assert_eq!(
        u32::from_le_bytes(wire[16..20].try_into().unwrap()),
        1,
        "num_items = 1"
    );
    assert_eq!(wire[20], 8, "format = 8");
    assert_eq!(wire[21], 4, "deviceid echoed (XI1-specific byte)");
    assert!(wire[22..32].iter().all(|&b| b == 0), "pad1..pad3 zero");
    assert_eq!(wire[32], 1, "value = XTEST marker");
    assert_eq!(&wire[33..36], &[0, 0, 0], "value padding");
}

#[test]
fn xi1_get_device_property_absent_returns_none() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    // XTEST pointer 4 exists but has no property at atom 999. The atom
    // ITSELF must be valid (T3 BadAtom guard); the test checks the
    // distinct case where the atom is interned but the device has
    // no value for it.
    state.atoms.register_for_test(AtomId(999), "test-prop-999");
    let body = xi1_get_property_body(999, 0, 0, 100, 4, 0);
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(3),
        xi2_header(39),
        &body,
    )
    .unwrap();
    let wire = read_all_available(&mut peer);
    assert_eq!(wire.len(), 32, "no data for absent property");
    assert_eq!(wire[0], 1, "X_Reply (not error)");
    assert_eq!(
        u32::from_le_bytes(wire[8..12].try_into().unwrap()),
        0,
        "type = None"
    );
    assert_eq!(
        u32::from_le_bytes(wire[12..16].try_into().unwrap()),
        0,
        "bytes_after = 0"
    );
    assert_eq!(wire[20], 0, "format = 0");
    assert_eq!(wire[21], 4, "deviceid still echoed");
}

#[test]
fn xi1_get_device_property_type_mismatch_metadata_only() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    // Seed an INTEGER/8 prop at atom 100, then ask for STRING.
    seed_one_prop(&mut state, 100, 8, vec![0xAB]);
    let string_atom = crate::xinput::XA_STRING.0;
    let body = xi1_get_property_body(100, string_atom, 0, 100, 4, 0);
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(4),
        xi2_header(39),
        &body,
    )
    .unwrap();
    let wire = read_all_available(&mut peer);
    assert_eq!(wire.len(), 32, "no value bytes on mismatch");
    assert_eq!(
        u32::from_le_bytes(wire[8..12].try_into().unwrap()),
        crate::xinput::XA_INTEGER.0,
        "type = stored INTEGER"
    );
    assert_eq!(
        u32::from_le_bytes(wire[12..16].try_into().unwrap()),
        1,
        "bytes_after = stored item count"
    );
    assert_eq!(
        u32::from_le_bytes(wire[16..20].try_into().unwrap()),
        0,
        "num_items = 0"
    );
    assert_eq!(wire[20], 8, "format = stored");
    assert_eq!(wire[21], 4, "deviceid echoed");
}

#[test]
fn xi1_get_device_property_bad_device() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    // Pre-register atom 100 so the BadAtom guard doesn't fire before
    // the bad-device check the test is exercising.
    state.atoms.register_for_test(AtomId(100), "test-prop-100");
    let body = xi1_get_property_body(100, 0, 0, 100, 99, 0); // device 99 absent
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(9),
        xi2_header(39),
        &body,
    )
    .unwrap();
    let wire = read_all_available(&mut peer);
    assert_eq!(wire.len(), 32, "error is 32 bytes");
    assert_eq!(wire[0], 0, "error packet");
    assert_eq!(
        wire[1], XI2_FIRST_ERROR,
        "BadDevice = XI2_FIRST_ERROR (157)"
    );
    assert_eq!(u16::from_le_bytes([wire[2], wire[3]]), 9, "sequence");
}

#[test]
fn xi1_get_device_property_bad_delete_value() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    state.atoms.register_for_test(AtomId(100), "test-prop-100");
    let body = xi1_get_property_body(100, 0, 0, 1, 4, 2); // delete = 2 (illegal)
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(11),
        xi2_header(39),
        &body,
    )
    .unwrap();
    let wire = read_all_available(&mut peer);
    assert_eq!(wire.len(), 32);
    assert_eq!(wire[0], 0, "error packet");
    assert_eq!(wire[1], 2, "BadValue");
}

#[test]
fn xi1_list_device_properties_wire() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    seed_one_prop(&mut state, 100, 8, vec![1]);
    seed_one_prop(&mut state, 200, 8, vec![2]);

    // xListDevicePropertiesReq body: deviceid(1), pad0(1), pad1(2).
    let body = [4u8, 0, 0, 0];
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        xi2_header(36),
        &body,
    )
    .unwrap();
    let wire = read_all_available(&mut peer);
    assert_eq!(
        wire.len(),
        32 + 16,
        "32 header + XTEST marker + Device Enabled + 2 client atoms"
    );
    assert_eq!(wire[0], 1, "X_Reply");
    assert_eq!(
        u32::from_le_bytes(wire[4..8].try_into().unwrap()),
        4,
        "length = nAtoms including the XTEST marker and Device Enabled"
    );
    assert_eq!(u16::from_le_bytes([wire[8], wire[9]]), 4, "nAtoms");
    assert!(wire[10..32].iter().all(|&b| b == 0), "pad zero");
    let mut atoms = wire[32..]
        .chunks_exact(4)
        .map(|chunk| u32::from_le_bytes(chunk.try_into().unwrap()))
        .collect::<Vec<_>>();
    let mut expected = vec![
        state.xtest_device_atom.0,
        state.xi_device_enabled_atom.0,
        100,
        200,
    ];
    atoms.sort_unstable();
    expected.sort_unstable();
    assert_eq!(atoms, expected, "the virtual XTEST Device marker is listed");
}

#[test]
fn xi1_list_device_properties_bad_device() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let body = [99u8, 0, 0, 0]; // device 99 absent
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(2),
        xi2_header(36),
        &body,
    )
    .unwrap();
    let wire = read_all_available(&mut peer);
    assert_eq!(wire.len(), 32);
    assert_eq!(wire[0], 0, "error packet");
    assert_eq!(wire[1], XI2_FIRST_ERROR, "BadDevice");
}

#[test]
fn xi1_change_then_get_roundtrip_wire() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    // Pre-register atom 100 (T3 BadAtom guard).
    state.atoms.register_for_test(AtomId(100), "test-prop-100");

    // xChangeDevicePropertyReq body: property(4)=100, type(4)=INTEGER,
    // deviceid(1)=4, format(1)=8, mode(1)=Replace, pad(1),
    // nUnits(4)=3, value=[7,8,9]+pad.
    let mut body = Vec::new();
    body.extend_from_slice(&100u32.to_le_bytes());
    body.extend_from_slice(&crate::xinput::XA_INTEGER.0.to_le_bytes());
    body.push(4); // deviceid (CARD8)
    body.push(8); // format
    body.push(crate::xinput::XI_PROP_MODE_REPLACE); // mode
    body.push(0); // pad
    body.extend_from_slice(&3u32.to_le_bytes());
    body.extend_from_slice(&[7, 8, 9, 0]); // 3 data bytes + 1 pad
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        xi2_header(37),
        &body,
    )
    .unwrap();
    // ChangeDeviceProperty has no reply.
    assert!(read_all_available(&mut peer).is_empty());

    // Read it back via XI1 GetDeviceProperty.
    let gbody = xi1_get_property_body(100, 0, 0, 100, 4, 0);
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(2),
        xi2_header(39),
        &gbody,
    )
    .unwrap();
    let wire = read_all_available(&mut peer);
    assert_eq!(
        u32::from_le_bytes(wire[16..20].try_into().unwrap()),
        3,
        "num_items"
    );
    assert_eq!(wire[21], 4, "deviceid echoed");
    assert_eq!(&wire[32..35], &[7, 8, 9], "value round-trips");
}

#[test]
fn xi1_change_device_property_bad_device_outranks_format() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let mut body = Vec::new();
    body.extend_from_slice(&100u32.to_le_bytes());
    body.extend_from_slice(&crate::xinput::XA_INTEGER.0.to_le_bytes());
    body.push(99); // deviceid (CARD8) absent
    body.push(7); // format 7 (illegal)
    body.push(0); // mode
    body.push(0); // pad
    body.extend_from_slice(&0u32.to_le_bytes());
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(5),
        xi2_header(37),
        &body,
    )
    .unwrap();
    let wire = read_all_available(&mut peer);
    assert_eq!(wire.len(), 32);
    assert_eq!(wire[0], 0, "error packet");
    assert_eq!(wire[1], XI2_FIRST_ERROR, "BadDevice outranks BadValue");
}

#[test]
fn xi1_delete_device_property_wire() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    seed_one_prop(&mut state, 100, 8, vec![1]);

    // xDeleteDevicePropertyReq body: property(4)=100, deviceid(1)=4,
    // pad0(1), pad1(2).
    let mut body = Vec::new();
    body.extend_from_slice(&100u32.to_le_bytes());
    body.push(4); // deviceid (CARD8)
    body.push(0);
    body.extend_from_slice(&0u16.to_le_bytes());
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        xi2_header(38),
        &body,
    )
    .unwrap();
    assert!(read_all_available(&mut peer).is_empty(), "no reply");
    let dev = state
        .xi_devices
        .device(crate::xinput::DEVICEID_XTEST_POINTER)
        .unwrap();
    assert!(
        !dev.properties.contains_key(&AtomId(100)),
        "property removed"
    );
}

fn parse_t3_xi2_config_request(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    client: u32,
    sequence: u16,
    deviceid: u16,
    property_name: &str,
    mode: u8,
    format: u8,
    data: &[u8],
) -> crate::core_loop::message::XiConfigRequest {
    let property = state.atoms.intern(property_name, false).0;
    let type_atom = if property_name == "libinput Accel Speed" {
        state.float_atom.0
    } else {
        crate::xinput::XA_INTEGER.0
    };
    let body = xi2_change_property_body(deviceid, mode, format, property, type_atom, data);
    match handle_xi2_request(
        state,
        backend,
        None,
        ClientId(client),
        SequenceNumber(sequence),
        xi2_header(57),
        &body,
    )
    .expect("parse XI2 property request")
    {
        RequestOutcome::PendingXiConfig(request) => request,
        outcome => panic!("recognized physical write must be pending, got {outcome:?}"),
    }
}

fn parse_t3_xi1_config_request(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    client: u32,
    sequence: u16,
    deviceid: u16,
    data: &[u8],
) -> crate::core_loop::message::XiConfigRequest {
    let property = state.atoms.intern("libinput Accel Speed", false).0;
    let body = xi1_change_property_body(
        property,
        state.float_atom.0,
        u8::try_from(deviceid).expect("physical XI1 id fits CARD8"),
        32,
        crate::xinput::XI_PROP_MODE_REPLACE,
        data,
    );
    match handle_xi2_request(
        state,
        backend,
        None,
        ClientId(client),
        SequenceNumber(sequence),
        xi2_header(37),
        &body,
    )
    .expect("parse XI1 property request")
    {
        RequestOutcome::PendingXiConfig(request) => request,
        outcome => panic!("recognized physical XI1 write must be pending, got {outcome:?}"),
    }
}

/// Run the real HostInput lifecycle dispatcher while one write occupies
/// the lane and another targets the source being removed or suspended.
/// RecordingBackend does not own XI topology, so prepare the state change
/// performed by the production KMS `on_host_input` before dispatching.
fn assert_t3_host_lifecycle_cancels_queued_write(removed: bool, xi1_request: bool) {
    use crate::xinput::libinput_props::{DeviceConfigChange, DeviceConfigStart, DeviceConfigToken};

    let mut state = ServerState::new();
    let mut peer_a = install_capture_client(&mut state, 1);
    let mut peer_b = install_capture_client(&mut state, 2);
    let mut backend = RecordingBackend::new();
    seed_pointer_for_t3(&mut state);
    let source = state
        .xi_devices
        .device(TEST_PHYSICAL_POINTER_ID)
        .unwrap()
        .source_id
        .unwrap();
    let source_info = state.xi_devices.source(source).unwrap().clone();
    let accel_atom = state.atoms.id_for("libinput Accel Speed").unwrap();
    let source_property_before = state
        .xi_devices
        .device(TEST_PHYSICAL_POINTER_ID)
        .unwrap()
        .properties[&accel_atom]
        .data
        .clone();

    let mut live_info = source_info.clone();
    live_info.source_id = crate::xinput::InputSourceId(source.0 + 1);
    live_info.device_node = "/dev/input/event78".into();
    live_info.sysname = "event78".into();
    let live_ids = state.xi_register_source(&live_info);
    let live_device = *live_ids
        .iter()
        .find(|id| *id != &TEST_PHYSICAL_POINTER_ID)
        .expect("second pointer facet");
    let live_source = live_info.source_id;
    let mut inventory = crate::core_loop::input_inventory::InputInventory::new();
    inventory.add(source_info);
    inventory.add(live_info);
    let mut pending = crate::core_loop::run::PendingBackendRequests::default();
    let mut lane = crate::core_loop::run::XiConfigLane::default();
    backend
        .device_config_start_results
        .push_back(Ok(DeviceConfigStart::Pending(DeviceConfigToken(207))));

    // B submits a write on the other, still-enabled source.
    let request_b = parse_t3_xi2_config_request(
        &mut state,
        &mut backend,
        2,
        4,
        live_device,
        "libinput Accel Speed",
        crate::xinput::XI_PROP_MODE_REPLACE,
        32,
        &0.75_f32.to_le_bytes(),
    );
    route_t3_config_request(
        &mut state,
        &mut backend,
        &mut inventory,
        &mut pending,
        &mut lane,
        request_b,
    );
    assert_eq!(backend.started_device_configs.len(), 1);
    assert!(!lane.is_empty(), "B's backend operation remains in flight");
    assert!(pending.xi_config_client_is_blocked_for_test(yserver_protocol::x11::ClientId(2)));

    // A's source-specific request queues behind B and retains its XI
    // minor opcode and sequence for an immediate lifecycle error.
    let request_a = if xi1_request {
        parse_t3_xi1_config_request(
            &mut state,
            &mut backend,
            1,
            9,
            TEST_PHYSICAL_POINTER_ID,
            &0.25_f32.to_le_bytes(),
        )
    } else {
        parse_t3_xi2_config_request(
            &mut state,
            &mut backend,
            1,
            9,
            TEST_PHYSICAL_POINTER_ID,
            "libinput Accel Speed",
            crate::xinput::XI_PROP_MODE_REPLACE,
            32,
            &0.25_f32.to_le_bytes(),
        )
    };
    route_t3_config_request(
        &mut state,
        &mut backend,
        &mut inventory,
        &mut pending,
        &mut lane,
        request_a,
    );
    assert!(pending.xi_config_client_is_blocked_for_test(yserver_protocol::x11::ClientId(1)));
    assert!(read_all_available(&mut peer_a).is_empty());
    assert!(read_all_available(&mut peer_b).is_empty());

    // This mirrors the registry transition made by the production KMS
    // backend in handle_host_input; the shared run_core dispatcher below
    // performs inventory maintenance, calls handle_host_input, and
    // cancels the queued XI write.
    let lifecycle_event = if removed {
        state.xi_unregister_source(source);
        crate::core_loop::HostInputEvent::DeviceRemoved { source_id: source }
    } else {
        let mut disabled = state.xi_devices.source(source).unwrap().clone();
        disabled.enabled = false;
        state.xi_devices.register(&disabled);
        crate::core_loop::HostInputEvent::DeviceSuspended { source_id: source }
    };
    crate::core_loop::run::dispatch_host_input(
        &mut state,
        &mut backend,
        &mut inventory,
        &mut pending,
        &mut lane,
        &mut crate::core_loop::reset::ResetTrigger::new(
            crate::core_loop::reset::ResetPolicy::NoReset,
        ),
        lifecycle_event,
        crate::core_loop::generation::Generation::default(),
    );

    let expected_error = if removed {
        XI2_FIRST_ERROR
    } else {
        x11::error::BAD_MATCH
    };
    let minor_opcode = if xi1_request { 37 } else { 57 };
    let cancelled = read_all_available(&mut peer_a);
    assert_xi_config_error(&cancelled, expected_error, 9, minor_opcode);
    assert!(
        !pending.xi_config_client_is_blocked_for_test(yserver_protocol::x11::ClientId(1)),
        "lifecycle cancellation unblocks A immediately"
    );
    assert!(pending.xi_config_client_is_blocked_for_test(yserver_protocol::x11::ClientId(2)));
    assert!(!lane.is_empty(), "B remains in flight after A is cancelled");
    assert_eq!(backend.started_device_configs.len(), 1, "A never starts");
    assert!(read_all_available(&mut peer_b).is_empty());

    if removed {
        assert!(state.xi_devices.source(source).is_none());
        assert!(inventory.get(source).is_none());
        assert!(state.xi_devices.device(TEST_PHYSICAL_POINTER_ID).is_none());
    } else {
        assert!(!state.xi_devices.source(source).unwrap().enabled);
        assert!(!inventory.get(source).unwrap().enabled);
        assert_eq!(
            state
                .xi_devices
                .device(TEST_PHYSICAL_POINTER_ID)
                .unwrap()
                .properties[&accel_atom]
                .data,
            source_property_before,
            "suspension leaves source S's property unchanged"
        );
    }

    crate::core_loop::run::dispatch_device_config_result(
        &mut state,
        &mut backend,
        &mut inventory,
        &mut pending,
        &mut lane,
        &mut crate::core_loop::reset::ResetTrigger::new(
            crate::core_loop::reset::ResetPolicy::NoReset,
        ),
        crate::core_loop::message::Message::DeviceConfigResult {
            token: DeviceConfigToken(207),
            source: live_source,
            result: Ok(()),
        },
        crate::core_loop::generation::Generation::default(),
    );

    assert_eq!(
        backend.started_device_configs,
        [(live_source, DeviceConfigChange::AccelSpeed(0.75))],
        "only B's enabled-source write reaches the backend"
    );
    assert_eq!(
        state.xi_devices.device(live_device).unwrap().properties[&accel_atom].data,
        0.75_f32.to_le_bytes(),
        "B's completion commits normally"
    );
    assert_eq!(
        inventory.get(live_source).unwrap().config.accel.current,
        0.75
    );
    if removed {
        assert!(state.xi_devices.source(source).is_none());
        assert!(inventory.get(source).is_none());
    } else {
        assert_eq!(
            state
                .xi_devices
                .device(TEST_PHYSICAL_POINTER_ID)
                .unwrap()
                .properties[&accel_atom]
                .data,
            source_property_before,
            "S's property remains unchanged after B completes"
        );
    }
    assert!(lane.is_empty());
    assert!(pending.is_empty());
    assert!(read_all_available(&mut peer_a).is_empty());
    assert!(read_all_available(&mut peer_b).is_empty());
}

#[test]
fn xi_config_completion_writable_descriptor_commits_to_registry() {
    let mut state = ServerState::new();
    let mut peer = install_capture_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    seed_pointer_for_t3(&mut state);
    let (mut inventory, source) = inventory_for_t3_source(&state);
    let mut pending = crate::core_loop::run::PendingBackendRequests::default();
    let mut lane = crate::core_loop::run::XiConfigLane::default();
    let tap_atom = state
        .atoms
        .intern(crate::xinput::PROP_TAPPING_ENABLED, false)
        .0;
    let integer_atom = crate::xinput::XA_INTEGER.0;

    // Write `[0]` (disable tap) to a writable Bool descriptor.
    let body = xi2_change_property_body(
        TEST_PHYSICAL_POINTER_ID,
        crate::xinput::XI_PROP_MODE_REPLACE,
        8,
        tap_atom,
        integer_atom,
        &[0],
    );
    drive_t3_config_wire_request(
        &mut state,
        &mut peer,
        &mut backend,
        &mut inventory,
        &mut pending,
        &mut lane,
        1,
        1,
        xi2_header(57),
        &body,
    );
    assert!(
        read_all_available(&mut peer).is_empty(),
        "successful XIChangeProperty has no reply"
    );
    // Registry now holds the new value.
    let dev = state
        .xi_devices
        .iter()
        .find(|d| d.id == TEST_PHYSICAL_POINTER_ID)
        .unwrap();
    let prop = dev.properties.get(&AtomId(tap_atom)).expect("tap stored");
    assert_eq!(prop.format, 8);
    assert_eq!(prop.data, vec![0], "tap toggled off");
    assert_confirmed_tap_write(&state, &inventory, source, AtomId(tap_atom));
    assert!(!inventory.get(source).unwrap().config.tap.current);
    assert_eq!(backend.started_device_configs.len(), 1);
    assert!(lane.is_empty());
    assert!(pending.is_empty());
}

// -----------------------------------------------------------------
// T3 B1 (review round): `dispatch_change_property` must pin the
// request's `format`/`type_atom` to the descriptor's before running
// `validate_value`/`decode_change` — the latter's fixed-width
// decoders (`float32`, `card32`) index the value slice assuming
// `validate_value` already enforced the descriptor's real width.
// Before the fix, a request lying about `format` slipped a
// too-short value past `validate_value` (which derived its expected
// length from the *request's* format) and then panicked on
// out-of-bounds indexing in `decode_change` — a client-reachable
// server crash. These tests drive the real dispatch path (not
// `validate_value` in isolation) so a regression shows up as either
// a wrong error or a test-thread panic.
// -----------------------------------------------------------------

/// Build an `xChangeDevicePropertyReq` body (after the 4-byte
/// generic header): property(4), type(4), deviceid(1), format(1),
/// mode(1), pad(1), num_items(4), value(num_items * format/8,
/// padded to 4 bytes) — mirrors `xi2_change_property_body` for the
/// XI1 wire layout (XIproto.h:1462-1473).
fn xi1_change_property_body(
    property: u32,
    type_atom: u32,
    deviceid: u8,
    format: u8,
    mode: u8,
    data: &[u8],
) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&property.to_le_bytes());
    body.extend_from_slice(&type_atom.to_le_bytes());
    body.push(deviceid);
    body.push(format);
    body.push(mode);
    body.push(0); // pad
    let num_items = data.len() / usize::from(format / 8);
    body.extend_from_slice(&(num_items as u32).to_le_bytes());
    body.extend_from_slice(data);
    while body.len() % 4 != 0 {
        body.push(0);
    }
    body
}

#[test]
fn xi_config_completion_b1_xi2_format8_scalar_float_mismatch_is_badmatch_no_panic() {
    // `libinput Accel Speed` is Scalar/Float, descriptor format 32.
    // A `format=8, num_items=1` write used to pass `validate_value`
    // (which computed its expected length from the request's
    // format) and then panic in `decode_change`'s `float32(&[7])`
    // (`b[1]` out of bounds).
    let mut state = ServerState::new();
    let mut peer = install_capture_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    seed_pointer_for_t3(&mut state);
    let (mut inventory, _) = inventory_for_t3_source(&state);
    let mut pending = crate::core_loop::run::PendingBackendRequests::default();
    let mut lane = crate::core_loop::run::XiConfigLane::default();
    let properties_before = xi_property_snapshot(&state);
    let accel_speed_atom = state.atoms.intern("libinput Accel Speed", false).0;

    let body = xi2_change_property_body(
        TEST_PHYSICAL_POINTER_ID,
        crate::xinput::XI_PROP_MODE_REPLACE,
        8,
        accel_speed_atom,
        crate::xinput::XA_INTEGER.0,
        &[7],
    );
    drive_t3_config_wire_request(
        &mut state,
        &mut peer,
        &mut backend,
        &mut inventory,
        &mut pending,
        &mut lane,
        1,
        1,
        xi2_header(57),
        &body,
    );
    let wire = read_all_available(&mut peer);
    assert_xi_config_error(&wire, x11::error::BAD_MATCH, 1, 57);
    assert_eq!(xi_property_snapshot(&state), properties_before);
    assert!(backend.started_device_configs.is_empty());
    assert!(lane.is_empty());
    assert!(pending.is_empty());
}

#[test]
fn xi_config_completion_b1_xi2_format16_scalar_float_mismatch_is_badmatch() {
    // Same target, format=16 (still not the descriptor's 32).
    let mut state = ServerState::new();
    let mut peer = install_capture_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    seed_pointer_for_t3(&mut state);
    let (mut inventory, _) = inventory_for_t3_source(&state);
    let mut pending = crate::core_loop::run::PendingBackendRequests::default();
    let mut lane = crate::core_loop::run::XiConfigLane::default();
    let properties_before = xi_property_snapshot(&state);
    let accel_speed_atom = state.atoms.intern("libinput Accel Speed", false).0;

    let body = xi2_change_property_body(
        TEST_PHYSICAL_POINTER_ID,
        crate::xinput::XI_PROP_MODE_REPLACE,
        16,
        accel_speed_atom,
        crate::xinput::XA_INTEGER.0,
        &[7, 0],
    );
    drive_t3_config_wire_request(
        &mut state,
        &mut peer,
        &mut backend,
        &mut inventory,
        &mut pending,
        &mut lane,
        1,
        1,
        xi2_header(57),
        &body,
    );
    let wire = read_all_available(&mut peer);
    assert_xi_config_error(&wire, x11::error::BAD_MATCH, 1, 57);
    assert_eq!(xi_property_snapshot(&state), properties_before);
    assert!(backend.started_device_configs.is_empty());
    assert!(lane.is_empty());
    assert!(pending.is_empty());
}

#[test]
fn xi_config_completion_b1_xi2_format8_card32_mismatch_is_badmatch_no_panic() {
    // `libinput Button Scrolling Button` is Scalar/Card32,
    // descriptor format 32. Same crash shape via `card32(&[7])`.
    let mut state = ServerState::new();
    let mut peer = install_capture_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    seed_pointer_for_t3(&mut state);
    let (mut inventory, _) = inventory_for_t3_source(&state);
    let mut pending = crate::core_loop::run::PendingBackendRequests::default();
    let mut lane = crate::core_loop::run::XiConfigLane::default();
    let properties_before = xi_property_snapshot(&state);
    let scroll_button_atom = state
        .atoms
        .intern("libinput Button Scrolling Button", false)
        .0;

    let body = xi2_change_property_body(
        TEST_PHYSICAL_POINTER_ID,
        crate::xinput::XI_PROP_MODE_REPLACE,
        8,
        scroll_button_atom,
        crate::xinput::XA_INTEGER.0,
        &[7],
    );
    drive_t3_config_wire_request(
        &mut state,
        &mut peer,
        &mut backend,
        &mut inventory,
        &mut pending,
        &mut lane,
        1,
        1,
        xi2_header(57),
        &body,
    );
    let wire = read_all_available(&mut peer);
    assert_xi_config_error(&wire, x11::error::BAD_MATCH, 1, 57);
    assert_eq!(xi_property_snapshot(&state), properties_before);
    assert!(backend.started_device_configs.is_empty());
    assert!(lane.is_empty());
    assert!(pending.is_empty());
}

/// The short-write relaxation is per-descriptor, not blanket.
///
/// `xf86-input-libinput` gates every 8-bit multi-slot property on an
/// exact `val->size` except `Accel Profile Enabled`, whose width grew
/// from 2 to 3 with libinput 1.23's custom-accel slot
/// (`LibinputSetPropertyAccelProfile`, xf86libinput.c:4622 —
/// `val->size < 2 || val->size > 3`; compare `ScrollMethods` :4884
/// `!= 3`, `ClickMethod` :4988 `!= 2`).
///
/// Accepting a short `Scroll Method Enabled` and zero-padding it
/// would silently commit "two-finger on, edge off, button off" — a
/// configuration the client never expressed, and BadMatch on Xorg.
#[test]
fn xi_config_completion_short_write_only_for_ranged_accel_profile_descriptor() {
    let mut state = ServerState::new();
    let mut peer = install_capture_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    seed_pointer_for_t3(&mut state);
    let (mut inventory, source) = inventory_for_t3_source(&state);
    let mut pending = crate::core_loop::run::PendingBackendRequests::default();
    let mut lane = crate::core_loop::run::XiConfigLane::default();
    let scroll_atom = state
        .atoms
        .intern("libinput Scroll Method Enabled", false)
        .0;

    // Two bytes against a 3-wide exact descriptor → BadMatch.
    let body = xi2_change_property_body(
        TEST_PHYSICAL_POINTER_ID,
        crate::xinput::XI_PROP_MODE_REPLACE,
        8,
        scroll_atom,
        crate::xinput::XA_INTEGER.0,
        &[0, 1],
    );
    drive_t3_config_wire_request(
        &mut state,
        &mut peer,
        &mut backend,
        &mut inventory,
        &mut pending,
        &mut lane,
        1,
        1,
        xi2_header(57),
        &body,
    );
    let wire = read_all_available(&mut peer);
    assert_xi_config_error(&wire, x11::error::BAD_MATCH, 1, 57);
    assert!(backend.started_device_configs.is_empty());
    assert!(lane.is_empty());
    assert!(pending.is_empty());

    // The same two-byte shape against Accel Profile Enabled is the
    // write mate-settings-daemon emits, and must be accepted.
    let accel_atom = state
        .atoms
        .intern("libinput Accel Profile Enabled", false)
        .0;
    let body = xi2_change_property_body(
        TEST_PHYSICAL_POINTER_ID,
        crate::xinput::XI_PROP_MODE_REPLACE,
        8,
        accel_atom,
        crate::xinput::XA_INTEGER.0,
        &[0, 1],
    );
    drive_t3_config_wire_request(
        &mut state,
        &mut peer,
        &mut backend,
        &mut inventory,
        &mut pending,
        &mut lane,
        1,
        2,
        xi2_header(57),
        &body,
    );
    assert!(
        read_all_available(&mut peer).is_empty(),
        "two-item accel-profile write must succeed (no error packet)",
    );
    assert_eq!(backend.started_device_configs.len(), 1);
    assert_eq!(
        inventory.get(source).unwrap().config.accel_profile.current,
        Some(1)
    );
    assert_eq!(
        state
            .xi_devices
            .device(TEST_PHYSICAL_POINTER_ID)
            .unwrap()
            .properties[&AtomId(accel_atom)]
            .data,
        [0, 1, 0]
    );
    assert!(lane.is_empty());
    assert!(pending.is_empty());
}

#[test]
fn xi_config_completion_b1_xi1_format_mismatch_is_badmatch_no_panic() {
    // Same crash shape, XI1 wire arm (minor 37) — the path MATE's
    // settings daemon actually uses.
    let mut state = ServerState::new();
    let mut peer = install_capture_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    seed_pointer_for_t3(&mut state);
    let (mut inventory, _) = inventory_for_t3_source(&state);
    let mut pending = crate::core_loop::run::PendingBackendRequests::default();
    let mut lane = crate::core_loop::run::XiConfigLane::default();
    let properties_before = xi_property_snapshot(&state);
    let accel_speed_atom = state.atoms.intern("libinput Accel Speed", false).0;

    let body = xi1_change_property_body(
        accel_speed_atom,
        crate::xinput::XA_INTEGER.0,
        u8::try_from(TEST_PHYSICAL_POINTER_ID).unwrap(),
        8,
        crate::xinput::XI_PROP_MODE_REPLACE,
        &[7],
    );
    drive_t3_config_wire_request(
        &mut state,
        &mut peer,
        &mut backend,
        &mut inventory,
        &mut pending,
        &mut lane,
        1,
        1,
        xi2_header(37),
        &body,
    );
    let wire = read_all_available(&mut peer);
    assert_xi_config_error(&wire, x11::error::BAD_MATCH, 1, 37);
    assert_eq!(xi_property_snapshot(&state), properties_before);
    assert!(backend.started_device_configs.is_empty());
    assert!(lane.is_empty());
    assert!(pending.is_empty());
}

// -----------------------------------------------------------------
// T3 B2 (review round): merge-then-validate. `Append`/`Prepend`
// must be merged with the existing stored value BEFORE
// `validate_value` runs, or a short *fragment* (legal on its own
// since Task 1) can pass validation, decode to a value the client
// never wrote, and reach `apply_device_config`. The commit must
// also store the *normalised* merged value, not the raw request
// bytes, or a short Replace silently narrows the property's
// advertised width for the next reader.
// -----------------------------------------------------------------

#[test]
fn xi_config_completion_t3_b2_xi2_two_item_accel_profile_commits_normalized_bytes() {
    // The headline vector: msd writes exactly 2 bytes; the merged
    // (here: unmerged, since Replace) value must validate, decode
    // to AccelProfile(Some(1)) — flat — and the STORED property
    // must be normalised to the full 3-byte descriptor width, not
    // the 2 raw bytes the client sent.
    let mut state = ServerState::new();
    let mut peer = install_capture_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    seed_pointer_for_t3(&mut state);
    let (mut inventory, source) = inventory_for_t3_source(&state);
    let mut pending = crate::core_loop::run::PendingBackendRequests::default();
    let mut lane = crate::core_loop::run::XiConfigLane::default();
    let atom = state
        .atoms
        .intern("libinput Accel Profile Enabled", false)
        .0;
    let integer_atom = crate::xinput::XA_INTEGER.0;

    let body = xi2_change_property_body(
        TEST_PHYSICAL_POINTER_ID,
        crate::xinput::XI_PROP_MODE_REPLACE,
        8,
        atom,
        integer_atom,
        &[0, 1],
    );
    let outcome = handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        xi2_header(57),
        &body,
    )
    .unwrap();
    let crate::core_loop::process_request::RequestOutcome::PendingXiConfig(request) = outcome
    else {
        panic!("recognized XI2 write must enter the core config lane")
    };
    assert!(
        read_all_available(&mut peer).is_empty(),
        "no X error for a short write"
    );
    assert!(
        backend.started_device_configs.is_empty(),
        "request parsing does not start the backend"
    );
    let dev = state.xi_devices.device(TEST_PHYSICAL_POINTER_ID).unwrap();
    assert_eq!(
        dev.properties.get(&AtomId(atom)).unwrap().data,
        vec![1, 0, 0],
        "request parsing leaves the seeded property unchanged"
    );

    route_t3_config_request(
        &mut state,
        &mut backend,
        &mut inventory,
        &mut pending,
        &mut lane,
        request,
    );
    assert_eq!(
        backend.started_device_configs,
        vec![(
            source,
            crate::xinput::libinput_props::DeviceConfigChange::AccelProfile(Some(1)),
        )],
        "reaches the backend as AccelProfile(Some(1)) — flat"
    );
    assert!(lane.is_empty(), "synchronous confirmation drains the lane");
    assert!(
        pending.is_empty(),
        "synchronous confirmation unblocks the client"
    );
    assert_eq!(
        inventory.get(source).unwrap().config.accel_profile.current,
        Some(1),
        "confirmed value is retained in the process-lifetime inventory"
    );

    let dev = state
        .xi_devices
        .iter()
        .find(|d| d.id == TEST_PHYSICAL_POINTER_ID)
        .unwrap();
    let prop = dev
        .properties
        .get(&AtomId(atom))
        .expect("accel profile stored");
    assert_eq!(
        prop.data,
        vec![0, 1, 0],
        "stored property is exactly 3 bytes, zero-padded"
    );
}

#[test]
fn xi_config_completion_t3_b2_xi1_two_item_accel_profile_commits_normalized_bytes() {
    // Same assertions, XI1 wire arm (minor 37).
    let mut state = ServerState::new();
    let mut peer = install_capture_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    seed_pointer_for_t3(&mut state);
    let atom = state
        .atoms
        .intern("libinput Accel Profile Enabled", false)
        .0;
    let integer_atom = crate::xinput::XA_INTEGER.0;

    let body = xi1_change_property_body(
        atom,
        integer_atom,
        u8::try_from(TEST_PHYSICAL_POINTER_ID).unwrap(),
        8,
        crate::xinput::XI_PROP_MODE_REPLACE,
        &[0, 1],
    );
    let (mut inventory, source) = inventory_for_t3_source(&state);
    let mut pending = crate::core_loop::run::PendingBackendRequests::default();
    let mut lane = crate::core_loop::run::XiConfigLane::default();
    let outcome = handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        xi2_header(37),
        &body,
    )
    .unwrap();
    let crate::core_loop::process_request::RequestOutcome::PendingXiConfig(request) = outcome
    else {
        panic!("recognized XI1 write must enter the core config lane")
    };
    assert!(
        read_all_available(&mut peer).is_empty(),
        "no X error for a short write"
    );
    assert!(
        backend.started_device_configs.is_empty(),
        "request parsing does not start the backend"
    );
    let dev = state.xi_devices.device(TEST_PHYSICAL_POINTER_ID).unwrap();
    assert_eq!(
        dev.properties.get(&AtomId(atom)).unwrap().data,
        vec![1, 0, 0],
        "request parsing leaves the seeded property unchanged"
    );
    route_t3_config_request(
        &mut state,
        &mut backend,
        &mut inventory,
        &mut pending,
        &mut lane,
        request,
    );
    assert_eq!(
        backend.started_device_configs,
        vec![(
            source,
            crate::xinput::libinput_props::DeviceConfigChange::AccelProfile(Some(1)),
        )],
        "reaches the backend as AccelProfile(Some(1)) — flat"
    );
    assert!(lane.is_empty(), "synchronous confirmation drains the lane");
    assert!(
        pending.is_empty(),
        "synchronous confirmation unblocks the client"
    );
    assert_eq!(
        inventory.get(source).unwrap().config.accel_profile.current,
        Some(1),
        "confirmed value is retained in the process-lifetime inventory"
    );

    let dev = state
        .xi_devices
        .iter()
        .find(|d| d.id == TEST_PHYSICAL_POINTER_ID)
        .unwrap();
    let prop = dev
        .properties
        .get(&AtomId(atom))
        .expect("accel profile stored");
    assert_eq!(
        prop.data,
        vec![0, 1, 0],
        "stored property is exactly 3 bytes, zero-padded"
    );
}

#[test]
fn xi_config_completion_t3_b2_full_width_accel_profile_append_is_badvalue_and_untouched() {
    // Append [1] onto an already-full-width (3-byte) stored value:
    // the merged length is 4 > n=3, so this must be BadValue with
    // BOTH the stored property and the libinput config untouched —
    // in particular, `apply_device_config` must NOT be called a
    // second time with the wrong fragment-decoded value.
    let mut state = ServerState::new();
    let mut peer = install_capture_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    seed_pointer_for_t3(&mut state);
    let (mut inventory, source) = inventory_for_t3_source(&state);
    let mut pending = crate::core_loop::run::PendingBackendRequests::default();
    let mut lane = crate::core_loop::run::XiConfigLane::default();
    let atom = state
        .atoms
        .intern("libinput Accel Profile Enabled", false)
        .0;
    let integer_atom = crate::xinput::XA_INTEGER.0;

    // Seed a full-width value first (Replace [1, 0, 0] = adaptive).
    let seed_body = xi2_change_property_body(
        TEST_PHYSICAL_POINTER_ID,
        crate::xinput::XI_PROP_MODE_REPLACE,
        8,
        atom,
        integer_atom,
        &[1, 0, 0],
    );
    let outcome = handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        xi2_header(57),
        &seed_body,
    )
    .unwrap();
    let crate::core_loop::process_request::RequestOutcome::PendingXiConfig(seed_request) = outcome
    else {
        panic!("setup write must enter the core config lane")
    };
    assert!(backend.started_device_configs.is_empty());
    assert_eq!(
        state
            .xi_devices
            .device(TEST_PHYSICAL_POINTER_ID)
            .unwrap()
            .properties
            .get(&AtomId(atom))
            .unwrap()
            .data,
        vec![1, 0, 0],
        "the setup request has not committed before lane processing"
    );
    route_t3_config_request(
        &mut state,
        &mut backend,
        &mut inventory,
        &mut pending,
        &mut lane,
        seed_request,
    );
    assert!(read_all_available(&mut peer).is_empty());
    assert_eq!(backend.started_device_configs.len(), 1);
    assert_eq!(
        inventory.get(source).unwrap().config.accel_profile.current,
        Some(0)
    );

    // Append [1] → merged length 4 > n=3 → BadValue, untouched.
    let append_body = xi2_change_property_body(
        TEST_PHYSICAL_POINTER_ID,
        crate::xinput::XI_PROP_MODE_APPEND,
        8,
        atom,
        integer_atom,
        &[1],
    );
    let outcome = handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(2),
        xi2_header(57),
        &append_body,
    )
    .unwrap();
    let crate::core_loop::process_request::RequestOutcome::PendingXiConfig(append_request) =
        outcome
    else {
        panic!("recognized append must enter the core config lane")
    };
    assert_eq!(
        backend.started_device_configs.len(),
        1,
        "append has not started"
    );
    assert_eq!(
        state
            .xi_devices
            .device(TEST_PHYSICAL_POINTER_ID)
            .unwrap()
            .properties
            .get(&AtomId(atom))
            .unwrap()
            .data,
        vec![1, 0, 0],
        "request parsing leaves the confirmed full-width value untouched"
    );
    assert!(
        read_all_available(&mut peer).is_empty(),
        "no reply or event before validation"
    );
    route_t3_config_request(
        &mut state,
        &mut backend,
        &mut inventory,
        &mut pending,
        &mut lane,
        append_request,
    );
    let wire = read_all_available(&mut peer);
    assert_xi_config_error(&wire, x11::error::BAD_VALUE, 2, 57);

    let dev = state
        .xi_devices
        .iter()
        .find(|d| d.id == TEST_PHYSICAL_POINTER_ID)
        .unwrap();
    let prop = dev.properties.get(&AtomId(atom)).unwrap();
    assert_eq!(
        prop.data,
        vec![1, 0, 0],
        "stored property untouched by the rejected append"
    );
    assert_eq!(
        backend.started_device_configs.len(),
        1,
        "only the seeding Replace reached start_device_config, not the rejected Append"
    );
}

#[test]
fn xi_config_completion_immediate_success_updates_registry_and_inventory() {
    use crate::xinput::libinput_props::DeviceConfigChange;

    let mut state = ServerState::new();
    let mut peer = install_capture_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    seed_pointer_for_t3(&mut state);
    let (mut inventory, source) = inventory_for_t3_source(&state);
    let mut pending = crate::core_loop::run::PendingBackendRequests::default();
    let mut lane = crate::core_loop::run::XiConfigLane::default();
    let request = parse_t3_xi2_config_request(
        &mut state,
        &mut backend,
        1,
        1,
        TEST_PHYSICAL_POINTER_ID,
        "libinput Accel Speed",
        crate::xinput::XI_PROP_MODE_REPLACE,
        32,
        &0.5_f32.to_le_bytes(),
    );
    let atom = request.property;

    assert!(backend.started_device_configs.is_empty());
    assert_eq!(
        state
            .xi_devices
            .device(TEST_PHYSICAL_POINTER_ID)
            .unwrap()
            .properties[&atom]
            .data,
        0.0_f32.to_le_bytes(),
        "request parsing has not changed the XI property"
    );
    route_t3_config_request(
        &mut state,
        &mut backend,
        &mut inventory,
        &mut pending,
        &mut lane,
        request,
    );

    assert_eq!(
        backend.started_device_configs,
        [(source, DeviceConfigChange::AccelSpeed(0.5))]
    );
    assert_eq!(
        state
            .xi_devices
            .device(TEST_PHYSICAL_POINTER_ID)
            .unwrap()
            .properties[&atom]
            .data,
        0.5_f32.to_le_bytes()
    );
    assert_eq!(inventory.get(source).unwrap().config.accel.current, 0.5);
    assert!(lane.is_empty());
    assert!(pending.is_empty());
    assert!(read_all_available(&mut peer).is_empty());
}

#[test]
fn xi_config_completion_pending_success_commits_only_after_message() {
    use crate::xinput::libinput_props::{DeviceConfigChange, DeviceConfigStart, DeviceConfigToken};

    let mut state = ServerState::new();
    let mut peer = install_capture_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    seed_pointer_for_t3(&mut state);
    let (mut inventory, source) = inventory_for_t3_source(&state);
    let mut pending = crate::core_loop::run::PendingBackendRequests::default();
    let mut lane = crate::core_loop::run::XiConfigLane::default();
    select_xi2_property_event_on_root(&mut state, 1, TEST_PHYSICAL_POINTER_ID);
    let original = 0.0_f32.to_le_bytes();
    backend
        .device_config_start_results
        .push_back(Ok(DeviceConfigStart::Pending(DeviceConfigToken(201))));
    let request = parse_t3_xi2_config_request(
        &mut state,
        &mut backend,
        1,
        1,
        TEST_PHYSICAL_POINTER_ID,
        "libinput Accel Speed",
        crate::xinput::XI_PROP_MODE_REPLACE,
        32,
        &0.5_f32.to_le_bytes(),
    );
    let atom = request.property;
    route_t3_config_request(
        &mut state,
        &mut backend,
        &mut inventory,
        &mut pending,
        &mut lane,
        request,
    );

    assert_eq!(
        backend.started_device_configs,
        [(source, DeviceConfigChange::AccelSpeed(0.5))]
    );
    assert!(
        !lane.is_empty(),
        "submitted write holds the lane until acknowledgment"
    );
    assert!(!pending.is_empty(), "originating client remains blocked");
    assert_eq!(
        state
            .xi_devices
            .device(TEST_PHYSICAL_POINTER_ID)
            .unwrap()
            .properties[&atom]
            .data,
        original,
        "XI value remains old until the input thread confirms success"
    );
    assert_eq!(inventory.get(source).unwrap().config.accel.current, 0.0);
    assert!(
        read_all_available(&mut peer).is_empty(),
        "no notification before success"
    );

    crate::core_loop::run::dispatch_device_config_result(
        &mut state,
        &mut backend,
        &mut inventory,
        &mut pending,
        &mut lane,
        &mut crate::core_loop::reset::ResetTrigger::new(
            crate::core_loop::reset::ResetPolicy::NoReset,
        ),
        crate::core_loop::message::Message::DeviceConfigResult {
            token: DeviceConfigToken(201),
            source,
            result: Ok(()),
        },
        crate::core_loop::generation::Generation::default(),
    );

    let wire = read_all_available(&mut peer);
    let event = find_xi2_property_event(&wire).expect("success emits XI_PropertyEvent");
    assert_eq!(event[20], crate::xinput::PropWhat::Modified as u8);
    assert_eq!(
        state
            .xi_devices
            .device(TEST_PHYSICAL_POINTER_ID)
            .unwrap()
            .properties[&atom]
            .data,
        0.5_f32.to_le_bytes()
    );
    assert_eq!(inventory.get(source).unwrap().config.accel.current, 0.5);
    assert!(lane.is_empty());
    assert!(pending.is_empty());
}

#[test]
fn xi_config_completion_start_and_async_errors_leave_values_untouched() {
    use crate::xinput::libinput_props::{
        DeviceConfigError as ConfigError, DeviceConfigStart, DeviceConfigToken,
    };

    for (start_result, async_error, expected_error) in [
        (Err(ConfigError::Unsupported), None, x11::error::BAD_MATCH),
        (Err(ConfigError::Invalid), None, x11::error::BAD_VALUE),
        // Xorg's libinput property handler returns BadMatch when the
        // shared handle is absent (xf86libinput.c:4392-4409, 4579-4607).
        (Err(ConfigError::SourceGone), None, x11::error::BAD_MATCH),
        (
            Ok(DeviceConfigStart::Pending(DeviceConfigToken(203))),
            Some(ConfigError::Invalid),
            x11::error::BAD_VALUE,
        ),
        (
            Ok(DeviceConfigStart::Pending(DeviceConfigToken(203))),
            Some(ConfigError::Unsupported),
            x11::error::BAD_MATCH,
        ),
        (
            Ok(DeviceConfigStart::Pending(DeviceConfigToken(203))),
            Some(ConfigError::SourceGone),
            x11::error::BAD_MATCH,
        ),
    ] {
        let mut state = ServerState::new();
        let mut peer = install_capture_client(&mut state, 1);
        let mut backend = RecordingBackend::new();
        seed_pointer_for_t3(&mut state);
        let (mut inventory, source) = inventory_for_t3_source(&state);
        let mut pending = crate::core_loop::run::PendingBackendRequests::default();
        let mut lane = crate::core_loop::run::XiConfigLane::default();
        let request = parse_t3_xi2_config_request(
            &mut state,
            &mut backend,
            1,
            3,
            TEST_PHYSICAL_POINTER_ID,
            "libinput Accel Speed",
            crate::xinput::XI_PROP_MODE_REPLACE,
            32,
            &0.5_f32.to_le_bytes(),
        );
        let atom = request.property;
        let old_property = state
            .xi_devices
            .device(TEST_PHYSICAL_POINTER_ID)
            .unwrap()
            .properties[&atom]
            .clone();
        backend.device_config_start_results.push_back(start_result);

        route_t3_config_request(
            &mut state,
            &mut backend,
            &mut inventory,
            &mut pending,
            &mut lane,
            request,
        );
        assert_eq!(backend.started_device_configs.len(), 1);
        if let Some(error) = async_error {
            assert!(!lane.is_empty());
            crate::core_loop::run::dispatch_device_config_result(
                &mut state,
                &mut backend,
                &mut inventory,
                &mut pending,
                &mut lane,
                &mut crate::core_loop::reset::ResetTrigger::new(
                    crate::core_loop::reset::ResetPolicy::NoReset,
                ),
                crate::core_loop::message::Message::DeviceConfigResult {
                    token: DeviceConfigToken(203),
                    source,
                    result: Err(error),
                },
                crate::core_loop::generation::Generation::default(),
            );
        }

        let wire = read_all_available(&mut peer);
        assert_xi_config_error(&wire, expected_error, 3, 57);
        assert_eq!(
            state
                .xi_devices
                .device(TEST_PHYSICAL_POINTER_ID)
                .unwrap()
                .properties[&atom],
            old_property
        );
        assert_eq!(inventory.get(source).unwrap().config.accel.current, 0.0);
        assert!(lane.is_empty());
        assert!(pending.is_empty());
    }
}

#[test]
fn xi_config_completion_merges_ordered_writes_at_dequeue_time() {
    use crate::xinput::libinput_props::{DeviceConfigChange, DeviceConfigStart, DeviceConfigToken};

    let mut state = ServerState::new();
    let mut peer_a = install_capture_client(&mut state, 1);
    let mut peer_b = install_capture_client(&mut state, 2);
    let mut backend = RecordingBackend::new();
    seed_pointer_for_t3(&mut state);
    let (mut inventory, source) = inventory_for_t3_source(&state);
    let mut pending = crate::core_loop::run::PendingBackendRequests::default();
    let mut lane = crate::core_loop::run::XiConfigLane::default();
    select_xi2_property_event_on_root(&mut state, 1, TEST_PHYSICAL_POINTER_ID);
    select_xi2_property_event_on_root(&mut state, 2, TEST_PHYSICAL_POINTER_ID);
    backend
        .device_config_start_results
        .push_back(Ok(DeviceConfigStart::Pending(DeviceConfigToken(204))));

    let request_a = parse_t3_xi2_config_request(
        &mut state,
        &mut backend,
        1,
        1,
        TEST_PHYSICAL_POINTER_ID,
        "libinput Accel Speed",
        crate::xinput::XI_PROP_MODE_REPLACE,
        32,
        &0.5_f32.to_le_bytes(),
    );
    let atom = request_a.property;
    route_t3_config_request(
        &mut state,
        &mut backend,
        &mut inventory,
        &mut pending,
        &mut lane,
        request_a,
    );
    let request_b = parse_t3_xi2_config_request(
        &mut state,
        &mut backend,
        2,
        1,
        TEST_PHYSICAL_POINTER_ID,
        "libinput Accel Speed",
        crate::xinput::XI_PROP_MODE_APPEND,
        32,
        &[],
    );
    route_t3_config_request(
        &mut state,
        &mut backend,
        &mut inventory,
        &mut pending,
        &mut lane,
        request_b,
    );

    assert_eq!(
        backend.started_device_configs,
        [(source, DeviceConfigChange::AccelSpeed(0.5))],
        "B remains queued behind A"
    );
    assert_eq!(
        state
            .xi_devices
            .device(TEST_PHYSICAL_POINTER_ID)
            .unwrap()
            .properties[&atom]
            .data,
        0.0_f32.to_le_bytes()
    );
    assert!(read_all_available(&mut peer_a).is_empty());
    assert!(read_all_available(&mut peer_b).is_empty());

    crate::core_loop::run::dispatch_device_config_result(
        &mut state,
        &mut backend,
        &mut inventory,
        &mut pending,
        &mut lane,
        &mut crate::core_loop::reset::ResetTrigger::new(
            crate::core_loop::reset::ResetPolicy::NoReset,
        ),
        crate::core_loop::message::Message::DeviceConfigResult {
            token: DeviceConfigToken(204),
            source,
            result: Ok(()),
        },
        crate::core_loop::generation::Generation::default(),
    );

    assert_eq!(
        backend.started_device_configs,
        [
            (source, DeviceConfigChange::AccelSpeed(0.5)),
            (source, DeviceConfigChange::AccelSpeed(0.5)),
        ],
        "B's empty Append merges with A's confirmed 0.5 at dequeue"
    );
    assert_eq!(
        state
            .xi_devices
            .device(TEST_PHYSICAL_POINTER_ID)
            .unwrap()
            .properties[&atom]
            .data,
        0.5_f32.to_le_bytes()
    );
    assert_eq!(inventory.get(source).unwrap().config.accel.current, 0.5);
    for peer in [&mut peer_a, &mut peer_b] {
        let wire = read_all_available(peer);
        let event = find_xi2_property_event(&wire).expect("confirmed write notification");
        assert_eq!(event[20], crate::xinput::PropWhat::Modified as u8);
    }
    assert!(lane.is_empty());
    assert!(pending.is_empty());
}

#[test]
fn xi_config_completion_removal_cancels_queued_write_before_id_reuse() {
    use crate::xinput::libinput_props::{DeviceConfigChange, DeviceConfigStart, DeviceConfigToken};

    let mut state = ServerState::new();
    let mut peer_a = install_capture_client(&mut state, 1);
    let mut peer_b = install_capture_client(&mut state, 2);
    let mut backend = RecordingBackend::new();
    seed_pointer_for_t3(&mut state);
    let (mut inventory, source) = inventory_for_t3_source(&state);
    let original_info = state.xi_devices.source(source).unwrap().clone();
    let mut pending = crate::core_loop::run::PendingBackendRequests::default();
    let mut lane = crate::core_loop::run::XiConfigLane::default();
    backend
        .device_config_start_results
        .push_back(Ok(DeviceConfigStart::Pending(DeviceConfigToken(205))));

    let request_a = parse_t3_xi2_config_request(
        &mut state,
        &mut backend,
        1,
        1,
        TEST_PHYSICAL_POINTER_ID,
        "libinput Accel Speed",
        crate::xinput::XI_PROP_MODE_REPLACE,
        32,
        &0.25_f32.to_le_bytes(),
    );
    route_t3_config_request(
        &mut state,
        &mut backend,
        &mut inventory,
        &mut pending,
        &mut lane,
        request_a,
    );
    let request_b = parse_t3_xi2_config_request(
        &mut state,
        &mut backend,
        2,
        2,
        TEST_PHYSICAL_POINTER_ID,
        "libinput Accel Speed",
        crate::xinput::XI_PROP_MODE_REPLACE,
        32,
        &0.75_f32.to_le_bytes(),
    );
    route_t3_config_request(
        &mut state,
        &mut backend,
        &mut inventory,
        &mut pending,
        &mut lane,
        request_b,
    );
    assert_eq!(backend.started_device_configs.len(), 1);

    state.xi_unregister_source(source);
    inventory.remove(source);
    crate::core_loop::run::cancel_queued_xi_configs_for_source(
        &mut state,
        &mut backend,
        &mut pending,
        &mut lane,
        &mut crate::core_loop::reset::ResetTrigger::new(
            crate::core_loop::reset::ResetPolicy::NoReset,
        ),
        source,
        crate::core_loop::generation::Generation::default(),
    );
    let mut replacement = original_info;
    replacement.source_id = crate::xinput::InputSourceId(source.0 + 1);
    replacement.device_node = "/dev/input/event88".into();
    replacement.sysname = "event88".into();
    let replacement_ids = state.xi_register_source(&replacement);
    inventory.add(replacement.clone());
    assert!(replacement_ids.contains(&TEST_PHYSICAL_POINTER_ID));

    // B's device has already been unregistered, so ProcXIChangeProperty's
    // dixLookupDevice returns BadDevice (Xi/xiproperty.c:1140-1142;
    // dix/devices.c:1259-1274).
    let error_b = read_all_available(&mut peer_b);
    assert_xi_config_error(&error_b, XI2_FIRST_ERROR, 2, 57);
    assert_eq!(
        backend.started_device_configs.len(),
        1,
        "B never reached the backend"
    );

    crate::core_loop::run::dispatch_device_config_result(
        &mut state,
        &mut backend,
        &mut inventory,
        &mut pending,
        &mut lane,
        &mut crate::core_loop::reset::ResetTrigger::new(
            crate::core_loop::reset::ResetPolicy::NoReset,
        ),
        crate::core_loop::message::Message::DeviceConfigResult {
            token: DeviceConfigToken(205),
            source,
            result: Err(crate::xinput::libinput_props::DeviceConfigError::SourceGone),
        },
        crate::core_loop::generation::Generation::default(),
    );
    // A's backend result is SourceGone while its request still names the
    // in-flight device; libinput maps the missing shared handle to
    // BadMatch (xf86libinput.c:4392-4409, 4579-4607).
    let error_a = read_all_available(&mut peer_a);
    assert_xi_config_error(&error_a, x11::error::BAD_MATCH, 1, 57);

    let replacement_device = state
        .xi_devices
        .device(TEST_PHYSICAL_POINTER_ID)
        .expect("replacement reused the physical XI id");
    let replacement_atom = state.atoms.id_for("libinput Accel Speed").unwrap();
    assert_eq!(replacement_device.source_id, Some(replacement.source_id));
    assert_eq!(
        replacement_device.properties[&replacement_atom].data,
        replacement.config.accel.current.to_le_bytes()
    );
    assert_eq!(
        inventory
            .get(replacement.source_id)
            .unwrap()
            .config
            .accel
            .current,
        0.0
    );
    assert!(lane.is_empty());
    assert!(pending.is_empty());
    assert_eq!(
        backend.started_device_configs,
        [(source, DeviceConfigChange::AccelSpeed(0.25))]
    );
}

#[test]
fn xi_config_completion_disconnect_discards_replies_but_keeps_submitted_change() {
    use crate::xinput::libinput_props::{DeviceConfigChange, DeviceConfigStart, DeviceConfigToken};

    let mut state = ServerState::new();
    let mut peer_a = install_capture_client(&mut state, 1);
    let mut peer_b = install_capture_client(&mut state, 2);
    let mut backend = RecordingBackend::new();
    seed_pointer_for_t3(&mut state);
    let (mut inventory, source) = inventory_for_t3_source(&state);
    let mut pending = crate::core_loop::run::PendingBackendRequests::default();
    let mut lane = crate::core_loop::run::XiConfigLane::default();
    backend
        .device_config_start_results
        .push_back(Ok(DeviceConfigStart::Pending(DeviceConfigToken(206))));

    let request_a = parse_t3_xi2_config_request(
        &mut state,
        &mut backend,
        1,
        1,
        TEST_PHYSICAL_POINTER_ID,
        "libinput Accel Speed",
        crate::xinput::XI_PROP_MODE_REPLACE,
        32,
        &0.5_f32.to_le_bytes(),
    );
    route_t3_config_request(
        &mut state,
        &mut backend,
        &mut inventory,
        &mut pending,
        &mut lane,
        request_a,
    );
    let request_b = parse_t3_xi2_config_request(
        &mut state,
        &mut backend,
        2,
        2,
        TEST_PHYSICAL_POINTER_ID,
        "libinput Accel Speed",
        crate::xinput::XI_PROP_MODE_REPLACE,
        32,
        &0.75_f32.to_le_bytes(),
    );
    route_t3_config_request(
        &mut state,
        &mut backend,
        &mut inventory,
        &mut pending,
        &mut lane,
        request_b,
    );

    let mut reset_trigger =
        crate::core_loop::reset::ResetTrigger::new(crate::core_loop::reset::ResetPolicy::NoReset);
    crate::core_loop::run::disconnect_with_pending_cleanup(
        &mut state,
        &mut backend,
        &mut pending,
        &mut lane,
        &mut reset_trigger,
        ClientId(2),
    );
    assert!(read_all_available(&mut peer_b).is_empty());
    assert_eq!(backend.started_device_configs.len(), 1);
    crate::core_loop::run::disconnect_with_pending_cleanup(
        &mut state,
        &mut backend,
        &mut pending,
        &mut lane,
        &mut reset_trigger,
        ClientId(1),
    );
    assert!(read_all_available(&mut peer_a).is_empty());
    assert!(pending.is_empty());
    assert!(
        !lane.is_empty(),
        "submitted backend work survives disconnect"
    );

    crate::core_loop::run::dispatch_device_config_result(
        &mut state,
        &mut backend,
        &mut inventory,
        &mut pending,
        &mut lane,
        &mut reset_trigger,
        crate::core_loop::message::Message::DeviceConfigResult {
            token: DeviceConfigToken(206),
            source,
            result: Ok(()),
        },
        crate::core_loop::generation::Generation::default(),
    );

    let atom = state.atoms.id_for("libinput Accel Speed").unwrap();
    assert_eq!(
        state
            .xi_devices
            .device(TEST_PHYSICAL_POINTER_ID)
            .unwrap()
            .properties[&atom]
            .data,
        0.5_f32.to_le_bytes()
    );
    assert_eq!(inventory.get(source).unwrap().config.accel.current, 0.5);
    assert_eq!(
        backend.started_device_configs,
        [(source, DeviceConfigChange::AccelSpeed(0.5))]
    );
    assert!(lane.is_empty());
    assert!(pending.is_empty());
}

#[test]
fn xi_config_completion_reset_keeps_submitted_change_and_uses_current_atoms() {
    use crate::xinput::libinput_props::{DeviceConfigChange, DeviceConfigStart, DeviceConfigToken};

    let mut state = ServerState::new();
    let mut peer_a = install_capture_client(&mut state, 1);
    let mut peer_b = install_capture_client(&mut state, 2);
    let mut backend = RecordingBackend::new();
    seed_pointer_for_t3(&mut state);
    let (mut inventory, source) = inventory_for_t3_source(&state);
    let info = inventory.get(source).unwrap().clone();
    let old_atom = state.atoms.id_for("libinput Accel Speed").unwrap();
    let mut pending = crate::core_loop::run::PendingBackendRequests::default();
    let mut lane = crate::core_loop::run::XiConfigLane::default();
    backend
        .device_config_start_results
        .push_back(Ok(DeviceConfigStart::Pending(DeviceConfigToken(207))));

    let request = parse_t3_xi2_config_request(
        &mut state,
        &mut backend,
        1,
        1,
        TEST_PHYSICAL_POINTER_ID,
        "libinput Accel Speed",
        crate::xinput::XI_PROP_MODE_REPLACE,
        32,
        &0.5_f32.to_le_bytes(),
    );
    route_t3_config_request(
        &mut state,
        &mut backend,
        &mut inventory,
        &mut pending,
        &mut lane,
        request,
    );
    let queued = parse_t3_xi2_config_request(
        &mut state,
        &mut backend,
        2,
        1,
        TEST_PHYSICAL_POINTER_ID,
        "libinput Accel Speed",
        crate::xinput::XI_PROP_MODE_REPLACE,
        32,
        &0.75_f32.to_le_bytes(),
    );
    route_t3_config_request(
        &mut state,
        &mut backend,
        &mut inventory,
        &mut pending,
        &mut lane,
        queued,
    );
    assert_eq!(backend.started_device_configs.len(), 1);
    crate::core_loop::run::cancel_unsubmitted_xi_configs(&mut lane, &mut pending);
    assert!(!lane.is_empty(), "reset retains the submitted operation");
    assert!(
        pending.is_empty(),
        "reset releases old client blocking state"
    );
    assert!(read_all_available(&mut peer_a).is_empty());
    assert!(read_all_available(&mut peer_b).is_empty());

    let mut reset_state = ServerState::new();
    let _ = reset_state.atoms.intern("reset atom allocation", false);
    reset_state.xi_register_source(&info);
    let current_atom = reset_state.atoms.id_for("libinput Accel Speed").unwrap();
    assert_ne!(
        current_atom, old_atom,
        "reset replay gives the descriptor a fresh atom id"
    );
    let generation = crate::core_loop::generation::GenerationCounter::new();
    let current_generation = generation.bump();
    crate::core_loop::run::dispatch_device_config_result(
        &mut reset_state,
        &mut backend,
        &mut inventory,
        &mut pending,
        &mut lane,
        &mut crate::core_loop::reset::ResetTrigger::new(
            crate::core_loop::reset::ResetPolicy::NoReset,
        ),
        crate::core_loop::message::Message::DeviceConfigResult {
            token: DeviceConfigToken(207),
            source,
            result: Ok(()),
        },
        current_generation,
    );

    let device = reset_state
        .xi_devices
        .device(TEST_PHYSICAL_POINTER_ID)
        .expect("replayed source has its pointer facet");
    assert_eq!(device.properties[&current_atom].data, 0.5_f32.to_le_bytes());
    assert_eq!(device.source_id, Some(source));
    assert_eq!(
        reset_state
            .xi_devices
            .source(source)
            .unwrap()
            .config
            .accel
            .current,
        0.5
    );
    assert_eq!(inventory.get(source).unwrap().config.accel.current, 0.5);
    assert_eq!(
        backend.started_device_configs,
        [(source, DeviceConfigChange::AccelSpeed(0.5))]
    );
    assert!(lane.is_empty());
    assert!(pending.is_empty());
}

#[test]
fn xi_config_completion_recreates_get_deleted_property_after_success() {
    use crate::xinput::libinput_props::{DeviceConfigStart, DeviceConfigToken};

    let mut state = ServerState::new();
    let mut peer = install_capture_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    seed_pointer_for_t3(&mut state);
    let (mut inventory, source) = inventory_for_t3_source(&state);
    let mut pending = crate::core_loop::run::PendingBackendRequests::default();
    let mut lane = crate::core_loop::run::XiConfigLane::default();
    let property = state.atoms.intern("libinput Accel Profile Enabled", false);
    select_xi2_property_event_on_root(&mut state, 1, TEST_PHYSICAL_POINTER_ID);

    let mut delete_body = TEST_PHYSICAL_POINTER_ID.to_le_bytes().to_vec();
    delete_body.extend_from_slice(&[1, 0]);
    delete_body.extend_from_slice(&property.0.to_le_bytes());
    delete_body.extend_from_slice(&0u32.to_le_bytes()); // type = AnyPropertyType
    delete_body.extend_from_slice(&0u32.to_le_bytes()); // offset
    delete_body.extend_from_slice(&3u32.to_le_bytes()); // full three-byte read
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        xi2_header(59),
        &delete_body,
    )
    .expect("GetProperty(delete)");
    let deletion_wire = read_all_available(&mut peer);
    assert!(find_xi2_property_event(&deletion_wire).is_some());
    assert!(
        !state
            .xi_devices
            .device(TEST_PHYSICAL_POINTER_ID)
            .unwrap()
            .properties
            .contains_key(&property),
        "GetProperty(delete) removes the seeded property"
    );

    backend
        .device_config_start_results
        .push_back(Ok(DeviceConfigStart::Pending(DeviceConfigToken(208))));
    let request = parse_t3_xi2_config_request(
        &mut state,
        &mut backend,
        1,
        2,
        TEST_PHYSICAL_POINTER_ID,
        "libinput Accel Profile Enabled",
        crate::xinput::XI_PROP_MODE_REPLACE,
        8,
        &[0, 1],
    );
    assert!(backend.started_device_configs.is_empty());
    route_t3_config_request(
        &mut state,
        &mut backend,
        &mut inventory,
        &mut pending,
        &mut lane,
        request,
    );
    assert!(
        !state
            .xi_devices
            .device(TEST_PHYSICAL_POINTER_ID)
            .unwrap()
            .properties
            .contains_key(&property),
        "a submitted write does not recreate the property before success"
    );
    assert!(
        read_all_available(&mut peer).is_empty(),
        "no Created event before success"
    );

    crate::core_loop::run::dispatch_device_config_result(
        &mut state,
        &mut backend,
        &mut inventory,
        &mut pending,
        &mut lane,
        &mut crate::core_loop::reset::ResetTrigger::new(
            crate::core_loop::reset::ResetPolicy::NoReset,
        ),
        crate::core_loop::message::Message::DeviceConfigResult {
            token: DeviceConfigToken(208),
            source,
            result: Ok(()),
        },
        crate::core_loop::generation::Generation::default(),
    );
    let wire = read_all_available(&mut peer);
    assert_eq!(
        find_xi2_property_event(&wire).unwrap()[20],
        crate::xinput::PropWhat::Created as u8
    );
    let recreated = &state
        .xi_devices
        .device(TEST_PHYSICAL_POINTER_ID)
        .unwrap()
        .properties[&property];
    assert_eq!(recreated.data, [0, 1, 0]);
    assert!(
        recreated.deletable,
        "recreated Xorg property uses default deletable flag"
    );
    assert_eq!(
        inventory.get(source).unwrap().config.accel_profile.current,
        Some(1)
    );
    assert!(lane.is_empty());
    assert!(pending.is_empty());
}

#[test]
fn xi_config_completion_suspended_source_rejects_without_blocking_other_source() {
    use crate::xinput::libinput_props::{DeviceConfigChange, DeviceConfigStart, DeviceConfigToken};

    let mut state = ServerState::new();
    let mut peer_suspended = install_capture_client(&mut state, 1);
    let mut peer_live = install_capture_client(&mut state, 2);
    let mut backend = RecordingBackend::new();
    seed_pointer_for_t3(&mut state);
    let source_suspended = state
        .xi_devices
        .device(TEST_PHYSICAL_POINTER_ID)
        .unwrap()
        .source_id
        .unwrap();
    let mut suspended_info = state.xi_devices.source(source_suspended).unwrap().clone();
    suspended_info.enabled = false;
    state.xi_devices.register(&suspended_info);
    let mut live_info = suspended_info.clone();
    live_info.source_id = crate::xinput::InputSourceId(source_suspended.0 + 1);
    live_info.enabled = true;
    live_info.device_node = "/dev/input/event77".into();
    live_info.sysname = "event77".into();
    let live_ids = state.xi_register_source(&live_info);
    let live_device = *live_ids
        .iter()
        .find(|id| *id != &TEST_PHYSICAL_POINTER_ID)
        .expect("second pointer facet");
    let source_live = live_info.source_id;
    let mut inventory = crate::core_loop::input_inventory::InputInventory::new();
    inventory.add(suspended_info);
    inventory.add(live_info);
    inventory.suspend(source_suspended);
    let mut pending = crate::core_loop::run::PendingBackendRequests::default();
    let mut lane = crate::core_loop::run::XiConfigLane::default();
    backend
        .device_config_start_results
        .push_back(Ok(DeviceConfigStart::Pending(DeviceConfigToken(206))));

    // Client B's enabled-source write occupies the global lane while its
    // backend completion remains pending.
    let request_live = parse_t3_xi2_config_request(
        &mut state,
        &mut backend,
        2,
        1,
        live_device,
        "libinput Accel Speed",
        crate::xinput::XI_PROP_MODE_REPLACE,
        32,
        &0.75_f32.to_le_bytes(),
    );
    route_t3_config_request(
        &mut state,
        &mut backend,
        &mut inventory,
        &mut pending,
        &mut lane,
        request_live,
    );
    assert_eq!(backend.started_device_configs.len(), 1);
    assert!(!lane.is_empty(), "client B's write is still in flight");
    assert!(pending.xi_config_client_is_blocked_for_test(yserver_protocol::x11::ClientId(2)));

    // Client A's write targets the suspended source and must fail before
    // it can wait behind B's in-flight write.
    let request_suspended = parse_t3_xi2_config_request(
        &mut state,
        &mut backend,
        1,
        2,
        TEST_PHYSICAL_POINTER_ID,
        "libinput Accel Speed",
        crate::xinput::XI_PROP_MODE_REPLACE,
        32,
        &0.25_f32.to_le_bytes(),
    );
    route_t3_config_request(
        &mut state,
        &mut backend,
        &mut inventory,
        &mut pending,
        &mut lane,
        request_suspended,
    );

    let rejected = read_all_available(&mut peer_suspended);
    assert_xi_config_error(&rejected, x11::error::BAD_MATCH, 2, 57);
    assert!(
        !pending.xi_config_client_is_blocked_for_test(yserver_protocol::x11::ClientId(1)),
        "client A is rejected without joining the pending lane"
    );
    assert!(pending.xi_config_client_is_blocked_for_test(yserver_protocol::x11::ClientId(2)));
    assert!(!lane.is_empty(), "client B remains in flight");
    assert_eq!(backend.started_device_configs.len(), 1);
    assert!(read_all_available(&mut peer_live).is_empty());

    let accel_atom = state.atoms.id_for("libinput Accel Speed").unwrap();
    assert_eq!(
        state
            .xi_devices
            .device(TEST_PHYSICAL_POINTER_ID)
            .unwrap()
            .properties[&accel_atom]
            .data,
        0.0_f32.to_le_bytes(),
        "the rejected suspended-source write leaves its property unchanged"
    );
    assert_eq!(
        inventory
            .get(source_suspended)
            .unwrap()
            .config
            .accel
            .current,
        0.0
    );

    crate::core_loop::run::dispatch_device_config_result(
        &mut state,
        &mut backend,
        &mut inventory,
        &mut pending,
        &mut lane,
        &mut crate::core_loop::reset::ResetTrigger::new(
            crate::core_loop::reset::ResetPolicy::NoReset,
        ),
        crate::core_loop::message::Message::DeviceConfigResult {
            token: DeviceConfigToken(206),
            source: source_live,
            result: Ok(()),
        },
        crate::core_loop::generation::Generation::default(),
    );

    let live_atom = state.atoms.id_for("libinput Accel Speed").unwrap();
    assert_eq!(
        backend.started_device_configs,
        [(source_live, DeviceConfigChange::AccelSpeed(0.75))]
    );
    assert_eq!(
        state.xi_devices.device(live_device).unwrap().properties[&live_atom].data,
        0.75_f32.to_le_bytes()
    );
    assert_eq!(
        inventory.get(source_live).unwrap().config.accel.current,
        0.75
    );
    assert!(lane.is_empty());
    assert!(pending.is_empty());
    assert!(read_all_available(&mut peer_live).is_empty());
    assert!(read_all_available(&mut peer_suspended).is_empty());
}

#[test]
fn xi_libinput_write_uses_enabled_sibling_facets() {
    use crate::xinput::libinput_props::DeviceConfigChange;

    // Mutation killed: restore the per-facet `!device.enabled` BadMatch
    // gate in the XI config path. The pointer write below must reach
    // libinput while its keyboard sibling remains enabled.
    // Xorg's shared handle stays open until every facet is disabled
    // (xf86libinput.c:416-432), and property checks fail only when that
    // shared handle is null (xf86libinput.c:4392-4410).
    let mut state = ServerState::new();
    let mut peer = install_capture_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let baseline_device_ids: Vec<_> = state
        .xi_devices
        .devices()
        .iter()
        .map(|device| device.id)
        .collect();
    let baseline_source_ids = state.xi_devices.source_ids();
    let baseline_properties = xi_property_snapshot(&state);
    let baseline_pointer_buttons = state.buttons_down;
    let baseline_device_buttons: Vec<_> = state
        .xi_devices
        .devices()
        .iter()
        .map(|device| (device.id, device.buttons_down))
        .collect();
    let baseline_key_holds = state.key_down_by_device.clone();
    let baseline_detached_masters = state.xi2_detached_masters.clone();
    let baseline_floating_positions = state.floating_pointer_positions.clone();
    let baseline_selections = {
        let client = &state.clients[&1];
        (
            client.xi2_masks.clone(),
            client.xi1_event_classes.clone(),
            client.xi1_window_event_classes.clone(),
        )
    };
    let core_xi2_header = |minor, body: &[u8]| {
        let mut header = xi2_header_for_body(minor, body);
        header.opcode = 137;
        header
    };

    let source = crate::xinput::InputSourceId(u64::from(line!()));
    let info = crate::core_loop::DeviceInfo {
        source_id: source,
        enabled: true,
        resume_key: None,
        capabilities: crate::xinput::InputCapabilities {
            keyboard: true,
            pointer: true,
            touch: false,
        },
        name: "Mixed keyboard and pointer".into(),
        device_node: "/dev/input/event88".into(),
        sysname: "event88".into(),
        vendor_id: 0x046d,
        product_id: 0xc52f,
        is_touchpad: false,
        config: crate::core_loop::message::LibinputConfigSnapshot {
            accel: crate::core_loop::message::FloatSetting {
                available: true,
                current: 0.0,
                default: 0.0,
            },
            ..Default::default()
        },
    };
    let ids = state.xi_register_source(&info);
    let pointer = state
        .xi_devices
        .facet(source, crate::xinput::XiFacetKind::PointerTouch)
        .expect("mixed source pointer facet");
    let keyboard = state
        .xi_devices
        .facet(source, crate::xinput::XiFacetKind::Keyboard)
        .expect("mixed source keyboard facet");
    let mut inventory = crate::core_loop::input_inventory::InputInventory::new();
    inventory.add(info);
    let mut pending = crate::core_loop::run::PendingBackendRequests::default();
    let mut lane = crate::core_loop::run::XiConfigLane::default();

    // Match the post-EnableDevice property value before exercising the
    // client disable transition (the fixture registered the source).
    let enabled_atom = state.xi_device_enabled_atom.0;
    let enabled_write = xi2_change_property_body(
        pointer,
        crate::xinput::XI_PROP_MODE_REPLACE,
        8,
        enabled_atom,
        crate::xinput::XA_INTEGER.0,
        &[1],
    );
    assert!(matches!(
        process_request(
            &mut state,
            &mut backend,
            ClientId(1),
            SequenceNumber(1),
            core_xi2_header(57, &enabled_write),
            &enabled_write,
            None,
        )
        .unwrap(),
        RequestOutcome::Handled
    ));

    let disable_pointer = xi2_change_property_body(
        pointer,
        crate::xinput::XI_PROP_MODE_REPLACE,
        8,
        enabled_atom,
        crate::xinput::XA_INTEGER.0,
        &[0],
    );
    assert!(matches!(
        process_request(
            &mut state,
            &mut backend,
            ClientId(1),
            SequenceNumber(2),
            core_xi2_header(57, &disable_pointer),
            &disable_pointer,
            None,
        )
        .unwrap(),
        RequestOutcome::Handled
    ));
    assert!(state.xi_devices.device(pointer).unwrap().client_disabled);
    assert!(!state.xi_devices.device(pointer).unwrap().enabled);
    assert!(state.xi_devices.device(keyboard).unwrap().enabled);

    let accel_atom = state.atoms.id_for("libinput Accel Speed").unwrap();
    let accel_body = xi2_change_property_body(
        pointer,
        crate::xinput::XI_PROP_MODE_REPLACE,
        32,
        accel_atom.0,
        state.float_atom.0,
        &0.75_f32.to_le_bytes(),
    );
    let outcome = process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(3),
        core_xi2_header(57, &accel_body),
        &accel_body,
        None,
    )
    .unwrap();
    let RequestOutcome::PendingXiConfig(request) = outcome else {
        panic!("recognized libinput write must enter the config lane: {outcome:?}");
    };
    route_t3_config_request(
        &mut state,
        &mut backend,
        &mut inventory,
        &mut pending,
        &mut lane,
        request,
    );
    assert_eq!(
        backend.started_device_configs,
        [(source, DeviceConfigChange::AccelSpeed(0.75))],
        "a disabled pointer facet can configure the still-open shared source"
    );
    assert_eq!(
        inventory.get(source).unwrap().config.accel.current,
        0.75,
        "the applied setting reaches the source inventory"
    );
    assert_eq!(
        f32::from_le_bytes(
            state.xi_devices.device(pointer).unwrap().properties[&accel_atom].data[..4]
                .try_into()
                .unwrap()
        ),
        0.75,
        "the confirmed value is committed to the disabled facet property"
    );

    let disable_keyboard = xi2_change_property_body(
        keyboard,
        crate::xinput::XI_PROP_MODE_REPLACE,
        8,
        enabled_atom,
        crate::xinput::XA_INTEGER.0,
        &[0],
    );
    assert!(matches!(
        process_request(
            &mut state,
            &mut backend,
            ClientId(1),
            SequenceNumber(4),
            core_xi2_header(57, &disable_keyboard),
            &disable_keyboard,
            None,
        )
        .unwrap(),
        RequestOutcome::Handled
    ));
    assert!(
        state
            .xi_devices
            .devices()
            .iter()
            .filter(|device| device.source_id == Some(source))
            .all(|device| !device.enabled)
    );

    let accel_body = xi2_change_property_body(
        pointer,
        crate::xinput::XI_PROP_MODE_REPLACE,
        32,
        accel_atom.0,
        state.float_atom.0,
        &0.25_f32.to_le_bytes(),
    );
    let outcome = process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(5),
        core_xi2_header(57, &accel_body),
        &accel_body,
        None,
    )
    .unwrap();
    let RequestOutcome::PendingXiConfig(request) = outcome else {
        panic!("recognized libinput write must enter the config lane: {outcome:?}");
    };
    route_t3_config_request(
        &mut state,
        &mut backend,
        &mut inventory,
        &mut pending,
        &mut lane,
        request,
    );
    assert_xi_config_error(&read_all_available(&mut peer), x11::error::BAD_MATCH, 5, 57);
    assert_eq!(
        backend.started_device_configs.len(),
        1,
        "no config reaches the backend once every facet is disabled"
    );
    assert_eq!(
        inventory.get(source).unwrap().config.accel.current,
        0.75,
        "a rejected write does not alter source settings"
    );
    assert!(lane.is_empty());
    assert!(pending.is_empty());

    let removed = state.xi_unregister_source(source);
    assert_eq!(removed, ids);
    assert_eq!(
        state.take_xi_removed_device_descriptors().len(),
        ids.len(),
        "the source removal publisher owns every removed descriptor"
    );
    inventory.remove(source);
    assert!(inventory.is_empty());
    assert_eq!(
        state
            .xi_devices
            .devices()
            .iter()
            .map(|device| device.id)
            .collect::<Vec<_>>(),
        baseline_device_ids,
        "registry returns to its original device set"
    );
    assert_eq!(state.xi_devices.source_ids(), baseline_source_ids);
    assert!(state.xi_devices.source(source).is_none());
    assert_eq!(xi_property_snapshot(&state), baseline_properties);
    assert_eq!(state.buttons_down, baseline_pointer_buttons);
    assert_eq!(
        state
            .xi_devices
            .devices()
            .iter()
            .map(|device| (device.id, device.buttons_down))
            .collect::<Vec<_>>(),
        baseline_device_buttons
    );
    assert_eq!(state.key_down_by_device, baseline_key_holds);
    assert_eq!(state.xi2_detached_masters, baseline_detached_masters);
    assert_eq!(
        state.floating_pointer_positions,
        baseline_floating_positions
    );
    let client = &state.clients[&1];
    assert_eq!(
        (
            &client.xi2_masks,
            &client.xi1_event_classes,
            &client.xi1_window_event_classes,
        ),
        (
            &baseline_selections.0,
            &baseline_selections.1,
            &baseline_selections.2,
        )
    );
    assert!(read_all_available(&mut peer).is_empty());
}

#[test]
fn xi_config_completion_removal_cancels_queued_write_on_host_input_for_xi1_and_xi2() {
    for xi1_request in [true, false] {
        assert_t3_host_lifecycle_cancels_queued_write(true, xi1_request);
    }
}

#[test]
fn xi_config_completion_suspension_cancels_queued_write_on_host_input_for_xi1_and_xi2() {
    for xi1_request in [true, false] {
        assert_t3_host_lifecycle_cancels_queued_write(false, xi1_request);
    }
}

#[test]
fn xi_config_completion_t3_b2_append_onto_absent_profile_behaves_like_replace() {
    // Model a client-deleted supported property. Append [0, 1, 0]
    // then behaves exactly like Replace and stores three bytes.
    let mut state = ServerState::new();
    let mut peer = install_capture_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    seed_pointer_for_t3(&mut state);
    let (mut inventory, source) = inventory_for_t3_source(&state);
    let mut pending = crate::core_loop::run::PendingBackendRequests::default();
    let mut lane = crate::core_loop::run::XiConfigLane::default();
    let atom = state
        .atoms
        .intern("libinput Accel Profile Enabled", false)
        .0;

    // Delete through the same XIGetProperty(delete) path a client uses.
    let mut delete_body = TEST_PHYSICAL_POINTER_ID.to_le_bytes().to_vec();
    delete_body.extend_from_slice(&[1, 0]);
    delete_body.extend_from_slice(&atom.to_le_bytes());
    delete_body.extend_from_slice(&0u32.to_le_bytes());
    delete_body.extend_from_slice(&0u32.to_le_bytes());
    delete_body.extend_from_slice(&3u32.to_le_bytes());
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        xi2_header(59),
        &delete_body,
    )
    .unwrap();
    let deleted_reply = read_all_available(&mut peer);
    assert_eq!(
        deleted_reply.len(),
        36,
        "GetProperty reply includes the full value"
    );
    assert_eq!(deleted_reply[0], 1, "GetProperty returns a reply");
    assert!(
        !state
            .xi_devices
            .device(TEST_PHYSICAL_POINTER_ID)
            .unwrap()
            .properties
            .contains_key(&AtomId(atom))
    );
    let integer_atom = crate::xinput::XA_INTEGER.0;

    let body = xi2_change_property_body(
        TEST_PHYSICAL_POINTER_ID,
        crate::xinput::XI_PROP_MODE_APPEND,
        8,
        atom,
        integer_atom,
        &[0, 1, 0],
    );
    let outcome = handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        xi2_header(57),
        &body,
    )
    .unwrap();
    let crate::core_loop::process_request::RequestOutcome::PendingXiConfig(request) = outcome
    else {
        panic!("recognized Append must enter the core config lane")
    };
    assert!(backend.started_device_configs.is_empty());
    assert!(
        !state
            .xi_devices
            .device(TEST_PHYSICAL_POINTER_ID)
            .unwrap()
            .properties
            .contains_key(&AtomId(atom)),
        "request parsing leaves the deleted property absent"
    );
    route_t3_config_request(
        &mut state,
        &mut backend,
        &mut inventory,
        &mut pending,
        &mut lane,
        request,
    );
    assert!(
        read_all_available(&mut peer).is_empty(),
        "no error: absent property + Append behaves like Replace"
    );

    let dev = state
        .xi_devices
        .iter()
        .find(|d| d.id == TEST_PHYSICAL_POINTER_ID)
        .unwrap();
    let prop = dev.properties.get(&AtomId(atom)).unwrap();
    assert_eq!(prop.data, vec![0, 1, 0]);
    assert!(
        prop.deletable,
        "recreated property uses Xorg's default flag"
    );
    assert_eq!(
        backend.started_device_configs,
        [(
            source,
            crate::xinput::libinput_props::DeviceConfigChange::AccelProfile(Some(1))
        )]
    );
    assert_eq!(
        inventory.get(source).unwrap().config.accel_profile.current,
        Some(1)
    );
    assert!(lane.is_empty());
    assert!(pending.is_empty());
}

#[test]
fn xi_config_completion_t5_xi2_modified_property_event_to_selecting_client() {
    // Selecting client receives an XI2 XI_PropertyEvent with
    // what=Modified after a successful XIChangeProperty against an
    // already-seeded libinput property ("libinput Tapping Enabled"
    // is set by `seed_pointer_properties`, so the write is a Modify).
    let mut state = ServerState::new();
    let mut peer = install_capture_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    seed_pointer_for_t3(&mut state);
    let (mut inventory, source) = inventory_for_t3_source(&state);
    let mut pending = crate::core_loop::run::PendingBackendRequests::default();
    let mut lane = crate::core_loop::run::XiConfigLane::default();
    select_xi2_property_event_on_root(&mut state, 1, TEST_PHYSICAL_POINTER_ID);
    let tap_atom = state
        .atoms
        .intern(crate::xinput::PROP_TAPPING_ENABLED, false)
        .0;
    let integer_atom = crate::xinput::XA_INTEGER.0;

    let body = xi2_change_property_body(
        TEST_PHYSICAL_POINTER_ID,
        crate::xinput::XI_PROP_MODE_REPLACE,
        8,
        tap_atom,
        integer_atom,
        &[0],
    );
    drive_t3_config_wire_request(
        &mut state,
        &mut peer,
        &mut backend,
        &mut inventory,
        &mut pending,
        &mut lane,
        1,
        1,
        xi2_header(57),
        &body,
    );
    let wire = read_all_available(&mut peer);
    // No reply for XIChangeProperty; the only bytes on the wire
    // must be the 32-byte XI_PropertyEvent.
    assert_eq!(wire.len(), 32, "exactly one 32-byte event, no reply");
    let ev = find_xi2_property_event(&wire).expect("XI_PropertyEvent present");
    assert_eq!(ev[0], 35, "GenericEvent");
    assert_eq!(ev[1], 137, "extension = XInputExtension major");
    assert_eq!(u16::from_le_bytes([ev[8], ev[9]]), 12, "evtype");
    assert_eq!(
        u16::from_le_bytes([ev[10], ev[11]]),
        TEST_PHYSICAL_POINTER_ID,
        "deviceid"
    );
    assert_eq!(
        u32::from_le_bytes([ev[16], ev[17], ev[18], ev[19]]),
        tap_atom,
        "property atom"
    );
    assert_eq!(ev[20], 2, "what = Modified (property already seeded)");
    assert_confirmed_tap_write(&state, &inventory, source, AtomId(tap_atom));
    assert_eq!(backend.started_device_configs.len(), 1);
    assert!(lane.is_empty());
    assert!(pending.is_empty());
}

#[test]
fn t5_xi2_change_property_emits_property_event_created_for_new_atom() {
    // Writing a property the device has never carried before
    // (atom 0x4242 — a synthetic test atom, no descriptor table
    // entry, so the dispatch falls through to the bare commit step)
    // reports `Created`.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    // No touchpad → the descriptor path is bypassed and any atom
    // commits straight into the registry.
    state.atoms.register_for_test(AtomId(0x4242), "T5 created");
    select_xi2_property_event_on_root(&mut state, 1, 4);

    let body = xi2_change_property_body(
        4,
        crate::xinput::XI_PROP_MODE_REPLACE,
        8,
        0x4242,
        crate::xinput::XA_INTEGER.0,
        &[7],
    );
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        xi2_header(57),
        &body,
    )
    .unwrap();
    let wire = read_all_available(&mut peer);
    let ev = find_xi2_property_event(&wire).expect("XI_PropertyEvent present");
    assert_eq!(ev[20], 1, "what = Created");
    assert_eq!(
        u32::from_le_bytes([ev[16], ev[17], ev[18], ev[19]]),
        0x4242,
        "property atom"
    );
}

#[test]
fn t5_xi2_delete_property_emits_property_event_deleted() {
    // XIDeleteProperty against a seeded property fans out
    // what=Deleted.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    seed_one_prop(&mut state, 100, 8, vec![1]);
    select_xi2_property_event_on_root(&mut state, 1, 4);

    let mut body = Vec::new();
    body.extend_from_slice(&4u16.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes());
    body.extend_from_slice(&100u32.to_le_bytes());
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        xi2_header(58),
        &body,
    )
    .unwrap();
    let wire = read_all_available(&mut peer);
    assert_eq!(wire.len(), 32, "exactly one 32-byte event, no reply");
    let ev = find_xi2_property_event(&wire).expect("XI_PropertyEvent present");
    assert_eq!(ev[20], 0, "what = Deleted");
    assert_eq!(
        u32::from_le_bytes([ev[16], ev[17], ev[18], ev[19]]),
        100,
        "property atom"
    );
}

#[test]
fn t5_xi2_delete_property_absent_is_silent() {
    // Redundant delete of an absent property must NOT emit an
    // event (mirror of xiproperty.c, which only calls
    // `send_property_event` when the registry actually shrank).
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    // Atom must exist for the BadAtom guard, but the property is
    // not seeded onto device 4.
    state.atoms.register_for_test(AtomId(100), "T5 absent prop");
    select_xi2_property_event_on_root(&mut state, 1, 4);

    let mut body = Vec::new();
    body.extend_from_slice(&4u16.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes());
    body.extend_from_slice(&100u32.to_le_bytes());
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        xi2_header(58),
        &body,
    )
    .unwrap();
    assert!(
        read_all_available(&mut peer).is_empty(),
        "no event for a no-op delete"
    );
}

#[test]
fn t5_xi2_get_property_delete_emits_property_event_deleted() {
    // `XIGetProperty(delete=1)` removes the property after a full
    // read and the dispatch arm fans out what=Deleted to selecting
    // clients. Both the GetProperty reply (36 bytes: 32 header +
    // 1 data byte padded to 4) and the 32-byte XI_PropertyEvent
    // end up on the same socket — the X protocol leaves the order
    // unspecified (clients reorder by sequence), so the test
    // searches for the event payload rather than anchoring on its
    // wire position.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    seed_one_prop(&mut state, 100, 8, vec![1]);
    select_xi2_property_event_on_root(&mut state, 1, 4);

    // body: deviceid(2)=4, delete(1)=1, pad(1), property(4)=100,
    // type(4)=0, offset(4)=0, len(4)=100.
    let mut body = Vec::new();
    body.extend_from_slice(&4u16.to_le_bytes());
    body.push(1); // delete = true
    body.push(0); // pad
    body.extend_from_slice(&100u32.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&100u32.to_le_bytes());
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        xi2_header(59),
        &body,
    )
    .unwrap();
    let wire = read_all_available(&mut peer);
    assert_eq!(wire.len(), 36 + 32, "GetProperty reply + PropertyEvent");
    let ev = find_xi2_property_event(&wire).expect("XI_PropertyEvent present");
    assert_eq!(ev[20], 0, "what = Deleted");
    // Independently confirm the reply is somewhere in the buffer.
    // The reply starts with X_Reply = 1, which the 32-byte event
    // (starts with GenericEvent = 35) can't impersonate.
    assert!(wire.contains(&1), "GetProperty reply is on the wire");
    // The registry must have actually shrunk.
    let dev = state.xi_devices.iter().find(|d| d.id == 4).unwrap();
    assert!(
        !dev.properties.contains_key(&AtomId(100)),
        "delete=1 consumed the property"
    );
}

#[test]
fn t5_xi2_get_property_no_delete_does_not_emit() {
    // The non-deleting read path must not emit anything.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    seed_one_prop(&mut state, 100, 8, vec![1]);
    select_xi2_property_event_on_root(&mut state, 1, 4);

    let mut body = Vec::new();
    body.extend_from_slice(&4u16.to_le_bytes());
    body.push(0); // delete = false
    body.push(0); // pad
    body.extend_from_slice(&100u32.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&100u32.to_le_bytes());
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        xi2_header(59),
        &body,
    )
    .unwrap();
    let wire = read_all_available(&mut peer);
    assert!(
        find_xi2_property_event(&wire).is_none(),
        "no XI_PropertyEvent on a non-deleting read"
    );
}

#[test]
fn xi_config_completion_t5_xi2_not_selected_client_gets_nothing() {
    // A client that did NOT select `XI_PropertyEventMask` for
    // this physical device must not receive the event, even if it has other
    // XI2 selections on the same window.
    let mut state = ServerState::new();
    let mut peer = install_capture_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    seed_pointer_for_t3(&mut state);
    let (mut inventory, source) = inventory_for_t3_source(&state);
    let mut pending = crate::core_loop::run::PendingBackendRequests::default();
    let mut lane = crate::core_loop::run::XiConfigLane::default();
    // Selecting `XI_DeviceChanged` only — the
    // `XI2_PROPERTY_EVENT_MASK` bit is NOT set, so this client
    // must be skipped.
    state.clients.get_mut(&1).unwrap().xi2_masks.insert(
        (ROOT_WINDOW, TEST_PHYSICAL_POINTER_ID),
        u64::from(crate::xinput::XI2_DEVICE_CHANGED_MASK),
    );
    let tap_atom = state
        .atoms
        .intern(crate::xinput::PROP_TAPPING_ENABLED, false)
        .0;
    let integer_atom = crate::xinput::XA_INTEGER.0;

    let body = xi2_change_property_body(
        TEST_PHYSICAL_POINTER_ID,
        crate::xinput::XI_PROP_MODE_REPLACE,
        8,
        tap_atom,
        integer_atom,
        &[0],
    );
    drive_t3_config_wire_request(
        &mut state,
        &mut peer,
        &mut backend,
        &mut inventory,
        &mut pending,
        &mut lane,
        1,
        1,
        xi2_header(57),
        &body,
    );
    assert!(
        read_all_available(&mut peer).is_empty(),
        "client without the property-event bit gets nothing"
    );
    assert_confirmed_tap_write(&state, &inventory, source, AtomId(tap_atom));
    assert_eq!(backend.started_device_configs.len(), 1);
    assert!(lane.is_empty());
    assert!(pending.is_empty());
}

#[test]
fn xi_config_completion_t5_xi2_other_device_does_not_match() {
    // Selecting `XI_PropertyEvent` for device 3 (master keyboard)
    // must NOT route a physical pointer property change to that client.
    // The wildcards `XIAllDevices(0)` / `XIAllMasterDevices(1)` are
    // followed by `xi2mask_isset` (xserver dix/inpututils.c:1153),
    // but device 3 is neither, so this client stays unselected.
    let mut state = ServerState::new();
    let mut peer = install_capture_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    seed_pointer_for_t3(&mut state);
    let (mut inventory, source) = inventory_for_t3_source(&state);
    let mut pending = crate::core_loop::run::PendingBackendRequests::default();
    let mut lane = crate::core_loop::run::XiConfigLane::default();
    // Client subscribes to property events on device 3, not the physical pointer.
    state
        .clients
        .get_mut(&1)
        .unwrap()
        .xi2_masks
        .insert((ROOT_WINDOW, 3), u64::from(XI2_PROPERTY_EVENT_MASK));
    let tap_atom = state
        .atoms
        .intern(crate::xinput::PROP_TAPPING_ENABLED, false)
        .0;
    let integer_atom = crate::xinput::XA_INTEGER.0;

    let body = xi2_change_property_body(
        TEST_PHYSICAL_POINTER_ID,
        crate::xinput::XI_PROP_MODE_REPLACE,
        8,
        tap_atom,
        integer_atom,
        &[0],
    );
    drive_t3_config_wire_request(
        &mut state,
        &mut peer,
        &mut backend,
        &mut inventory,
        &mut pending,
        &mut lane,
        1,
        1,
        xi2_header(57),
        &body,
    );
    assert!(
        read_all_available(&mut peer).is_empty(),
        "device-3 selection must not see physical pointer events"
    );
    assert_confirmed_tap_write(&state, &inventory, source, AtomId(tap_atom));
    assert_eq!(backend.started_device_configs.len(), 1);
    assert!(lane.is_empty());
    assert!(pending.is_empty());
}

#[test]
fn xi_config_completion_t5_xi2_all_devices_wildcard_matches() {
    // Regression for the bug `xinput watch-props 4` exposed on
    // M1/Asahi: every realworld client (xinput, MATE, etc.) calls
    // XISelectEvents with `deviceid = XIAllDevices = 0`, not the
    // specific slave id. xserver's `xi2mask_isset`
    // (dix/inpututils.c:1153) ORs the wildcard mask with the
    // specific mask; without that OR the client silently received
    // nothing.
    let mut state = ServerState::new();
    let mut peer = install_capture_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    seed_pointer_for_t3(&mut state);
    let (mut inventory, source) = inventory_for_t3_source(&state);
    let mut pending = crate::core_loop::run::PendingBackendRequests::default();
    let mut lane = crate::core_loop::run::XiConfigLane::default();
    // Select on the wildcard, NOT on the physical pointer directly.
    state
        .clients
        .get_mut(&1)
        .unwrap()
        .xi2_masks
        .insert((ROOT_WINDOW, 0), u64::from(XI2_PROPERTY_EVENT_MASK));
    let tap_atom = state
        .atoms
        .intern(crate::xinput::PROP_TAPPING_ENABLED, false)
        .0;
    let integer_atom = crate::xinput::XA_INTEGER.0;
    let body = xi2_change_property_body(
        TEST_PHYSICAL_POINTER_ID,
        crate::xinput::XI_PROP_MODE_REPLACE,
        8,
        tap_atom,
        integer_atom,
        &[0],
    );
    drive_t3_config_wire_request(
        &mut state,
        &mut peer,
        &mut backend,
        &mut inventory,
        &mut pending,
        &mut lane,
        1,
        1,
        xi2_header(57),
        &body,
    );
    let wire = read_all_available(&mut peer);
    let ev = find_xi2_property_event(&wire)
        .expect("XIAllDevices selection must receive the property event");
    assert_eq!(ev[20], 2, "what byte = Modified");
    assert_confirmed_tap_write(&state, &inventory, source, AtomId(tap_atom));
    assert_eq!(backend.started_device_configs.len(), 1);
    assert!(lane.is_empty());
    assert!(pending.is_empty());
}

#[test]
fn xi_config_completion_t5_xi1_write_also_emits_xi2_property_event() {
    // The XI1 write path (minor 37) must trigger the same XI2
    // fan-out — xserver's `send_property_event` fires from both
    // protocol paths so an XI2-listening client sees its tap
    // toggle even when an XI1 caller (e.g. an older xset variant)
    // wrote the property.
    let mut state = ServerState::new();
    let mut peer = install_capture_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    seed_pointer_for_t3(&mut state);
    let (mut inventory, source) = inventory_for_t3_source(&state);
    let mut pending = crate::core_loop::run::PendingBackendRequests::default();
    let mut lane = crate::core_loop::run::XiConfigLane::default();
    select_xi2_property_event_on_root(&mut state, 1, TEST_PHYSICAL_POINTER_ID);
    let tap_atom = state
        .atoms
        .intern(crate::xinput::PROP_TAPPING_ENABLED, false)
        .0;
    let integer_atom = crate::xinput::XA_INTEGER.0;

    // xChangeDevicePropertyReq body layout (XIproto.h:1467-1479):
    // property(4), type(4), deviceid(1), format(1), mode(1), pad(1),
    // num_items(4), value(num_items * format/8 padded to 4).
    let mut body = Vec::new();
    body.extend_from_slice(&tap_atom.to_le_bytes());
    body.extend_from_slice(&integer_atom.to_le_bytes());
    body.push(u8::try_from(TEST_PHYSICAL_POINTER_ID).unwrap()); // deviceid
    body.push(8); // format
    body.push(crate::xinput::XI_PROP_MODE_REPLACE);
    body.push(0); // pad
    body.extend_from_slice(&1u32.to_le_bytes()); // num_items = 1
    body.extend_from_slice(&[0u8, 0, 0, 0]); // value=[0] + 3 pad
    drive_t3_config_wire_request(
        &mut state,
        &mut peer,
        &mut backend,
        &mut inventory,
        &mut pending,
        &mut lane,
        1,
        1,
        xi2_header(37),
        &body,
    );
    let wire = read_all_available(&mut peer);
    let ev = find_xi2_property_event(&wire).expect("XI1 write must also fan-out XI_PropertyEvent");
    assert_eq!(ev[20], 2, "what = Modified");
    assert_confirmed_tap_write(&state, &inventory, source, AtomId(tap_atom));
    assert_eq!(backend.started_device_configs.len(), 1);
    assert!(lane.is_empty());
    assert!(pending.is_empty());
}

#[test]
fn xi_config_completion_t5_property_event_selected_on_any_window_still_delivers() {
    // SendEventToAllWindows: a client that selected
    // `XI_PropertyEvent` for the device on ANY window (here a
    // synthetic non-root xid) must still receive the event. The
    // event has no event-window field, so per-window distinctions
    // only affect delivery.
    let mut state = ServerState::new();
    let mut peer = install_capture_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    seed_pointer_for_t3(&mut state);
    let (mut inventory, source) = inventory_for_t3_source(&state);
    let mut pending = crate::core_loop::run::PendingBackendRequests::default();
    let mut lane = crate::core_loop::run::XiConfigLane::default();
    // Subscribe via a non-root window — anything keyed by the
    // device id wins.
    state.clients.get_mut(&1).unwrap().xi2_masks.insert(
        (ResourceId(0xdead_beef), TEST_PHYSICAL_POINTER_ID),
        u64::from(XI2_PROPERTY_EVENT_MASK),
    );
    let tap_atom = state
        .atoms
        .intern(crate::xinput::PROP_TAPPING_ENABLED, false)
        .0;
    let integer_atom = crate::xinput::XA_INTEGER.0;

    let body = xi2_change_property_body(
        TEST_PHYSICAL_POINTER_ID,
        crate::xinput::XI_PROP_MODE_REPLACE,
        8,
        tap_atom,
        integer_atom,
        &[0],
    );
    drive_t3_config_wire_request(
        &mut state,
        &mut peer,
        &mut backend,
        &mut inventory,
        &mut pending,
        &mut lane,
        1,
        1,
        xi2_header(57),
        &body,
    );
    let wire = read_all_available(&mut peer);
    assert!(
        find_xi2_property_event(&wire).is_some(),
        "non-root selection must still deliver"
    );
    assert_confirmed_tap_write(&state, &inventory, source, AtomId(tap_atom));
    assert_eq!(backend.started_device_configs.len(), 1);
    assert!(lane.is_empty());
    assert!(pending.is_empty());
}

#[test]
fn t3_xi2_get_property_uninterned_atom_yields_bad_atom() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    seed_pointer_for_t3(&mut state);
    // Atom 0xdeadbeef has never been interned → BadAtom.
    let bogus_atom = 0xdead_beefu32;
    assert!(!state.atoms.exists(AtomId(bogus_atom)), "precondition");
    // XIGetProperty body: physical deviceid(2), delete(1)=0, pad(1),
    // property(4), type(4)=0, offset(4)=0, len(4)=100.
    let mut body = Vec::new();
    body.extend_from_slice(&TEST_PHYSICAL_POINTER_ID.to_le_bytes());
    body.push(0); // delete
    body.push(0); // pad
    body.extend_from_slice(&bogus_atom.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&100u32.to_le_bytes());
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(3),
        xi2_header(59),
        &body,
    )
    .unwrap();
    let wire = read_all_available(&mut peer);
    assert_eq!(wire.len(), 32, "error packet");
    assert_eq!(wire[0], 0, "X_Error");
    assert_eq!(wire[1], 5, "BadAtom");
    // The error's bad-value field should echo the offending atom.
    assert_eq!(
        u32::from_le_bytes(wire[4..8].try_into().unwrap()),
        bogus_atom,
        "BadAtom echoes property"
    );
}

#[test]
fn xi_config_completion_invalid_value_yields_bad_value_without_mutation() {
    let mut state = ServerState::new();
    let mut peer = install_capture_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    seed_pointer_for_t3(&mut state);
    let (mut inventory, _) = inventory_for_t3_source(&state);
    let mut pending = crate::core_loop::run::PendingBackendRequests::default();
    let mut lane = crate::core_loop::run::XiConfigLane::default();
    // Scroll Method Enabled is `OneHotOrNone { n: 3 }` — two bits set is illegal.
    let scroll_atom = state
        .atoms
        .intern("libinput Scroll Method Enabled", false)
        .0;
    let integer_atom = crate::xinput::XA_INTEGER.0;
    let before = state
        .xi_devices
        .iter()
        .find(|d| d.id == TEST_PHYSICAL_POINTER_ID)
        .unwrap()
        .properties
        .get(&AtomId(scroll_atom))
        .cloned()
        .expect("scroll method seeded");
    let properties_before = xi_property_snapshot(&state);

    let body = xi2_change_property_body(
        TEST_PHYSICAL_POINTER_ID,
        crate::xinput::XI_PROP_MODE_REPLACE,
        8,
        scroll_atom,
        integer_atom,
        &[1, 1, 0],
    );
    drive_t3_config_wire_request(
        &mut state,
        &mut peer,
        &mut backend,
        &mut inventory,
        &mut pending,
        &mut lane,
        1,
        4,
        xi2_header(57),
        &body,
    );
    let wire = read_all_available(&mut peer);
    assert_xi_config_error(&wire, 2, 4, 57);

    let after = state
        .xi_devices
        .iter()
        .find(|d| d.id == TEST_PHYSICAL_POINTER_ID)
        .unwrap()
        .properties
        .get(&AtomId(scroll_atom))
        .cloned()
        .expect("scroll method still present");
    assert_eq!(
        before.data, after.data,
        "registry untouched when validation rejects the value"
    );
    assert_eq!(xi_property_snapshot(&state), properties_before);
    assert!(backend.started_device_configs.is_empty());
    assert!(lane.is_empty());
    assert!(pending.is_empty());
}
