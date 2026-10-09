use super::*;

#[test]
fn xi1_device_bell_rejects_feedback_without_bell_proc() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();

    // Device 3 advertises KbdFeedback id 0, but yserver has no audible
    // BellProc. Xorg Xi/devbell.c returns BadValue in this case.
    let body = [3, 0, 0, 50];
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(7),
        xi2_header_for_body(32, &body),
        &body,
    )
    .unwrap();
    let wire = read_all_available(&mut peer);
    assert_eq!(wire.len(), 32);
    assert_eq!(wire[0], 0, "X error");
    assert_eq!(wire[1], x11::error::BAD_VALUE);
    assert_eq!(u16::from_le_bytes(wire[2..4].try_into().unwrap()), 7);
    assert_eq!(u16::from_le_bytes(wire[8..10].try_into().unwrap()), 32);
    assert_eq!(wire[10], XI2_MAJOR_OPCODE);
}

#[test]
fn xi1_device_bell_validates_percent_before_feedback_class() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();

    // Both percent=101 and feedbackclass=99 are invalid. Xorg reports
    // the percent first and places its raw CARD8 value in errorValue.
    let body = [3, 0, 99, 101];
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(8),
        xi2_header_for_body(32, &body),
        &body,
    )
    .unwrap();
    let wire = read_all_available(&mut peer);
    assert_eq!(wire[1], x11::error::BAD_VALUE);
    assert_eq!(u32::from_le_bytes(wire[4..8].try_into().unwrap()), 101);
}

/// XI1 device-property event code is 16; with `XI_FIRST_EVENT = 66`
/// the wire type byte is 82. The XEventClass packs
/// `(deviceid << 8) | event_code`, so for the virtual XTEST pointer
/// (deviceid 4) the class is `(4 << 8) | 82 = 0x0452`.
const XI1_DEV_PROP_CLASS_XTEST_POINTER: u32 =
    ((crate::xinput::DEVICEID_XTEST_POINTER as u32) << 8) | 82;
const XI1_DEV_PROP_CLASS_XTEST_KEYBOARD: u32 =
    ((crate::xinput::DEVICEID_XTEST_KEYBOARD as u32) << 8) | 82;

#[test]
fn xi1_dynamic_hotplug_selects_the_device_256_presence_class() {
    const DEVICE_PRESENCE_CLASS: u32 = 256 << 8;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let body = xi1_select_extension_event_body(ROOT_WINDOW.0, &[DEVICE_PRESENCE_CLASS]);

    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(19),
        xi2_header_for_body(6, &body),
        &body,
    )
    .expect("device 256 is valid for XI1 presence selection");

    let wire = read_all_available(&mut peer);
    assert!(
        wire.is_empty(),
        "SelectExtensionEvent has no reply or error"
    );
    let client = state.clients.get(&1).expect("selected client");
    assert!(
        client
            .xi1_window_event_classes
            .get(&ROOT_WINDOW)
            .is_some_and(|classes| classes.contains(&DEVICE_PRESENCE_CLASS)),
        "preserve the special presence class on its selected window"
    );
    assert!(
        client.xi1_event_classes.contains(&DEVICE_PRESENCE_CLASS),
        "presence class participates in server-wide event delivery"
    );
}

#[test]
fn xi1_dynamic_hotplug_does_not_treat_an_ordinary_class_as_presence() {
    const DEVICE_PRESENCE_CLASS: u32 = 256 << 8;
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    // Device 256's low byte is `_devicePresence` (0). Other classes
    // with that special device id are stripped by Xorg but do not
    // select presence; a real device's event class is not a substitute.
    let other_device_256_class: u32 = (256 << 8) | 15;
    let ordinary_device_class: u32 = ((crate::xinput::DEVICEID_XTEST_POINTER as u32) << 8) | 81;
    let body = xi1_select_extension_event_body(
        ROOT_WINDOW.0,
        &[other_device_256_class, ordinary_device_class],
    );

    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(20),
        xi2_header_for_body(6, &body),
        &body,
    )
    .expect("unsupported ordinary classes remain ignored");

    let wire = read_all_available(&mut peer);
    assert!(
        wire.is_empty(),
        "SelectExtensionEvent has no reply or error"
    );
    let client = state.clients.get(&1).expect("selected client");
    assert!(
        !client.xi1_event_classes.contains(&DEVICE_PRESENCE_CLASS),
        "ordinary XI1 classes must not subscribe to presence"
    );
    assert!(
        client.xi1_window_event_classes.values().all(|classes| {
            !classes.contains(&DEVICE_PRESENCE_CLASS)
                && !classes.contains(&other_device_256_class)
                && !classes.contains(&ordinary_device_class)
        }),
        "only the recognized device 256 class is stored"
    );
}

/// Scan a wire buffer for the first XI1 `DevicePropertyNotify`
/// (type byte == 82) and return the 32-byte slice. T5's
/// `find_xi2_property_event` keyed off GenericEvent + evtype 12;
/// XI1 is a sequential event code so a single-byte type match is
/// enough — and it explicitly cannot collide with the XI2
/// `GenericEvent` (35) or other extension events in our range.
fn find_xi1_device_property_notify(wire: &[u8]) -> Option<&[u8]> {
    let end = wire.len().checked_sub(32)?;
    (0..=end).find_map(|i| (wire[i] == 82).then(|| &wire[i..i + 32]))
}

#[test]
fn xi1_button_shape_query_device_state_uses_core_pointer_shape() {
    // Mutation killed: hard-code the former seven-button/four-valuator
    // physical shape for the XTEST pointer instead of its CorePointer
    // class shape.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    {
        let entry = state
            .xi1_device_input_state
            .entry(crate::xinput::DEVICEID_XTEST_POINTER)
            .or_default();
        entry.buttons_down[0] = 0b0000_0010; // button 1 down
        entry.valuators = [120, 45, 0, 0]; // axes as of last motion
    }
    let before = xi_query_side_effect_snapshot(&state, 1);

    let wire = dispatch_xi_request_wire(
        &mut state,
        &mut peer,
        1,
        30,
        &[
            u8::try_from(crate::xinput::DEVICEID_XTEST_POINTER).unwrap(),
            0,
            0,
            0,
        ],
    );
    assert_eq!(
        wire.len(),
        32 + 36 + 12,
        "reply + xButtonState + xValuatorState"
    );
    assert_eq!(wire[8], 2, "num_classes");
    let bs = &wire[32..68];
    assert_eq!((bs[0], bs[1], bs[2]), (1, 36, 10), "ButtonClass/len/count");
    assert_eq!(bs[4], 0b0000_0010, "button 1 down bit");
    let vs = &wire[68..];
    assert_eq!(
        (vs[0], vs[1], vs[2], vs[3]),
        (2, 12, 2, 0),
        "ValuatorClass/len/axes/mode"
    );
    assert_eq!(i32::from_le_bytes(vs[4..8].try_into().unwrap()), 120);
    assert_eq!(i32::from_le_bytes(vs[8..12].try_into().unwrap()), 45);
    assert_eq!(xi_query_side_effect_snapshot(&state, 1), before);
}

#[test]
fn xi1_button_shape_xtest_get_mapping_has_ten_entries() {
    // Mutation killed: keep GetDeviceButtonMapping's old seven-entry
    // reply instead of reading the XTEST device's ButtonClass shape.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let before = xi_query_side_effect_snapshot(&state, 1);

    let wire = dispatch_xi_request_wire(&mut state, &mut peer, 1, 28, &[4, 0, 0, 0]);

    assert_eq!(wire[0], 1, "GetDeviceButtonMapping reply");
    assert_eq!(wire[8], 10, "nElts follows XTEST's ten-button class");
    assert_eq!(wire.len(), 32 + 12, "ten map bytes padded to three words");
    assert_eq!(&wire[32..42], &(1..=10).collect::<Vec<_>>());
    assert_eq!(&wire[42..44], &[0, 0], "reply map padding");
    assert_eq!(xi_query_side_effect_snapshot(&state, 1), before);
}

#[test]
fn xi1_button_shape_set_mapping_accepts_ten_and_eleven_entries() {
    // Mutation killed: restore a seven-entry map length ceiling, which
    // rejects Xorg's valid eleven-entry request on device 4.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let before = xi_query_side_effect_snapshot(&state, 1);

    for (sequence, map) in [(1, (1..=10).collect::<Vec<_>>()), (2, (1..=11).collect())] {
        let mut body = vec![4, u8::try_from(map.len()).unwrap(), 0, 0];
        body.extend_from_slice(&map);
        let wire = dispatch_xi_request_wire(&mut state, &mut peer, sequence, 29, &body);
        assert_eq!(
            wire.len(),
            32,
            "MappingSuccess reply for {} entries",
            map.len()
        );
        assert_eq!(wire[0], 1, "SetDeviceButtonMapping reply");
        assert_eq!(wire[8], 0, "MappingSuccess");
        assert!(read_all_available(&mut peer).is_empty());
    }

    assert_eq!(state.xi1_button_map, [(4, (1..=11).collect())].into());
    assert_eq!(xi_query_side_effect_snapshot(&state, 1), before);
}

#[test]
fn xi1_button_shape_physical_pointer_keeps_seven_buttons_and_four_valuators() {
    // Mutation killed: apply CorePointer's ten-button/two-valuator shape
    // to a physical slave whose XI class descriptors remain seven/four.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let physical_pointer = seed_pointer_for_t3(&mut state);
    let before = xi_query_side_effect_snapshot(&state, 1);

    let mapping = dispatch_xi_request_wire(
        &mut state,
        &mut peer,
        1,
        28,
        &[u8::try_from(physical_pointer).unwrap(), 0, 0, 0],
    );
    assert_eq!(mapping[8], 7, "physical button-map count");
    assert_eq!(&mapping[32..39], &(1..=7).collect::<Vec<_>>());

    let query = dispatch_xi_request_wire(
        &mut state,
        &mut peer,
        2,
        30,
        &[u8::try_from(physical_pointer).unwrap(), 0, 0, 0],
    );
    assert_eq!(query[34], 7, "physical ButtonClass reports seven buttons");
    assert_eq!(
        query[32 + 36 + 2],
        4,
        "physical ValuatorClass reports four axes"
    );
    assert_eq!(query.len(), 32 + 36 + 20);
    assert_eq!(xi_query_side_effect_snapshot(&state, 1), before);
}

#[test]
fn xi1_shape_sweep_list_input_devices_uses_each_registry_shape() {
    // Kills: restore ListInputDevices' fixed seven-button/four-axis
    // pointer descriptor for master 2 instead of its current CorePointer
    // registry shape (Xorg Xi/listdev.c:100-107, 277-283).
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let physical = state.xi_register_source(&crate::core_loop::DeviceInfo {
        source_id: crate::xinput::InputSourceId(0x51a9),
        enabled: true,
        resume_key: None,
        capabilities: crate::xinput::InputCapabilities {
            pointer: true,
            ..Default::default()
        },
        name: "shape-sweep-mouse".into(),
        device_node: "/dev/input/event-shape-sweep".into(),
        sysname: "event-shape-sweep".into(),
        vendor_id: 1,
        product_id: 2,
        is_touchpad: false,
        config: Default::default(),
    })[0];
    let before = xi_query_side_effect_snapshot(&state, 1);

    let wire = dispatch_wire_request(&mut state, &mut peer, 1, XI2_MAJOR_OPCODE, 2, &[]);
    assert_eq!(wire[0], 1, "XListInputDevices reply");
    let device_count = usize::from(wire[8]);
    let mut devices = Vec::with_capacity(device_count);
    let mut offset = 32;
    for _ in 0..device_count {
        devices.push((u16::from(wire[offset + 4]), usize::from(wire[offset + 5])));
        offset += 8;
    }
    let mut shapes = HashMap::new();
    for (id, classes) in devices {
        let mut buttons = None;
        let mut axes = None;
        for _ in 0..classes {
            let class_type = wire[offset];
            let class_len = usize::from(wire[offset + 1]);
            match class_type {
                1 => buttons = Some(u16::from_le_bytes([wire[offset + 2], wire[offset + 3]])),
                2 => axes = Some(wire[offset + 2]),
                _ => {}
            }
            offset += class_len;
        }
        shapes.insert(id, (buttons, axes));
    }
    assert_eq!(shapes.get(&2), Some(&(Some(10), Some(2))), "master 2");
    assert_eq!(shapes.get(&4), Some(&(Some(10), Some(2))), "XTEST 4");
    assert_eq!(shapes.get(&physical), Some(&(Some(7), Some(4))), "physical");
    assert_eq!(state.xi_devices.device(2).unwrap().class_sourceid, 2);
    assert_eq!(xi_query_side_effect_snapshot(&state, 1), before);
}

#[test]
fn xi1_shape_sweep_device_state_notify_uses_current_shape() {
    // Kills: report seven buttons/four axes and emit a valuator
    // continuation for XTEST 4; Xorg counts the device's current button
    // and valuator classes in FixDeviceStateNotify and
    // DeliverStateNotifyEvent (Xi/exevents.c:282-285,
    // dix/enterleave.c:715-729, 753-760).
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let baseline = xi_query_side_effect_snapshot(&state, 1);
    let child = ResourceId(0x0040_0a01);
    xi_hotplug_dispatch(
        &mut state,
        &mut backend,
        1,
        1,
        1,
        0,
        &xi_hotplug_create_window_body(child.0, ROOT_WINDOW.0),
    );
    xi_hotplug_dispatch(&mut state, &mut backend, 1, 2, 8, 0, &child.0.to_le_bytes());
    let state_class = (u32::from(crate::xinput::DEVICEID_XTEST_POINTER) << 8)
        | u32::from(crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_STATE_NOTIFY_OFFSET);
    let select = xi1_select_extension_event_body(child.0, &[state_class]);
    xi_hotplug_dispatch(&mut state, &mut backend, 1, 3, XI2_MAJOR_OPCODE, 6, &select);
    assert!(
        read_all_available(&mut peer).is_empty(),
        "selection has no reply"
    );

    let mut focus_body = child.0.to_le_bytes().to_vec();
    focus_body.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    focus_body.push(0); // RevertToNone
    focus_body.push(u8::try_from(crate::xinput::DEVICEID_XTEST_POINTER).unwrap());
    let wire = dispatch_xi_request_wire(&mut state, &mut peer, 4, 21, &focus_body);
    assert_eq!(wire.len(), 32, "DeviceStateNotify has no continuation");
    assert_eq!(
        wire[0],
        crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_STATE_NOTIFY_OFFSET
    );
    assert_eq!(wire[1], crate::xinput::DEVICEID_XTEST_POINTER as u8);
    assert_eq!(wire[9], 10, "ButtonClass count follows CorePointer");
    assert_eq!(wire[10], 2, "ValuatorClass count follows CorePointer");

    // Restore focus, then destroy the selecting window through the core
    // dispatcher so its selection is removed by the normal resource path.
    focus_body[..4].copy_from_slice(&1u32.to_le_bytes()); // PointerRoot
    let _ = dispatch_xi_request_wire(&mut state, &mut peer, 5, 21, &focus_body);
    xi_hotplug_dispatch(&mut state, &mut backend, 1, 6, 4, 0, &child.0.to_le_bytes());
    let _ = read_all_available(&mut peer);
    assert!(state.resources.window(child).is_none());
    assert_eq!(
        crate::core_loop::xi1_focus::device_focus(&state, crate::xinput::DEVICEID_XTEST_POINTER)
            .focus,
        1,
        "focus returned to PointerRoot"
    );
    assert_eq!(xi_query_side_effect_snapshot(&state, 1), baseline);
}

#[test]
fn xi1_shape_sweep_get_device_motion_events_uses_class_axis_count() {
    // Kills: keep GetDeviceMotionEvents' four-axis reply for a two-axis
    // master; Xorg copies v->numAxes into the reply (Xi/gtmotion.c:107-120).
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let physical = state.xi_register_source(&crate::core_loop::DeviceInfo {
        source_id: crate::xinput::InputSourceId(0x51aa),
        enabled: true,
        resume_key: None,
        capabilities: crate::xinput::InputCapabilities {
            pointer: true,
            ..Default::default()
        },
        name: "motion-shape-mouse".into(),
        device_node: "/dev/input/event-motion-shape".into(),
        sysname: "event-motion-shape".into(),
        vendor_id: 1,
        product_id: 3,
        is_touchpad: false,
        config: Default::default(),
    })[0];
    let before = xi_query_side_effect_snapshot(&state, 1);
    let master = dispatch_xi_request_wire(
        &mut state,
        &mut peer,
        1,
        10,
        &[0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0],
    );
    assert_eq!(master.len(), 32);
    assert_eq!(master[12], 2, "master has two valuators");
    let mut body = vec![0; 8];
    body.extend_from_slice(&u32::from(physical).to_le_bytes());
    let physical_wire = dispatch_xi_request_wire(&mut state, &mut peer, 2, 10, &body);
    assert_eq!(physical_wire.len(), 32);
    assert_eq!(
        physical_wire[12], 4,
        "physical pointer keeps four valuators"
    );
    assert_eq!(xi_query_side_effect_snapshot(&state, 1), before);
}

#[test]
fn xi1_shape_sweep_get_device_control_uses_class_axis_count() {
    // Kills: keep GetDeviceControl's fixed four-axis resolution block;
    // Xorg sizes and serializes it from v->numAxes (Xi/getdctl.c:192-197,
    // :219).
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let before = xi_query_side_effect_snapshot(&state, 1);
    let wire = dispatch_xi_request_wire(&mut state, &mut peer, 1, 34, &[1, 0, 2, 0]);
    assert_eq!(wire.len(), 64, "reply + 2-axis resolution state");
    assert_eq!(u32::from_le_bytes(wire[36..40].try_into().unwrap()), 2);
    assert_eq!(xi_query_side_effect_snapshot(&state, 1), before);
}

#[test]
fn xi1_shape_sweep_set_device_valuators_rejects_axis_after_master_shape() {
    // Kills: validate SetDeviceValuators against four global axes instead
    // of the master's two current axes; Xorg compares against
    // dev->valuator->numAxes (Xi/setdval.c:110-117).
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let before = xi_query_side_effect_snapshot(&state, 1);
    let mut body = vec![2, 2, 1, 0]; // device 2, first axis 2, one value
    body.extend_from_slice(&0i32.to_le_bytes());
    let wire = dispatch_xi_request_wire(&mut state, &mut peer, 1, 33, &body);
    assert_eq!(wire[0], 0, "X error");
    assert_eq!(wire[1], x11::error::BAD_VALUE);
    assert_eq!(xi_query_side_effect_snapshot(&state, 1), before);
}

#[test]
fn xi1_set_device_valuators_xtest_pointer_is_bad_match() {
    // Kills: accept valid-axis writes on the XTEST pointer. Xorg returns
    // BadMatch for IsXTestDevice (Xi/setdval.c:113-114), before the
    // range check (:116), so an out-of-range write is BadMatch too; the
    // master pointer (id 2) is still accepted.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut body = vec![4, 0, 1, 0]; // XTEST pointer, axis 0, one value
    body.extend_from_slice(&7i32.to_le_bytes());
    let wire = dispatch_xi_request_wire(&mut state, &mut peer, 1, 33, &body);
    assert_eq!(wire[0], 0, "X error");
    assert_eq!(wire[1], x11::error::BAD_MATCH);
    body[1] = 9; // out of range
    let wire = dispatch_xi_request_wire(&mut state, &mut peer, 1, 33, &body);
    assert_eq!(wire[1], x11::error::BAD_MATCH);
    let mut ok = vec![2, 0, 1, 0];
    ok.extend_from_slice(&7i32.to_le_bytes());
    let wire = dispatch_xi_request_wire(&mut state, &mut peer, 1, 33, &ok);
    assert_eq!(wire[0], 1, "master pointer still accepted");
}

#[test]
fn xi1_shape_sweep_change_pointer_device_matches_xorg_bad_device() {
    // Kills: keep axis-based ChangePointerDevice handling. Xorg's
    // ProcXChangePointerDevice returns BadDevice unconditionally after
    // request-size validation (Xi/chgptr.c:92-98).
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let before = xi_query_side_effect_snapshot(&state, 1);
    let wire = dispatch_xi_request_wire(&mut state, &mut peer, 1, 12, &[0, 1, 2, 0]);
    assert_eq!(wire[0], 0, "X error");
    assert_eq!(wire[1], XI1_ERROR_BAD_DEVICE);
    assert_eq!(xi_query_side_effect_snapshot(&state, 1), before);
}

#[test]
fn xi1_shape_sweep_change_device_control_rejects_axis_after_master_shape() {
    // Kills: validate ChangeDeviceControl against four global axes;
    // Xorg checks first_valuator + num_valuators against the device's
    // valuator class (Xi/chgdctl.c:157-160).
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let before = xi_query_side_effect_snapshot(&state, 1);
    let mut body = vec![1, 0, 2, 0, 1, 0, 12, 0, 2, 1, 0, 0];
    body.extend_from_slice(&0i32.to_le_bytes());
    let wire = dispatch_xi_request_wire(&mut state, &mut peer, 1, 35, &body);
    assert_eq!(wire[0], 0, "X error");
    assert_eq!(wire[1], x11::error::BAD_VALUE);
    assert_eq!(xi_query_side_effect_snapshot(&state, 1), before);
}

#[test]
fn xi1_shape_sweep_physical_device_control_keeps_four_axes() {
    // Kills: apply the two-axis CorePointer shape to a physical device
    // whose current XI classes retain four valuators.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let physical = state.xi_register_source(&crate::core_loop::DeviceInfo {
        source_id: crate::xinput::InputSourceId(0x51ab),
        enabled: true,
        resume_key: None,
        capabilities: crate::xinput::InputCapabilities {
            pointer: true,
            ..Default::default()
        },
        name: "physical-control-mouse".into(),
        device_node: "/dev/input/event-control-shape".into(),
        sysname: "event-control-shape".into(),
        vendor_id: 1,
        product_id: 4,
        is_touchpad: false,
        config: Default::default(),
    })[0];
    let before = xi_query_side_effect_snapshot(&state, 1);
    let wire = dispatch_xi_request_wire(
        &mut state,
        &mut peer,
        1,
        34,
        &[1, 0, u8::try_from(physical).unwrap(), 0],
    );
    assert_eq!(wire.len(), 88, "reply + 4-axis resolution state");
    assert_eq!(u32::from_le_bytes(wire[36..40].try_into().unwrap()), 4);
    assert_eq!(xi_query_side_effect_snapshot(&state, 1), before);
}

#[test]
fn xi1_shape_sweep_core_pointer_mapping_uses_master_button_count() {
    // Kills: keep core Get/SetPointerMapping at the former seven-button
    // constant; Xorg reads PickPointer(client)->button->numButtons
    // (dix/devices.c:1904-1912, 1994-2016).
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let before = xi_query_side_effect_snapshot(&state, 1);
    let get = dispatch_wire_request(&mut state, &mut peer, 1, 117, 0, &[]);
    assert_eq!(get[1], 10, "CorePointer has ten buttons");
    assert_eq!(&get[32..42], &(1..=10).collect::<Vec<_>>());

    let map: Vec<u8> = (1..=10).collect();
    let mut body = map.clone();
    body.resize(12, 0);
    let set = dispatch_wire_request(&mut state, &mut peer, 2, 116, 10, &body);
    assert!(
        set.len() >= 64,
        "MappingNotify plus SetPointerMapping reply"
    );
    assert_eq!(set[set.len() - 32], 1, "SetPointerMapping reply");
    let after = dispatch_wire_request(&mut state, &mut peer, 3, 117, 0, &[]);
    assert_eq!(after[1], 10);
    assert_eq!(&after[32..42], &map);
    assert_eq!(xi_query_side_effect_snapshot(&state, 1), before);
}

#[test]
fn xi1_shape_sweep_fake_input_validates_current_master_buttons() {
    // Kills: branch on device ID (master always ten buttons) instead of
    // checking the class currently copied onto master 2. Xorg validates
    // details against dev->button->numButtons (Xext/xtest.c:401-409).
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let physical = state.xi_register_source(&crate::core_loop::DeviceInfo {
        source_id: crate::xinput::InputSourceId(0x51ac),
        enabled: true,
        resume_key: None,
        capabilities: crate::xinput::InputCapabilities {
            pointer: true,
            ..Default::default()
        },
        name: "master-shape-mouse".into(),
        device_node: "/dev/input/event-master-shape".into(),
        sysname: "event-master-shape".into(),
        vendor_id: 1,
        product_id: 5,
        is_touchpad: false,
        config: Default::default(),
    })[0];
    assert!(state.xi_record_last_slave(2, physical));
    assert_eq!(state.xi_devices.device(2).unwrap().class_sourceid, physical);
    assert_eq!(
        state
            .xi_devices
            .device(2)
            .unwrap()
            .class_shape
            .button_count(),
        7
    );
    let before = xi_query_side_effect_snapshot(&state, 1);

    let mut body = vec![0; 32];
    body[0] = crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_BUTTON_PRESS_OFFSET;
    body[1] = 8;
    body[31] = 2;
    let wire = dispatch_wire_request(&mut state, &mut peer, 1, 146, 2, &body);
    assert_eq!(wire[0], 0, "X error");
    assert_eq!(wire[1], x11::error::BAD_VALUE);
    assert_eq!(u32::from_le_bytes(wire[4..8].try_into().unwrap()), 8);
    assert_eq!(xi_query_side_effect_snapshot(&state, 1), before);
}

#[test]
fn xi1_button_map_prefix_preserves_the_unsupplied_tail() {
    // Kills: replacing the entire stored map with a short request instead
    // of Xorg's prefix memcpy (`dix/inpututils.c:72-80`).
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let stable_before = xi_query_side_effect_snapshot(&state, 1);
    let custom: Vec<u8> = (11..=20).collect();
    let mut full = vec![4, 10, 0, 0];
    full.extend_from_slice(&custom);
    let first = dispatch_xi_request_wire(&mut state, &mut peer, 1, 29, &full);
    assert_eq!(first[0], 1);
    assert_eq!(first[8], 0, "full custom map succeeds");

    let mut prefix = vec![4, 1, 0, 0, 1];
    prefix.resize(8, 0);
    let second = dispatch_xi_request_wire(&mut state, &mut peer, 2, 29, &prefix);
    assert_eq!(second[0], 1);
    assert_eq!(second[8], 0, "one-entry prefix succeeds");
    let get = dispatch_xi_request_wire(&mut state, &mut peer, 3, 28, &[4, 0, 0, 0]);
    assert_eq!(get[8], 10);
    assert_eq!(&get[32..42], &[1, 12, 13, 14, 15, 16, 17, 18, 19, 20]);
    assert_eq!(
        state.xi1_button_map.get(&4),
        Some(&vec![1, 12, 13, 14, 15, 16, 17, 18, 19, 20])
    );
    assert_eq!(xi_query_side_effect_snapshot(&state, 1), stable_before);
}

#[test]
fn t6_xi1_select_extension_event_populates_client_class_set() {
    // Selecting a DevicePropertyNotify class for XTEST pointer 4 stores
    // the exact wire-form class in `xi1_event_classes`. Classes the
    // server does not yet implement (e.g. a hypothetical XI1
    // motion-event class 0x04XX with the wrong low byte) are
    // silently dropped — Xorg's behaviour.
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    // Body: window=ROOT, count=2, [DevicePropertyNotify(XTEST pointer),
    // motion-event(XTEST pointer)]. The motion-event class has low byte
    // != 82 so must be dropped.
    let unknown_class: u32 = ((crate::xinput::DEVICEID_XTEST_POINTER as u32) << 8) | 70; // bogus DeviceMotion class
    let body = xi1_select_extension_event_body(
        ROOT_WINDOW.0,
        &[XI1_DEV_PROP_CLASS_XTEST_POINTER, unknown_class],
    );
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        xi2_header_for_body(6, &body),
        &body,
    )
    .unwrap();
    let client = state.clients.get(&1).expect("client installed");
    assert!(
        client
            .xi1_event_classes
            .contains(&XI1_DEV_PROP_CLASS_XTEST_POINTER),
        "DevicePropertyNotify class stored verbatim"
    );
    assert!(
        !client.xi1_event_classes.contains(&unknown_class),
        "unsupported XI1 event classes dropped"
    );
    assert_eq!(
        client.xi1_event_classes.len(),
        1,
        "exactly one class accepted"
    );
}

#[test]
fn t6_xi1_select_extension_event_replaces_classes_for_device() {
    // Xorg's SelectExtensionEvent REPLACES the prior selection for
    // every deviceid mentioned in the request. So a follow-up call
    // with an empty class list for the same device drops the entry.
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    // Step 1: select DevicePropertyNotify for XTEST pointer 4.
    let body = xi1_select_extension_event_body(ROOT_WINDOW.0, &[XI1_DEV_PROP_CLASS_XTEST_POINTER]);
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        xi2_header_for_body(6, &body),
        &body,
    )
    .unwrap();
    assert!(
        state
            .clients
            .get(&1)
            .unwrap()
            .xi1_event_classes
            .contains(&XI1_DEV_PROP_CLASS_XTEST_POINTER)
    );
    // Step 2: re-select with a class list that names XTEST pointer 4 but
    // requests a class we don't handle. The replacement must clear
    // the prior device-4 entry.
    let unknown_class_dev4: u32 = ((crate::xinput::DEVICEID_XTEST_POINTER as u32) << 8) | 70;
    let body = xi1_select_extension_event_body(ROOT_WINDOW.0, &[unknown_class_dev4]);
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(2),
        xi2_header_for_body(6, &body),
        &body,
    )
    .unwrap();
    let client = state.clients.get(&1).unwrap();
    assert!(
        client.xi1_event_classes.is_empty(),
        "replacement with no supported classes clears XTEST pointer selection"
    );
}

#[test]
fn t6_xi1_select_extension_event_other_device_preserved() {
    // Replacement is scoped to the deviceids mentioned in the new
    // request. Pre-select XTEST pointer 4 + XTEST keyboard 5; re-select
    // only XTEST pointer 4. The keyboard entry must survive (Xorg's
    // per-device replace).
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let class_dev5 = XI1_DEV_PROP_CLASS_XTEST_KEYBOARD;
    // Seed via the dispatch — two classes, two devices.
    let body = xi1_select_extension_event_body(
        ROOT_WINDOW.0,
        &[XI1_DEV_PROP_CLASS_XTEST_POINTER, class_dev5],
    );
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        xi2_header_for_body(6, &body),
        &body,
    )
    .unwrap();
    // Re-select only XTEST pointer 4 (with the supported class). The
    // keyboard entry must remain — its deviceid wasn't named here.
    let body = xi1_select_extension_event_body(ROOT_WINDOW.0, &[XI1_DEV_PROP_CLASS_XTEST_POINTER]);
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(2),
        xi2_header_for_body(6, &body),
        &body,
    )
    .unwrap();
    let client = state.clients.get(&1).unwrap();
    assert!(
        client
            .xi1_event_classes
            .contains(&XI1_DEV_PROP_CLASS_XTEST_POINTER),
        "XTEST pointer selection re-installed"
    );
    assert!(
        client.xi1_event_classes.contains(&class_dev5),
        "XTEST keyboard selection untouched"
    );
}

#[test]
fn xi1_get_selected_extension_events_reports_this_and_all_clients() {
    let mut state = ServerState::new();
    let mut peer1 = install_client(&mut state, 1);
    let _peer2 = install_client(&mut state, 2);
    let mut backend = RecordingBackend::new();
    let motion_dev4: u32 = ((crate::xinput::DEVICEID_XTEST_POINTER as u32) << 8) | 70;
    let property_dev5 = XI1_DEV_PROP_CLASS_XTEST_KEYBOARD;

    let body = xi1_select_extension_event_body(
        ROOT_WINDOW.0,
        &[XI1_DEV_PROP_CLASS_XTEST_POINTER, motion_dev4],
    );
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        xi2_header_for_body(6, &body),
        &body,
    )
    .unwrap();
    let body = xi1_select_extension_event_body(ROOT_WINDOW.0, &[property_dev5]);
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(2),
        SequenceNumber(1),
        xi2_header_for_body(6, &body),
        &body,
    )
    .unwrap();

    let body = ROOT_WINDOW.0.to_le_bytes();
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(2),
        xi2_header_for_body(7, &body),
        &body,
    )
    .unwrap();
    let wire = read_all_available(&mut peer1);
    assert_eq!(wire.len(), 32 + 5 * 4);
    assert_eq!(wire[1], 7, "XI1 reply subtype");
    assert_eq!(u32::from_le_bytes(wire[4..8].try_into().unwrap()), 5);
    assert_eq!(u16::from_le_bytes(wire[8..10].try_into().unwrap()), 2);
    assert_eq!(u16::from_le_bytes(wire[10..12].try_into().unwrap()), 3);
    let classes: Vec<u32> = wire[32..]
        .chunks_exact(4)
        .map(|chunk| u32::from_le_bytes(chunk.try_into().unwrap()))
        .collect();
    assert_eq!(
        classes,
        vec![
            motion_dev4,
            XI1_DEV_PROP_CLASS_XTEST_POINTER,
            motion_dev4,
            XI1_DEV_PROP_CLASS_XTEST_POINTER,
            property_dev5,
        ],
        "this-client list precedes the all-clients list"
    );
}

#[test]
fn xi_config_completion_t6_xi1_change_delivers_device_property_notify() {
    // End-to-end: a client SelectExtensionEvent(DevicePropertyNotify
    // for XTEST pointer 4), then someone (here, the same client via XI2
    // XIChangeProperty minor 57) writes a property. The 32-byte
    // XI1 event must land on the selecting client with the right
    // type byte, atom, and deviceid.
    let mut state = ServerState::new();
    let mut peer = install_capture_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    seed_pointer_for_t3(&mut state);
    let (mut inventory, source) = inventory_for_t3_source(&state);
    let mut pending = crate::core_loop::run::PendingBackendRequests::default();
    let mut lane = crate::core_loop::run::XiConfigLane::default();
    let physical_class = (u32::from(TEST_PHYSICAL_POINTER_ID) << 8) | 82;
    // Subscribe via SelectExtensionEvent only (no XI2 mask), so the
    // ONLY event on the wire is the XI1 one.
    let select_body = xi1_select_extension_event_body(ROOT_WINDOW.0, &[physical_class]);
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        xi2_header_for_body(6, &select_body),
        &select_body,
    )
    .unwrap();
    // Drain anything the select produced (must be empty — minor 6
    // is event-only, no reply).
    assert!(
        read_all_available(&mut peer).is_empty(),
        "SelectExtensionEvent emits no reply"
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
        2,
        xi2_header(57),
        &body,
    );
    let wire = read_all_available(&mut peer);
    assert_eq!(wire.len(), 32, "exactly one 32-byte XI1 event");
    let ev = find_xi1_device_property_notify(&wire).expect("XI1 event present");
    assert_eq!(ev[0], 82, "type = XI_FIRST_EVENT(66) + 16");
    assert_eq!(ev[1], 0, "state = PropertyNewValue");
    assert_eq!(
        u32::from_le_bytes([ev[8], ev[9], ev[10], ev[11]]),
        tap_atom,
        "atom"
    );
    assert_eq!(
        ev[31],
        u8::try_from(TEST_PHYSICAL_POINTER_ID).unwrap(),
        "deviceid (last byte)"
    );
    assert_confirmed_tap_write(&state, &inventory, source, AtomId(tap_atom));
    assert_eq!(backend.started_device_configs.len(), 1);
    assert!(lane.is_empty());
    assert!(pending.is_empty());
}

#[test]
fn t6_xi1_delete_property_delivers_event_with_state_one() {
    // The delete path (XI2 minor 58) must also fan out an XI1
    // event, with `state = 1` (PropertyDelete).
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    seed_one_prop(&mut state, 100, 8, vec![1]);
    let select_body =
        xi1_select_extension_event_body(ROOT_WINDOW.0, &[XI1_DEV_PROP_CLASS_XTEST_POINTER]);
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        xi2_header_for_body(6, &select_body),
        &select_body,
    )
    .unwrap();
    let _ = read_all_available(&mut peer);

    let mut body = Vec::new();
    body.extend_from_slice(&4u16.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes());
    body.extend_from_slice(&100u32.to_le_bytes());
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(2),
        xi2_header(58),
        &body,
    )
    .unwrap();
    let wire = read_all_available(&mut peer);
    assert_eq!(wire.len(), 32, "exactly one 32-byte XI1 event");
    let ev = find_xi1_device_property_notify(&wire).expect("XI1 event present");
    assert_eq!(ev[1], 1, "state = PropertyDelete");
    assert_eq!(
        u32::from_le_bytes([ev[8], ev[9], ev[10], ev[11]]),
        100,
        "atom"
    );
    assert_eq!(ev[31], 4, "deviceid");
}

#[test]
fn xi_config_completion_t6_xi1_unselected_client_gets_nothing() {
    // A client that did NOT call SelectExtensionEvent must not
    // receive an XI1 DevicePropertyNotify — even if the property
    // change happens on the device it cares about.
    let mut state = ServerState::new();
    let mut peer = install_capture_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    seed_pointer_for_t3(&mut state);
    let (mut inventory, source) = inventory_for_t3_source(&state);
    let mut pending = crate::core_loop::run::PendingBackendRequests::default();
    let mut lane = crate::core_loop::run::XiConfigLane::default();
    // No SelectExtensionEvent, no XI2 mask.
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
        "unselected client receives nothing"
    );
    assert_confirmed_tap_write(&state, &inventory, source, AtomId(tap_atom));
    assert_eq!(backend.started_device_configs.len(), 1);
    assert!(lane.is_empty());
    assert!(pending.is_empty());
}

#[test]
fn xi_config_completion_t6_xi1_wrong_device_class_does_not_match() {
    // Selecting DevicePropertyNotify for XTEST keyboard 5 must NOT route a
    // physical pointer property change to that client (parallel to the
    // XI2 same-property-different-device test in T5).
    let mut state = ServerState::new();
    let mut peer = install_capture_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    seed_pointer_for_t3(&mut state);
    let (mut inventory, source) = inventory_for_t3_source(&state);
    let mut pending = crate::core_loop::run::PendingBackendRequests::default();
    let mut lane = crate::core_loop::run::XiConfigLane::default();
    let class_dev5 = XI1_DEV_PROP_CLASS_XTEST_KEYBOARD;
    let select_body = xi1_select_extension_event_body(ROOT_WINDOW.0, &[class_dev5]);
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        xi2_header_for_body(6, &select_body),
        &select_body,
    )
    .unwrap();
    let _ = read_all_available(&mut peer);

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
        2,
        xi2_header(57),
        &body,
    );
    assert!(
        read_all_available(&mut peer).is_empty(),
        "device-5 selection must not see physical pointer events"
    );
    assert_confirmed_tap_write(&state, &inventory, source, AtomId(tap_atom));
    assert_eq!(backend.started_device_configs.len(), 1);
    assert!(lane.is_empty());
    assert!(pending.is_empty());
}

#[test]
fn t6_xi1_select_extension_event_short_body_bad_length() {
    // A body shorter than the 8-byte fixed header must yield
    // BadLength. The error reply uses minor=6 and major=137.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    // Only 4 bytes — missing the count/pad portion.
    let body = vec![0u8; 4];
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        xi2_header_for_body(6, &body),
        &body,
    )
    .unwrap();
    let wire = read_all_available(&mut peer);
    assert_eq!(wire.len(), 32, "single 32-byte X error reply");
    assert_eq!(wire[0], 0, "X error opcode");
    assert_eq!(wire[1], 16, "BadLength code");
}

#[test]
fn t6_xi1_select_extension_event_truncated_class_list_bad_length() {
    // count = 2 but only one class on the wire → BadLength.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let mut body = Vec::new();
    body.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    body.extend_from_slice(&2u16.to_le_bytes()); // count = 2
    body.extend_from_slice(&0u16.to_le_bytes()); // pad
    body.extend_from_slice(&XI1_DEV_PROP_CLASS_XTEST_POINTER.to_le_bytes()); // only one class
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        xi2_header_for_body(6, &body),
        &body,
    )
    .unwrap();
    let wire = read_all_available(&mut peer);
    assert_eq!(wire[0], 0, "X error opcode");
    assert_eq!(wire[1], 16, "BadLength");
}

#[test]
fn xi_config_completion_t6_dual_selection_delivers_each_event_path() {
    // A client that selected `XI_PropertyEvent` via XI2's
    // `xi2_masks` AND a `DevicePropertyNotify` class via XI1's
    // `SelectExtensionEvent` receives TWO independent events from
    // a single property write — one per protocol path. The fan-outs
    // do not deduplicate across paths because a client may be
    // running adapter code that bridges both (e.g. a GTK app that
    // uses XI2 directly plus a legacy libXi consumer in the same
    // process). Pins the no-cross-path-dedup invariant.
    let mut state = ServerState::new();
    let mut peer = install_capture_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    seed_pointer_for_t3(&mut state);
    let (mut inventory, source) = inventory_for_t3_source(&state);
    let mut pending = crate::core_loop::run::PendingBackendRequests::default();
    let mut lane = crate::core_loop::run::XiConfigLane::default();
    let physical_class = (u32::from(TEST_PHYSICAL_POINTER_ID) << 8) | 82;
    // XI2 selection.
    select_xi2_property_event_on_root(&mut state, 1, TEST_PHYSICAL_POINTER_ID);
    // XI1 selection.
    let select_body = xi1_select_extension_event_body(ROOT_WINDOW.0, &[physical_class]);
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        xi2_header_for_body(6, &select_body),
        &select_body,
    )
    .unwrap();
    assert!(
        read_all_available(&mut peer).is_empty(),
        "SelectExtensionEvent emits no reply"
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
        2,
        xi2_header(57),
        &body,
    );
    let wire = read_all_available(&mut peer);
    assert_eq!(wire.len(), 64, "two 32-byte events (XI2 + XI1)");
    // Both events must be present — anchor by type byte.
    let xi2 = find_xi2_property_event(&wire).expect("XI2 GenericEvent present");
    assert_eq!(xi2[0], 35, "GenericEvent");
    assert_eq!(u16::from_le_bytes([xi2[8], xi2[9]]), 12, "evtype = 12");
    assert_eq!(
        u16::from_le_bytes([xi2[10], xi2[11]]),
        TEST_PHYSICAL_POINTER_ID,
        "deviceid"
    );
    let xi1 = find_xi1_device_property_notify(&wire).expect("XI1 event present");
    assert_eq!(xi1[0], 82, "XI_FIRST_EVENT + 16");
    assert_eq!(
        xi1[31],
        u8::try_from(TEST_PHYSICAL_POINTER_ID).unwrap(),
        "XI1 deviceid (last byte)"
    );
    assert_confirmed_tap_write(&state, &inventory, source, AtomId(tap_atom));
    assert_eq!(backend.started_device_configs.len(), 1);
    assert!(lane.is_empty());
    assert!(pending.is_empty());
}

#[test]
fn xi_config_completion_readonly_descriptor_yields_bad_access() {
    let mut state = ServerState::new();
    let mut peer = install_capture_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    seed_pointer_for_t3(&mut state);
    let (mut inventory, _) = inventory_for_t3_source(&state);
    let mut pending = crate::core_loop::run::PendingBackendRequests::default();
    let mut lane = crate::core_loop::run::XiConfigLane::default();
    let properties_before = xi_property_snapshot(&state);
    // `…Enabled Default` is the ReadOnly companion (Access::ReadOnly).
    let default_atom = state
        .atoms
        .intern("libinput Tapping Enabled Default", false)
        .0;
    let integer_atom = crate::xinput::XA_INTEGER.0;

    // Capture the seeded value so we can confirm the write was
    // rejected and the registry was not mutated.
    let before = state
        .xi_devices
        .iter()
        .find(|d| d.id == TEST_PHYSICAL_POINTER_ID)
        .unwrap()
        .properties
        .get(&AtomId(default_atom))
        .cloned()
        .expect("default seeded");

    let body = xi2_change_property_body(
        TEST_PHYSICAL_POINTER_ID,
        crate::xinput::XI_PROP_MODE_REPLACE,
        8,
        default_atom,
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
        2,
        xi2_header(57),
        &body,
    );
    let wire = read_all_available(&mut peer);
    assert_xi_config_error(&wire, 10, 2, 57);

    let after = state
        .xi_devices
        .iter()
        .find(|d| d.id == TEST_PHYSICAL_POINTER_ID)
        .unwrap()
        .properties
        .get(&AtomId(default_atom))
        .cloned()
        .expect("default still present");
    assert_eq!(before.data, after.data, "read-only value unchanged");
    assert_eq!(xi_property_snapshot(&state), properties_before);
    assert!(backend.started_device_configs.is_empty());
    assert!(lane.is_empty());
    assert!(pending.is_empty());
}

// -----------------------------------------------------------------
// XI 1.x `SendExtensionEvent` (minor 31) — dispatch + specials.
// -----------------------------------------------------------------

/// Build an `xSendExtensionEventReq` body (the bytes after the
/// 4-byte X request header) for a single 32-byte event payload and
/// a single XEventClass.
fn xi1_send_extension_event_body(
    destination: u32,
    deviceid: u8,
    propagate: bool,
    event: &[u8; 32],
    classes: &[u32],
) -> Vec<u8> {
    let mut body = Vec::with_capacity(12 + 32 + classes.len() * 4);
    body.extend_from_slice(&destination.to_le_bytes()); // 0..4 destination
    body.push(deviceid); // 4 deviceid
    body.push(u8::from(propagate)); // 5 propagate
    #[allow(clippy::cast_possible_truncation)]
    let count = classes.len() as u16;
    body.extend_from_slice(&count.to_le_bytes()); // 6..8 count
    body.push(1); // 8 num_events
    body.extend_from_slice(&[0u8; 3]); // 9..12 pad
    body.extend_from_slice(event); // 12..44 one xEvent
    for c in classes {
        body.extend_from_slice(&c.to_le_bytes());
    }
    body
}

/// `length_units` covering the 4-byte X header + the body produced
/// by [`xi1_send_extension_event_body`] for a single 32-byte event:
/// `4 + 8*num_events + class_count` = `4 + 8 + classes.len()`.
#[allow(clippy::cast_possible_truncation)]
fn xi1_send_extension_event_header(class_count: usize) -> RequestHeader {
    RequestHeader {
        opcode: 131,
        data: 31,
        length_units: 12 + class_count as u32,
    }
}

/// 32-byte XI1 DeviceKeyPress wire template — only the type byte
/// matters for these tests; everything else is zero.
fn xi1_device_key_press_event(first_event: u8) -> [u8; 32] {
    let mut ev = [0u8; 32];
    ev[0] = first_event + crate::xinput::XI_DEVICE_KEY_PRESS_OFFSET;
    ev
}

/// XI1 _noExtensionEvent code (9) — see /usr/include/X11/extensions/XI.h.
/// `noextensioneventclass = (deviceid << 8) | 9` is the sentinel
/// Xlib's `NoExtensionEvent` macro produces. Xorg
/// `CreateMaskFromList` maps it to a per-device mask of 0, which
/// short-circuits `DeliverToWindowOwner`'s filter check
/// (`filter == CantBeFiltered`) and so the destination window's
/// CREATOR receives the event regardless of any selection.
const XI1_NO_EXTENSION_EVENT_OFFSET: u8 = 9;

fn seed_test_window(state: &mut ServerState, owner: u32, xid: u32) {
    state.resources.create_window(
        ClientId(owner),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(xid),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 100,
            height: 100,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
}

#[test]
fn xi1_send_extension_event_with_noextensioneventclass_delivers_to_window_creator() {
    // XSendExtensionEvent test 1 (xts5 XI/SendExtensionEvent.m:124):
    // a client creates a window and SendExtensionEvent's an XI1
    // event to it with `event_list = &noextensioneventclass`. The
    // expected behaviour mirrors Xorg `Xi/sendexev.c::SendEvent` →
    // `dix/events.c::DeliverToWindowOwner`: when the per-device
    // mask is empty (i.e. `noextensioneventclass`), the owner
    // receives the event unconditionally. Without this path xts5
    // SendExtensionEvent 1–4 / 11 / 12 / 14 / 16 all fail with
    // "Expected event (DeviceKeyPress) not received".
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let win_xid = 0x4000_0001u32;
    seed_test_window(&mut state, 1, win_xid);

    let no_class = (4u32 << 8) | u32::from(XI1_NO_EXTENSION_EVENT_OFFSET);
    let event = xi1_device_key_press_event(crate::server::XI_FIRST_EVENT);
    let body = xi1_send_extension_event_body(win_xid, 4, false, &event, &[no_class]);

    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        xi1_send_extension_event_header(1),
        &body,
    )
    .unwrap();

    let wire = read_all_available(&mut peer);
    assert_eq!(
        wire.len(),
        32,
        "exactly one 32-byte XI1 event delivered to creator"
    );
    assert_eq!(
        wire[0],
        (crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_KEY_PRESS_OFFSET) | 0x80,
        "type byte is DeviceKeyPress with send_event (0x80) bit set"
    );
}

#[test]
fn xi1_send_extension_event_pointer_window_resolves_to_sprite() {
    // dest = 0 (PointerWindow) must resolve to the window currently
    // containing the sprite (Xorg SendEvent:2915 — `pWin = spriteWin`).
    // With the sprite at (0,0) on the root and a 100x100 child
    // window covering the origin, the child's creator gets the
    // event when noextensioneventclass is used.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let win_xid = 0x4000_0002u32;
    seed_test_window(&mut state, 1, win_xid);
    // Map the window so `direct_child_at` finds it.
    if let Some(w) = state.resources.window_mut(ResourceId(win_xid)) {
        w.map_state = crate::resources::MapState::Viewable;
    }
    state.pointer_root = (10, 10);

    let no_class = (4u32 << 8) | u32::from(XI1_NO_EXTENSION_EVENT_OFFSET);
    let event = xi1_device_key_press_event(crate::server::XI_FIRST_EVENT);
    let body = xi1_send_extension_event_body(0, 4, false, &event, &[no_class]);

    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        xi1_send_extension_event_header(1),
        &body,
    )
    .unwrap();

    let wire = read_all_available(&mut peer);
    assert_eq!(
        wire.len(),
        32,
        "PointerWindow special resolves to sprite, creator gets event"
    );
}

#[test]
fn xi1_send_extension_event_input_focus_resolves_to_device_focus() {
    // dest = 1 (InputFocus) must resolve to the device's focus
    // window when the focus is a real window (not PointerRoot /
    // None) — Xorg SendEvent:2917-2942.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let win_xid = 0x4000_0003u32;
    seed_test_window(&mut state, 1, win_xid);
    state.xi1_device_focus.insert(
        4,
        crate::server::Xi1DeviceFocus {
            focus: win_xid,
            revert_to: 0,
            time: 0,
        },
    );

    let no_class = (4u32 << 8) | u32::from(XI1_NO_EXTENSION_EVENT_OFFSET);
    let event = xi1_device_key_press_event(crate::server::XI_FIRST_EVENT);
    let body = xi1_send_extension_event_body(1, 4, false, &event, &[no_class]);

    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        xi1_send_extension_event_header(1),
        &body,
    )
    .unwrap();

    let wire = read_all_available(&mut peer);
    assert_eq!(
        wire.len(),
        32,
        "InputFocus special resolves to device-focus window, creator gets event"
    );
}

#[test]
fn xi1_send_extension_event_propagate_walks_to_selecting_ancestor() {
    // xts5 XI/SendExtensionEvent purpose 11: 3-level window
    // hierarchy, only the top has a selection for `dbpc`, send
    // to bottom with propagate=True. Walk must reach the top.
    // Mirrors Xorg SendEvent's `for (; pWin; pWin = pWin->parent)`
    // loop (xserver.git Xi/exevents.c:2952-2962).
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let grandparent_xid = 0x4000_0010u32;
    let parent_xid = 0x4000_0011u32;
    let child_xid = 0x4000_0012u32;
    seed_test_window(&mut state, 2, grandparent_xid);
    state.resources.create_window(
        ClientId(2),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(parent_xid),
            parent: ResourceId(grandparent_xid),
            x: 0,
            y: 0,
            width: 50,
            height: 50,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    state.resources.create_window(
        ClientId(2),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(child_xid),
            parent: ResourceId(parent_xid),
            x: 0,
            y: 0,
            width: 25,
            height: 25,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    // Client 1 selects DeviceButtonPress (offset 3) on the
    // GRANDPARENT only. Neither parent nor child have any
    // selectors. With propagate=True, the dispatch must reach
    // grandparent.
    let dbpc = (4u32 << 8)
        | u32::from(crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_BUTTON_PRESS_OFFSET);
    if let Some(c) = state.clients.get_mut(&1) {
        c.xi1_window_event_classes
            .entry(ResourceId(grandparent_xid))
            .or_default()
            .insert(dbpc);
    }

    let mut event = [0u8; 32];
    event[0] = crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_BUTTON_PRESS_OFFSET;
    let body = xi1_send_extension_event_body(child_xid, 4, true, &event, &[dbpc]);
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        xi1_send_extension_event_header(1),
        &body,
    )
    .unwrap();

    let wire = read_all_available(&mut peer);
    assert_eq!(
        wire.len(),
        32,
        "propagate=True walks ancestors, grandparent selector receives event"
    );
    assert_eq!(
        wire[0],
        (crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_BUTTON_PRESS_OFFSET) | 0x80,
        "type byte is DeviceButtonPress + send_event"
    );
}
