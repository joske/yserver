use super::{
    access_notify_support::{
        CallOrder, CaseExpect, ExpectedReceive, NOTIFY_RECEIVER_ALLOWED, NOTIFY_RECEIVER_DENIED,
        NOTIFY_SENDER, NotifyFixture, ReceiveCase, Stream, check_receive_case,
        finish_literal_event, literal_event_header, literal_mapping_notify, receive_checks,
        request_opcodes,
    },
    xi_hotplug::{
        xi_hotplug_create_child, xi_hotplug_select_hierarchy, xi_hotplug_select_presence,
        xi_hotplug_set_enabled,
    },
    *,
};
use crate::{
    access::{Decision, EventDescription, EventSubject, RequestOpcodes},
    core_loop::process_request::RequestOutcome,
    resources::ROOT_WINDOW,
};
use std::{sync::atomic::Ordering, time::Duration};
use yserver_protocol::x11::{ClientId, ResourceId};

pub(super) const CHILD_WINDOW: ResourceId = ResourceId(0x0071_0001);

pub(super) fn hotplug_fixture() -> (NotifyFixture, u16) {
    let mut fixture = NotifyFixture::new();
    let device_id = seed_pointer_for_t3(&mut fixture.state);
    xi_hotplug_set_enabled(
        &mut fixture.state,
        &mut fixture.backend,
        NOTIFY_SENDER.0,
        1,
        device_id,
        true,
    );
    xi_hotplug_create_child(
        &mut fixture.state,
        &mut fixture.backend,
        NOTIFY_SENDER.0,
        2,
        CHILD_WINDOW,
    );
    fixture.state.start_instant = std::time::Instant::now() + Duration::from_secs(30);
    for client in [
        NOTIFY_SENDER,
        NOTIFY_RECEIVER_DENIED,
        NOTIFY_RECEIVER_ALLOWED,
    ] {
        fixture.drain(client);
    }
    (fixture, device_id)
}

pub(super) fn disable_device(fixture: &mut NotifyFixture, device_id: u16) {
    xi_hotplug_set_enabled(
        &mut fixture.state,
        &mut fixture.backend,
        NOTIFY_SENDER.0,
        3,
        device_id,
        false,
    );
}

fn literal_presence_disabled(sequence: u16, device_id: u16) -> Vec<u8> {
    literal_presence(sequence, device_id, 3)
}

fn literal_presence(sequence: u16, device_id: u16, change: u8) -> Vec<u8> {
    literal_presence_with_order(sequence, device_id, change, ClientByteOrder::LittleEndian)
}

fn literal_presence_with_order(
    sequence: u16,
    device_id: u16,
    change: u8,
    order: ClientByteOrder,
) -> Vec<u8> {
    let mut bytes = vec![81, 0]; // XI_FIRST_EVENT + XI_DevicePresenceNotify
    super::access_notify_support::append_event_u16(&mut bytes, order, sequence);
    super::access_notify_support::append_event_u32(&mut bytes, order, 0); // zero timestamp
    bytes.extend_from_slice(&[change, u8::try_from(device_id).unwrap(), 0, 0]);
    bytes.extend_from_slice(&[0; 20]);
    bytes
}

pub(super) fn literal_hierarchy_disabled(sequence: u16) -> Vec<u8> {
    literal_hierarchy_disabled_with_order(sequence, ClientByteOrder::LittleEndian)
}

fn literal_hierarchy_disabled_with_order(sequence: u16, order: ClientByteOrder) -> Vec<u8> {
    let mut bytes = vec![35, 137]; // GenericEvent, XInput major opcode
    super::access_notify_support::append_event_u16(&mut bytes, order, sequence);
    super::access_notify_support::append_event_u32(&mut bytes, order, 15); // five 12-byte device records
    super::access_notify_support::append_event_u16(&mut bytes, order, 11); // XI_HierarchyChanged
    super::access_notify_support::append_event_u16(&mut bytes, order, 0); // XIAllDevices
    super::access_notify_support::append_event_u32(&mut bytes, order, 0); // zero timestamp
    super::access_notify_support::append_event_u32(&mut bytes, order, 128); // XI_DeviceDisabled
    super::access_notify_support::append_event_u16(&mut bytes, order, 5);
    bytes.extend_from_slice(&[0; 10]); // header padding
    // Master pointer: attached to its paired master keyboard.
    append_hierarchy_record(&mut bytes, order, 2, 3, 1, true, 0);
    // Master keyboard: attached to its paired master pointer.
    append_hierarchy_record(&mut bytes, order, 3, 2, 2, true, 0);
    // Virtual XTEST pointer.
    append_hierarchy_record(&mut bytes, order, 4, 2, 3, true, 0);
    // Virtual XTEST keyboard.
    append_hierarchy_record(&mut bytes, order, 5, 3, 4, true, 0);
    // Disabled physical pointer facet (the changed device).
    append_hierarchy_record(&mut bytes, order, 6, 2, 3, false, 128);
    bytes
}

fn append_hierarchy_record(
    bytes: &mut Vec<u8>,
    order: ClientByteOrder,
    device_id: u16,
    attachment: u16,
    use_: u8,
    enabled: bool,
    flags: u32,
) {
    super::access_notify_support::append_event_u16(bytes, order, device_id);
    super::access_notify_support::append_event_u16(bytes, order, attachment);
    bytes.extend_from_slice(&[use_, u8::from(enabled)]);
    bytes.extend_from_slice(&[0; 2]);
    super::access_notify_support::append_event_u32(bytes, order, flags);
}

fn literal_disabled_stream_with_order(
    sequence: u16,
    device_id: u16,
    order: ClientByteOrder,
) -> Vec<u8> {
    [
        literal_presence_with_order(sequence, device_id, 3, order),
        literal_presence_with_order(sequence, device_id, 3, order),
        literal_hierarchy_disabled_with_order(sequence, order),
        literal_hierarchy_disabled_with_order(sequence, order),
    ]
    .concat()
}

pub(super) fn select_hierarchy_on_root_and_child(fixture: &mut NotifyFixture, client: ClientId) {
    xi_hotplug_select_hierarchy(
        &mut fixture.state,
        &mut fixture.backend,
        client.0,
        3,
        ROOT_WINDOW,
        true,
    );
    xi_hotplug_select_hierarchy(
        &mut fixture.state,
        &mut fixture.backend,
        client.0,
        4,
        CHILD_WINDOW,
        true,
    );
}

#[test]
fn receive_xi_hierarchy_subject_is_selected_window() {
    // mutation: pass W(ROOT_WINDOW) instead of the selected window in X2
    let build = || {
        let (mut fixture, device_id) = hotplug_fixture();
        select_hierarchy_on_root_and_child(&mut fixture, NOTIFY_RECEIVER_DENIED);
        (fixture, device_id)
    };
    let act = |fixture: &mut NotifyFixture,
               device_id: &u16,
               _: &std::sync::Mutex<Vec<ExpectedReceive>>| {
        disable_device(fixture, *device_id);
        let device = fixture.state.xi_devices.device(*device_id).unwrap();
        assert!(
            !device.enabled,
            "device disable persists in every policy mode"
        );
        assert_eq!(
            device.properties[&fixture.state.xi_device_enabled_atom].data,
            [0]
        );
        assert_eq!(fixture.state.xi_devices.attachment(*device_id), None);
    };

    for (name, denied_window) in [
        ("hierarchy root copy denied", ROOT_WINDOW),
        ("hierarchy child copy denied", CHILD_WINDOW),
    ] {
        let expect = |_: &u16| {
            let event = EventDescription::extension(35, 137, 11);
            let root = ExpectedReceive {
                client: NOTIFY_RECEIVER_DENIED,
                request: request_opcodes(137, 57),
                subject: EventSubject::Window(ROOT_WINDOW),
                events: vec![event],
            };
            let child = ExpectedReceive {
                subject: EventSubject::Window(CHILD_WINDOW),
                ..root.clone()
            };
            let frame = literal_hierarchy_disabled(4);
            CaseExpect::new(
                vec![
                    Stream::unchanged(NOTIFY_SENDER, Vec::new()),
                    Stream {
                        client: NOTIFY_RECEIVER_DENIED,
                        bytes: [frame.clone(), frame.clone()].concat(),
                        denied: Some(frame),
                    },
                    Stream::unchanged(NOTIFY_RECEIVER_ALLOWED, Vec::new()),
                ],
                vec![root.clone(), child.clone()],
                CallOrder::Multiset,
                vec![if denied_window == ROOT_WINDOW {
                    root
                } else {
                    child
                }],
            )
        };
        let case = ReceiveCase {
            name,
            build: &build,
            act: &act,
            expect: &expect,
        };
        check_receive_case(&case);
    }
}

#[test]
fn receive_xi_presence_denial_keeps_other_client() {
    // mutation: skip the receive check in X1 (use fanout_event_to_clients)
    let build = || {
        let (mut fixture, device_id) = hotplug_fixture();
        xi_hotplug_select_presence(
            &mut fixture.state,
            &mut fixture.backend,
            NOTIFY_RECEIVER_DENIED.0,
            3,
            ROOT_WINDOW,
        );
        xi_hotplug_select_presence(
            &mut fixture.state,
            &mut fixture.backend,
            NOTIFY_RECEIVER_ALLOWED.0,
            3,
            CHILD_WINDOW,
        );
        (fixture, device_id)
    };
    let act = |fixture: &mut NotifyFixture,
               device_id: &u16,
               _: &std::sync::Mutex<Vec<ExpectedReceive>>| {
        disable_device(fixture, *device_id);
        let device = fixture.state.xi_devices.device(*device_id).unwrap();
        assert!(
            !device.enabled,
            "device disable persists in every policy mode"
        );
        assert_eq!(
            device.properties[&fixture.state.xi_device_enabled_atom].data,
            [0]
        );
        assert_eq!(fixture.state.xi_devices.attachment(*device_id), None);
    };
    let expect = |device_id: &u16| {
        let device_id = *device_id;
        let event = EventDescription::extension(crate::server::XI_FIRST_EVENT + 15, 137, 15);
        let denied_receive = ExpectedReceive {
            client: NOTIFY_RECEIVER_DENIED,
            request: request_opcodes(137, 57),
            subject: EventSubject::Window(ROOT_WINDOW),
            events: vec![event],
        };
        let allowed_receive = ExpectedReceive {
            client: NOTIFY_RECEIVER_ALLOWED,
            request: request_opcodes(137, 57),
            subject: EventSubject::Window(CHILD_WINDOW),
            events: vec![event],
        };
        let denied_bytes = literal_presence_disabled(3, device_id);
        CaseExpect::new(
            vec![
                Stream::unchanged(NOTIFY_SENDER, Vec::new()),
                Stream {
                    client: NOTIFY_RECEIVER_DENIED,
                    bytes: denied_bytes,
                    denied: Some(Vec::new()),
                },
                Stream::unchanged(
                    NOTIFY_RECEIVER_ALLOWED,
                    literal_presence_disabled(3, device_id),
                ),
            ],
            vec![denied_receive.clone(), allowed_receive],
            CallOrder::PerClient,
            vec![denied_receive],
        )
    };
    let case = ReceiveCase {
        name: "XI presence denial keeps other client",
        build: &build,
        act: &act,
        expect: &expect,
    };
    check_receive_case(&case);
}

#[test]
fn receive_xi_presence_from_server_work_has_no_request() {
    // Focused because it checks and clears the Receive log between two checkpoints.
    // mutation: hard-code Some(RequestOpcodes) in the check
    let (mut fixture, device_id) = hotplug_fixture();
    xi_hotplug_select_presence(
        &mut fixture.state,
        &mut fixture.backend,
        NOTIFY_RECEIVER_DENIED.0,
        3,
        ROOT_WINDOW,
    );
    let calls = fixture.install_policy(|_, _| Decision::Allow);
    assert!(fixture.state.xi_set_facet_client_disabled(device_id, true));
    crate::xinput::hotplug::publish_facet_disabled(&mut fixture.state, device_id);
    let server_work = receive_checks(&calls);
    assert_eq!(server_work.len(), 1);
    assert_eq!(server_work[0].client, NOTIFY_RECEIVER_DENIED);
    assert_eq!(
        server_work[0].receive,
        Some(EventSubject::Window(ROOT_WINDOW))
    );
    assert_eq!(server_work[0].request, None);
    assert_eq!(
        fixture.drain(NOTIFY_RECEIVER_DENIED),
        literal_presence_disabled(3, device_id)
    );

    calls.lock().unwrap().clear();
    xi_hotplug_set_enabled(
        &mut fixture.state,
        &mut fixture.backend,
        NOTIFY_SENDER.0,
        4,
        device_id,
        true,
    );
    let request_work = receive_checks(&calls);
    assert_eq!(request_work.len(), 1);
    assert_eq!(
        request_work[0].request,
        Some(RequestOpcodes {
            major: 137,
            minor: 57
        })
    );
    assert_eq!(
        fixture.drain(NOTIFY_RECEIVER_DENIED),
        literal_presence(3, device_id, 2)
    );
    assert!(fixture.state.xi_devices.device(device_id).unwrap().enabled);
}

#[test]
fn receive_xi_hotplug_baseline_frames() {
    // Focused because it is a byte-only baseline-frame test.
    // mutation: drop the last hierarchy device record
    let (mut fixture, device_id) = hotplug_fixture();
    for client in [NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED] {
        let (
            hierarchy_root_sequence,
            hierarchy_child_sequence,
            presence_root_sequence,
            presence_child_sequence,
            order,
        ) = if client == NOTIFY_RECEIVER_DENIED {
            (3, 4, 5, 6, ClientByteOrder::LittleEndian)
        } else {
            (7, 8, 9, 10, ClientByteOrder::BigEndian)
        };
        fixture.state.clients.get_mut(&client.0).unwrap().byte_order = order;
        xi_hotplug_select_hierarchy(
            &mut fixture.state,
            &mut fixture.backend,
            client.0,
            hierarchy_root_sequence,
            ROOT_WINDOW,
            true,
        );
        xi_hotplug_select_hierarchy(
            &mut fixture.state,
            &mut fixture.backend,
            client.0,
            hierarchy_child_sequence,
            CHILD_WINDOW,
            true,
        );
        xi_hotplug_select_presence(
            &mut fixture.state,
            &mut fixture.backend,
            client.0,
            presence_root_sequence,
            ROOT_WINDOW,
        );
        xi_hotplug_select_presence(
            &mut fixture.state,
            &mut fixture.backend,
            client.0,
            presence_child_sequence,
            CHILD_WINDOW,
        );
    }
    disable_device(&mut fixture, device_id);
    assert_eq!(
        fixture.drain(NOTIFY_RECEIVER_DENIED),
        literal_disabled_stream_with_order(6, device_id, ClientByteOrder::LittleEndian),
        "complete little-endian hotplug frames with sequence 6"
    );
    assert_eq!(
        fixture.drain(NOTIFY_RECEIVER_ALLOWED),
        literal_disabled_stream_with_order(10, device_id, ClientByteOrder::BigEndian),
        "complete big-endian hotplug frames with sequence 10"
    );
}

fn select_device_changed(
    fixture: &mut NotifyFixture,
    client: ClientId,
    sequence: u16,
    window: ResourceId,
    device_id: u16,
) {
    select_device_changed_with_order(
        fixture,
        client,
        sequence,
        window,
        device_id,
        ClientByteOrder::LittleEndian,
    );
}

fn select_device_changed_with_order(
    fixture: &mut NotifyFixture,
    client: ClientId,
    sequence: u16,
    window: ResourceId,
    device_id: u16,
    order: ClientByteOrder,
) {
    // NotifyFixture calls process_request after the reader's body swap, so
    // the reused XI test builder supplies its normalized little-endian body.
    fixture
        .state
        .clients
        .get_mut(&client.0)
        .expect("fixture client")
        .byte_order = order;
    let body = super::xi2_grabs::xi_dynamic_select_events_body(
        window.0,
        device_id,
        crate::xinput::XI2_DEVICE_CHANGED_MASK,
    );
    let outcome = fixture
        .request(
            client,
            SequenceNumber(sequence),
            RequestHeader {
                opcode: 137,
                data: 46,
                length_units: 5,
            },
            &body,
        )
        .expect("XISelectEvents DeviceChanged selection");
    assert!(
        matches!(outcome, RequestOutcome::Handled),
        "XISelectEvents must complete without disconnecting its issuer"
    );
}

fn literal_device_changed_event(
    sequence: u16,
    device_id: u16,
    source_id: u16,
    num_classes: u16,
    classes: &[u8],
    order: ClientByteOrder,
) -> Vec<u8> {
    let mut bytes = vec![35, 137]; // GenericEvent, XInput major opcode
    super::access_notify_support::append_event_u16(&mut bytes, order, sequence);
    super::access_notify_support::append_event_u32(
        &mut bytes,
        order,
        u32::try_from(classes.len() / 4).unwrap(),
    );
    super::access_notify_support::append_event_u16(&mut bytes, order, 1); // XI_DeviceChanged
    super::access_notify_support::append_event_u16(&mut bytes, order, device_id);
    super::access_notify_support::append_event_u32(&mut bytes, order, 0);
    super::access_notify_support::append_event_u16(&mut bytes, order, num_classes);
    super::access_notify_support::append_event_u16(&mut bytes, order, source_id);
    bytes.extend_from_slice(&[1, 0]); // SlaveSwitch, pad0
    super::access_notify_support::append_event_u16(&mut bytes, order, 0);
    super::access_notify_support::append_event_u32(&mut bytes, order, 0);
    super::access_notify_support::append_event_u32(&mut bytes, order, 0);
    bytes.extend_from_slice(classes);
    bytes
}

fn literal_xtest_device_changed_event(sequence: u16, order: ClientByteOrder) -> Vec<u8> {
    // The existing emitter builds this shared class block in little endian;
    // preserve those bytes while the event header follows the recipient.
    let class_order = ClientByteOrder::LittleEndian;
    let mut classes = Vec::new();
    super::access_notify_support::append_event_u16(&mut classes, class_order, 1); // ButtonClass
    super::access_notify_support::append_event_u16(&mut classes, class_order, 13); // 13 four-byte units
    super::access_notify_support::append_event_u16(
        &mut classes,
        class_order,
        crate::xinput::DEVICEID_XTEST_POINTER,
    );
    super::access_notify_support::append_event_u16(&mut classes, class_order, 10); // CorePointerProc buttons
    classes.extend_from_slice(&[0; 4]); // DeviceChanged does not report button state
    for atom in 0x4b_u32..=0x51 {
        super::access_notify_support::append_event_u32(&mut classes, class_order, atom);
    }
    for _ in 0..3 {
        super::access_notify_support::append_event_u32(&mut classes, class_order, 0);
    }

    for (axis, label, value) in [(0_u16, 0x52_u32, 400_i32), (1, 0x53, 300)] {
        super::access_notify_support::append_event_u16(&mut classes, class_order, 2); // ValuatorClass
        super::access_notify_support::append_event_u16(&mut classes, class_order, 11); // 11 units
        super::access_notify_support::append_event_u16(
            &mut classes,
            class_order,
            crate::xinput::DEVICEID_XTEST_POINTER,
        );
        super::access_notify_support::append_event_u16(&mut classes, class_order, axis);
        super::access_notify_support::append_event_u32(&mut classes, class_order, label);
        super::access_notify_support::append_event_u32(&mut classes, class_order, u32::MAX); // minimum = -1
        super::access_notify_support::append_event_u32(&mut classes, class_order, 0);
        super::access_notify_support::append_event_u32(&mut classes, class_order, u32::MAX); // maximum = -1
        super::access_notify_support::append_event_u32(&mut classes, class_order, 0);
        super::access_notify_support::append_event_u32(&mut classes, class_order, value as u32);
        super::access_notify_support::append_event_u32(&mut classes, class_order, 0);
        super::access_notify_support::append_event_u32(&mut classes, class_order, 0);
        classes.extend_from_slice(&[0; 4]); // relative mode and padding
    }
    assert_eq!(classes.len(), 140, "complete XTEST pointer class payload");

    let mut bytes = vec![35, 137]; // GenericEvent, XInput major opcode
    super::access_notify_support::append_event_u16(&mut bytes, order, sequence);
    super::access_notify_support::append_event_u32(
        &mut bytes,
        order,
        u32::try_from(classes.len() / 4).unwrap(),
    );
    super::access_notify_support::append_event_u16(&mut bytes, order, 1); // XI_DeviceChanged
    super::access_notify_support::append_event_u16(
        &mut bytes,
        order,
        crate::xinput::DEVICEID_XTEST_POINTER,
    );
    super::access_notify_support::append_event_u32(&mut bytes, order, 0); // zero timestamp
    super::access_notify_support::append_event_u16(&mut bytes, order, 3); // three classes
    super::access_notify_support::append_event_u16(
        &mut bytes,
        order,
        crate::xinput::DEVICEID_XTEST_POINTER,
    );
    bytes.extend_from_slice(&[2, 0]); // XIDeviceChange, pad0
    super::access_notify_support::append_event_u16(&mut bytes, order, 0);
    super::access_notify_support::append_event_u32(&mut bytes, order, 0);
    super::access_notify_support::append_event_u32(&mut bytes, order, 0);
    bytes.extend_from_slice(&classes);
    bytes
}

fn literal_physical_pointer_classes(source_id: u16, order: ClientByteOrder) -> Vec<u8> {
    let mut classes = Vec::new();
    super::access_notify_support::append_event_u16(&mut classes, order, 1); // ButtonClass
    super::access_notify_support::append_event_u16(&mut classes, order, 10); // 10 four-byte units
    super::access_notify_support::append_event_u16(&mut classes, order, source_id);
    super::access_notify_support::append_event_u16(&mut classes, order, 7); // seven buttons
    super::access_notify_support::append_event_u16(&mut classes, order, 0); // state bitmap (4 bytes), first half
    super::access_notify_support::append_event_u16(&mut classes, order, 0); // state bitmap, second half: all zero
    for atom in 0x5d_u32..=0x63 {
        super::access_notify_support::append_event_u32(&mut classes, order, atom);
    }
    for (axis, atom, max, value) in [
        (0u16, 0x64_u32, -1_i32, 400_i32),
        (1, 0x65, -1, 300),
        (2, 0x66, 0, 0),
        (3, 0x67, 0, 0),
    ] {
        super::access_notify_support::append_event_u16(&mut classes, order, 2); // ValuatorClass
        super::access_notify_support::append_event_u16(&mut classes, order, 11); // 11 units
        super::access_notify_support::append_event_u16(&mut classes, order, source_id);
        super::access_notify_support::append_event_u16(&mut classes, order, axis);
        super::access_notify_support::append_event_u32(&mut classes, order, atom);
        super::access_notify_support::append_event_u32(&mut classes, order, u32::MAX); // minimum = -1
        super::access_notify_support::append_event_u32(&mut classes, order, 0);
        super::access_notify_support::append_event_u32(&mut classes, order, max as u32);
        super::access_notify_support::append_event_u32(&mut classes, order, 0);
        super::access_notify_support::append_event_u32(&mut classes, order, value as u32);
        super::access_notify_support::append_event_u32(&mut classes, order, 0);
        super::access_notify_support::append_event_u32(&mut classes, order, 0);
        classes.extend_from_slice(&[0; 4]); // relative mode and padding
    }
    for (number, scroll_type) in [(2u16, 1u16), (3, 2)] {
        super::access_notify_support::append_event_u16(&mut classes, order, 3); // ScrollClass
        super::access_notify_support::append_event_u16(&mut classes, order, 6); // 6 units
        super::access_notify_support::append_event_u16(&mut classes, order, source_id);
        super::access_notify_support::append_event_u16(&mut classes, order, number);
        super::access_notify_support::append_event_u16(&mut classes, order, scroll_type);
        super::access_notify_support::append_event_u16(&mut classes, order, 0);
        super::access_notify_support::append_event_u32(&mut classes, order, 0);
        super::access_notify_support::append_event_u32(&mut classes, order, 1);
        super::access_notify_support::append_event_u32(&mut classes, order, 0);
    }
    assert_eq!(classes.len(), 264, "complete seven-class pointer payload");
    classes
}

fn literal_keyboard_classes(source_id: u16, order: ClientByteOrder) -> Vec<u8> {
    let mut classes = Vec::new();
    super::access_notify_support::append_event_u16(&mut classes, order, 0); // KeyClass
    super::access_notify_support::append_event_u16(&mut classes, order, 250); // 250 units
    super::access_notify_support::append_event_u16(&mut classes, order, source_id);
    super::access_notify_support::append_event_u16(&mut classes, order, 248); // keycodes 8 through 255
    for keycode in 8_u32..=255 {
        super::access_notify_support::append_event_u32(&mut classes, order, keycode);
    }
    assert_eq!(classes.len(), 1000, "complete 248-key KeyClass payload");
    classes
}

fn literal_bootstrap_device_changed_stream(
    sequence: u16,
    pointer_id: u16,
    keyboard_id: u16,
    order: ClientByteOrder,
) -> Vec<u8> {
    let pointer_event = literal_device_changed_event(
        sequence,
        crate::xinput::DEVICEID_MASTER_POINTER,
        pointer_id,
        7,
        &literal_physical_pointer_classes(pointer_id, order),
        order,
    );
    let keyboard_event = literal_device_changed_event(
        sequence,
        crate::xinput::DEVICEID_MASTER_KEYBOARD,
        keyboard_id,
        1,
        &literal_keyboard_classes(keyboard_id, order),
        order,
    );
    [pointer_event, keyboard_event].concat()
}

fn bootstrap_device_changed_fixture() -> (NotifyFixture, u16, u16) {
    let mut fixture = NotifyFixture::new();
    let pointer_id = seed_pointer_for_t3(&mut fixture.state);
    let keyboard_ids = fixture
        .state
        .xi_register_source(&crate::core_loop::DeviceInfo {
            source_id: crate::xinput::InputSourceId(0x4b45_5942),
            enabled: true,
            resume_key: None,
            capabilities: crate::xinput::InputCapabilities {
                keyboard: true,
                pointer: false,
                touch: false,
            },
            name: "Access test keyboard".into(),
            device_node: "/dev/input/access-test-keyboard".into(),
            sysname: "access-test-keyboard".into(),
            vendor_id: 1,
            product_id: 2,
            is_touchpad: false,
            config: Default::default(),
        });
    let keyboard_id = *keyboard_ids
        .first()
        .expect("keyboard source creates one XI keyboard facet");
    assert!(
        fixture
            .state
            .xi_record_last_slave(crate::xinput::DEVICEID_MASTER_POINTER, pointer_id)
    );
    assert!(
        fixture
            .state
            .xi_record_last_slave(crate::xinput::DEVICEID_MASTER_KEYBOARD, keyboard_id)
    );
    fixture.state.start_instant = std::time::Instant::now() + Duration::from_secs(30);
    for client in [
        NOTIFY_SENDER,
        NOTIFY_RECEIVER_DENIED,
        NOTIFY_RECEIVER_ALLOWED,
    ] {
        fixture.drain(client);
    }
    (fixture, pointer_id, keyboard_id)
}

fn device_changed_description() -> EventDescription {
    EventDescription::extension(35, 137, 1)
}

#[test]
fn receive_xi_device_changed_denial_per_window() {
    // mutation: drop the subject loop variable and check W(ROOT_WINDOW)
    let build = || {
        let (mut fixture, device_id) = hotplug_fixture();
        select_device_changed(
            &mut fixture,
            NOTIFY_RECEIVER_DENIED,
            3,
            ROOT_WINDOW,
            crate::xinput::DEVICEID_MASTER_POINTER,
        );
        select_device_changed(
            &mut fixture,
            NOTIFY_RECEIVER_DENIED,
            4,
            CHILD_WINDOW,
            crate::xinput::DEVICEID_MASTER_POINTER,
        );
        (fixture, device_id)
    };
    let act = |fixture: &mut NotifyFixture,
               device_id: &u16,
               _: &std::sync::Mutex<Vec<ExpectedReceive>>| {
        assert!(
            crate::xinput::hotplug::announce_xi2_slave_switch(
                &mut fixture.state,
                crate::xinput::DEVICEID_MASTER_POINTER,
                *device_id,
            )
            .is_empty(),
            "notification denial is not a disconnect"
        );
        assert_eq!(
            fixture
                .state
                .xi_last_slave(crate::xinput::DEVICEID_MASTER_POINTER),
            Some(*device_id),
            "denying a copy leaves the master switch applied"
        );
    };
    for (name, denied_window) in [
        ("DeviceChanged root copy denied", ROOT_WINDOW),
        ("DeviceChanged child copy denied", CHILD_WINDOW),
    ] {
        let expect = |device_id: &u16| {
            let device_id = *device_id;
            let event = literal_device_changed_event(
                4,
                crate::xinput::DEVICEID_MASTER_POINTER,
                device_id,
                7,
                &literal_physical_pointer_classes(device_id, ClientByteOrder::LittleEndian),
                ClientByteOrder::LittleEndian,
            );
            let description = device_changed_description();
            let root = ExpectedReceive {
                client: NOTIFY_RECEIVER_DENIED,
                request: None,
                subject: EventSubject::Window(ROOT_WINDOW),
                events: vec![description],
            };
            let child = ExpectedReceive {
                subject: EventSubject::Window(CHILD_WINDOW),
                ..root.clone()
            };
            CaseExpect::new(
                vec![
                    Stream::unchanged(NOTIFY_SENDER, Vec::new()),
                    Stream {
                        client: NOTIFY_RECEIVER_DENIED,
                        bytes: [event.clone(), event.clone()].concat(),
                        denied: Some(event),
                    },
                    Stream::unchanged(NOTIFY_RECEIVER_ALLOWED, Vec::new()),
                ],
                vec![root.clone(), child.clone()],
                CallOrder::Multiset,
                vec![if denied_window == ROOT_WINDOW {
                    root
                } else {
                    child
                }],
            )
        };
        let case = ReceiveCase {
            name,
            build: &build,
            act: &act,
            expect: &expect,
        };
        check_receive_case(&case);
    }
}

#[test]
fn receive_xi_bootstrap_denies_one_master_only() {
    // mutation: check once for the whole buffer instead of per master
    // mutation: pass EventSubject::Global to the bootstrap Receive check
    // mutation: skip the bootstrap Receive check (the explicit-Allow count must fail)
    // mutation: return RequestOutcome::Disconnect from XISelectEvents
    let build = || {
        let (fixture, pointer_id, keyboard_id) = bootstrap_device_changed_fixture();
        (fixture, (pointer_id, keyboard_id))
    };
    let act = |fixture: &mut NotifyFixture,
               _: &(u16, u16),
               _: &std::sync::Mutex<Vec<ExpectedReceive>>| {
        select_device_changed(fixture, NOTIFY_RECEIVER_DENIED, 20, ROOT_WINDOW, 0);
        assert_eq!(
            fixture.state.clients[&NOTIFY_RECEIVER_DENIED.0]
                .xi2_masks
                .get(&(ROOT_WINDOW, 0)),
            Some(&u64::from(crate::xinput::XI2_DEVICE_CHANGED_MASK)),
            "XISelectEvents remains installed in every policy mode"
        );
    };
    // This explicit-Allow execution preserves the original mode of the
    // Handled-result probe before the runner's no-policy pass.
    let (mut explicit_allow, explicit_allow_context) = build();
    let no_denied_calls = std::sync::Mutex::new(Vec::<ExpectedReceive>::new());
    explicit_allow.install_policy(|_, _| Decision::Allow);
    super::access_notify_support::run_with_context(
        "XISelectEvents bootstrap result",
        "explicit allow-all",
        || {
            act(
                &mut explicit_allow,
                &explicit_allow_context,
                &no_denied_calls,
            )
        },
    );

    for (name, denied_count, deny_first_only) in [
        ("bootstrap denies first master copy", 1, true),
        ("bootstrap denies both master copies", 2, false),
    ] {
        let expect = |(pointer_id, keyboard_id): &(u16, u16)| {
            let event = EventDescription::extension(35, 137, 1);
            let receive = ExpectedReceive {
                client: NOTIFY_RECEIVER_DENIED,
                request: request_opcodes(137, 46),
                subject: EventSubject::Window(ROOT_WINDOW),
                events: vec![event],
            };
            let allow_bytes = literal_bootstrap_device_changed_stream(
                20,
                *pointer_id,
                *keyboard_id,
                ClientByteOrder::LittleEndian,
            );
            let denied_bytes = if denied_count == 1 {
                literal_device_changed_event(
                    20,
                    crate::xinput::DEVICEID_MASTER_KEYBOARD,
                    *keyboard_id,
                    1,
                    &literal_keyboard_classes(*keyboard_id, ClientByteOrder::LittleEndian),
                    ClientByteOrder::LittleEndian,
                )
            } else {
                Vec::new()
            };
            let mut expectations = CaseExpect::new(
                vec![
                    Stream::unchanged(NOTIFY_SENDER, Vec::new()),
                    Stream {
                        client: NOTIFY_RECEIVER_DENIED,
                        bytes: allow_bytes,
                        denied: Some(denied_bytes),
                    },
                    Stream::unchanged(NOTIFY_RECEIVER_ALLOWED, Vec::new()),
                ],
                vec![receive.clone(), receive.clone()],
                CallOrder::Total,
                vec![receive; denied_count],
            );
            expectations.deny_first_only = deny_first_only;
            expectations
        };
        let case = ReceiveCase {
            name,
            build: &build,
            act: &act,
            expect: &expect,
        };
        check_receive_case(&case);
    }
}

#[test]
fn receive_xi_device_changed_baseline_frames() {
    // Focused because it is a byte-only baseline-frame test.
    // mutation: swap the bootstrap master order
    // mutation: hard-code little-endian class encoding at X3 or X4
    let (mut changed, device_id) = hotplug_fixture();
    select_device_changed(
        &mut changed,
        NOTIFY_RECEIVER_DENIED,
        3,
        ROOT_WINDOW,
        crate::xinput::DEVICEID_MASTER_POINTER,
    );
    select_device_changed(
        &mut changed,
        NOTIFY_RECEIVER_DENIED,
        4,
        CHILD_WINDOW,
        crate::xinput::DEVICEID_MASTER_POINTER,
    );
    select_device_changed(
        &mut changed,
        NOTIFY_RECEIVER_ALLOWED,
        10,
        ROOT_WINDOW,
        crate::xinput::DEVICEID_MASTER_POINTER,
    );
    changed
        .state
        .clients
        .get_mut(&NOTIFY_RECEIVER_ALLOWED.0)
        .expect("fixture client")
        .byte_order = ClientByteOrder::BigEndian;
    assert!(
        crate::xinput::hotplug::announce_xi2_slave_switch(
            &mut changed.state,
            crate::xinput::DEVICEID_MASTER_POINTER,
            device_id,
        )
        .is_empty()
    );
    let changed_wire = changed.drain(NOTIFY_RECEIVER_DENIED);
    let pointer_classes =
        literal_physical_pointer_classes(device_id, ClientByteOrder::LittleEndian);
    let changed_event = literal_device_changed_event(
        4,
        crate::xinput::DEVICEID_MASTER_POINTER,
        device_id,
        7,
        &pointer_classes,
        ClientByteOrder::LittleEndian,
    );
    assert_eq!(
        changed_wire,
        [changed_event.clone(), changed_event].concat()
    );
    let changed_big_endian_wire = changed.drain(NOTIFY_RECEIVER_ALLOWED);
    let changed_big_endian_event = literal_device_changed_event(
        10,
        crate::xinput::DEVICEID_MASTER_POINTER,
        device_id,
        7,
        &literal_physical_pointer_classes(device_id, ClientByteOrder::BigEndian),
        ClientByteOrder::BigEndian,
    );
    assert_eq!(
        changed_big_endian_wire, changed_big_endian_event,
        "fanout serializes the full frame and class payload in the recipient byte order"
    );

    let (mut bootstrap, pointer_id, keyboard_id) = bootstrap_device_changed_fixture();
    select_device_changed(
        &mut bootstrap,
        NOTIFY_RECEIVER_DENIED,
        20,
        ROOT_WINDOW,
        0, // XIAllDevices
    );
    select_device_changed_with_order(
        &mut bootstrap,
        NOTIFY_RECEIVER_ALLOWED,
        30,
        ROOT_WINDOW,
        0, // XIAllDevices
        ClientByteOrder::BigEndian,
    );
    let bootstrap_wire = bootstrap.drain(NOTIFY_RECEIVER_DENIED);
    assert_eq!(
        bootstrap_wire,
        literal_bootstrap_device_changed_stream(
            20,
            pointer_id,
            keyboard_id,
            ClientByteOrder::LittleEndian,
        ),
        "bootstrap complete literal frames retain pointer then keyboard order"
    );
    let bootstrap_big_endian_wire = bootstrap.drain(NOTIFY_RECEIVER_ALLOWED);
    assert_eq!(
        bootstrap_big_endian_wire,
        literal_bootstrap_device_changed_stream(
            30,
            pointer_id,
            keyboard_id,
            ClientByteOrder::BigEndian,
        ),
        "bootstrap encodes both complete frames and class payloads in recipient byte order"
    );
}

#[test]
fn receive_xtest_device_changed_checks_root_selectors() {
    // mutation: leave emit_xi2_device_changed_slave_pointer on unchecked fanout_event_to_clients
    let select_root_recipients = |fixture: &mut NotifyFixture| {
        fixture.state.randr.screen_width = 800;
        fixture.state.randr.screen_height = 600;
        fixture.state.start_instant = std::time::Instant::now() + Duration::from_secs(30);

        select_device_changed(
            fixture,
            NOTIFY_RECEIVER_DENIED,
            0x31,
            ROOT_WINDOW,
            crate::xinput::DEVICEID_XTEST_POINTER,
        );
        assert!(
            fixture.drain(NOTIFY_RECEIVER_DENIED).is_empty(),
            "XISelectEvents has no requester reply or error"
        );
        assert!(crate::core_loop::client_io::failed_writers(&fixture.state.clients).is_empty());
        assert_eq!(
            fixture.state.clients[&NOTIFY_RECEIVER_DENIED.0]
                .xi2_masks
                .get(&(ROOT_WINDOW, crate::xinput::DEVICEID_XTEST_POINTER)),
            Some(&u64::from(crate::xinput::XI2_DEVICE_CHANGED_MASK)),
            "the denied recipient's ROOT selection is installed"
        );

        select_device_changed_with_order(
            fixture,
            NOTIFY_RECEIVER_ALLOWED,
            0x42,
            ROOT_WINDOW,
            crate::xinput::DEVICEID_XTEST_POINTER,
            ClientByteOrder::BigEndian,
        );
        assert!(
            fixture.drain(NOTIFY_RECEIVER_ALLOWED).is_empty(),
            "XISelectEvents has no requester reply or error"
        );
        assert!(crate::core_loop::client_io::failed_writers(&fixture.state.clients).is_empty());
        assert_eq!(
            fixture.state.clients[&NOTIFY_RECEIVER_ALLOWED.0]
                .xi2_masks
                .get(&(ROOT_WINDOW, crate::xinput::DEVICEID_XTEST_POINTER)),
            Some(&u64::from(crate::xinput::XI2_DEVICE_CHANGED_MASK)),
            "the allowed recipient's ROOT selection is installed"
        );
    };
    let expected_denied = literal_xtest_device_changed_event(0x31, ClientByteOrder::LittleEndian);
    let expected_allowed = literal_xtest_device_changed_event(0x42, ClientByteOrder::BigEndian);

    let build = || {
        let mut fixture = NotifyFixture::new();
        select_root_recipients(&mut fixture);
        (fixture, ())
    };
    let act = |fixture: &mut NotifyFixture, _: &(), _: &std::sync::Mutex<Vec<ExpectedReceive>>| {
        assert!(
            crate::core_loop::fanout::emit_xi2_device_changed_slave_pointer(
                &mut fixture.state,
                137,
            )
            .is_empty(),
            "policy denial is not a transport disconnect"
        );
    };
    let expect = |_: &()| {
        let event = EventDescription::extension(35, 137, 1);
        let receives = [NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED]
            .into_iter()
            .map(|client| ExpectedReceive {
                client,
                request: None,
                subject: EventSubject::Window(ROOT_WINDOW),
                events: vec![event],
            })
            .collect();
        let mut expectations = CaseExpect::new(
            vec![
                Stream::unchanged(NOTIFY_SENDER, Vec::new()),
                Stream {
                    client: NOTIFY_RECEIVER_DENIED,
                    bytes: expected_denied.clone(),
                    denied: Some(Vec::new()),
                },
                Stream::unchanged(NOTIFY_RECEIVER_ALLOWED, expected_allowed.clone()),
            ],
            receives,
            CallOrder::Multiset,
            vec![ExpectedReceive {
                client: NOTIFY_RECEIVER_DENIED,
                request: None,
                subject: EventSubject::Window(ROOT_WINDOW),
                events: vec![event],
            }],
        );
        expectations.after_deny = Some(Box::new(|allow, denied| {
            assert_eq!(
                allow.state.clients[&NOTIFY_RECEIVER_DENIED.0]
                    .xi2_masks
                    .get(&(ROOT_WINDOW, crate::xinput::DEVICEID_XTEST_POINTER)),
                denied.state.clients[&NOTIFY_RECEIVER_DENIED.0]
                    .xi2_masks
                    .get(&(ROOT_WINDOW, crate::xinput::DEVICEID_XTEST_POINTER))
            );
        }));
        expectations
    };
    let case = ReceiveCase {
        name: "XTEST DeviceChanged root selectors",
        build: &build,
        act: &act,
        expect: &expect,
    };
    check_receive_case(&case);
}

#[derive(Clone, Copy, Debug)]
enum XiMappingRequest {
    ChangeDeviceKeyMapping,
    SetDeviceModifierMapping,
    SetDeviceButtonMapping,
    ChangeKeyboardDevice,
}

const MAPPING_ORIGINATOR_SEQUENCE: u16 = 0x31;
const MAPPING_DENIED_SEQUENCE: u16 = 0x42;
const MAPPING_ALLOWED_SEQUENCE: u16 = 0x53;
const MAPPING_CLIENTS: [ClientId; 3] = [
    NOTIFY_SENDER,
    NOTIFY_RECEIVER_DENIED,
    NOTIFY_RECEIVER_ALLOWED,
];

fn xi1_class(device_id: u16, event_offset: u8) -> u32 {
    (u32::from(device_id) << 8) | u32::from(crate::server::XI_FIRST_EVENT + event_offset)
}

fn mapping_device(request: XiMappingRequest) -> u16 {
    match request {
        XiMappingRequest::SetDeviceButtonMapping => crate::xinput::DEVICEID_MASTER_POINTER,
        XiMappingRequest::ChangeDeviceKeyMapping
        | XiMappingRequest::SetDeviceModifierMapping
        | XiMappingRequest::ChangeKeyboardDevice => crate::xinput::DEVICEID_MASTER_KEYBOARD,
    }
}

fn mapping_event_offset(request: XiMappingRequest) -> u8 {
    match request {
        XiMappingRequest::ChangeKeyboardDevice => crate::xinput::XI_CHANGE_DEVICE_NOTIFY_OFFSET,
        XiMappingRequest::ChangeDeviceKeyMapping
        | XiMappingRequest::SetDeviceModifierMapping
        | XiMappingRequest::SetDeviceButtonMapping => {
            crate::xinput::XI_DEVICE_MAPPING_NOTIFY_OFFSET
        }
    }
}

fn mapping_event_description(request: XiMappingRequest) -> EventDescription {
    let offset = mapping_event_offset(request);
    EventDescription::extension(
        crate::server::XI_FIRST_EVENT + offset,
        137,
        u16::from(offset),
    )
}

fn mapping_request_body(request: XiMappingRequest) -> (u8, Vec<u8>) {
    match request {
        XiMappingRequest::ChangeDeviceKeyMapping => {
            let mut body = vec![crate::xinput::DEVICEID_MASTER_KEYBOARD as u8, 8, 1, 1];
            body.extend_from_slice(&0x0061_u32.to_le_bytes());
            (25, body)
        }
        XiMappingRequest::SetDeviceModifierMapping => {
            let mut body = vec![crate::xinput::DEVICEID_MASTER_KEYBOARD as u8, 1, 0, 0];
            body.extend_from_slice(&[50, 0, 37, 0, 0, 0, 0, 0]);
            (27, body)
        }
        XiMappingRequest::SetDeviceButtonMapping => (29, vec![2, 1, 0, 0, 2, 0, 0, 0]),
        XiMappingRequest::ChangeKeyboardDevice => (
            11,
            vec![crate::xinput::DEVICEID_MASTER_KEYBOARD as u8, 0, 0, 0],
        ),
    }
}

fn mapping_device_state(state: &ServerState) -> Vec<(u16, Option<u16>, bool)> {
    state
        .xi_devices
        .devices()
        .iter()
        .map(|device| {
            (
                device.id,
                state.xi_devices.attachment(device.id),
                device.enabled,
            )
        })
        .collect()
}

fn select_xi1_class(
    fixture: &mut NotifyFixture,
    client: ClientId,
    sequence: u16,
    device_id: u16,
    event_offset: u8,
) {
    let body =
        xi1_select_extension_event_body(ROOT_WINDOW.0, &[xi1_class(device_id, event_offset)]);
    let outcome = fixture
        .request(
            client,
            SequenceNumber(sequence),
            RequestHeader {
                opcode: 137,
                data: 6,
                length_units: 4,
            },
            &body,
        )
        .expect("XI SelectExtensionEvent through production dispatcher");
    assert!(matches!(outcome, RequestOutcome::Handled));
    assert!(
        fixture.state.clients[&client.0]
            .xi1_event_classes
            .contains(&xi1_class(device_id, event_offset)),
        "SelectExtensionEvent stores the selected XI1 class"
    );
    fixture.state.clients[&client.0]
        .last_sequence
        .store(sequence, Ordering::Relaxed);
    let selection_output = fixture.drain(client);
    assert!(
        selection_output.is_empty(),
        "selection has no reply or error: {selection_output:?}"
    );
    assert!(
        crate::core_loop::client_io::failed_writers(&fixture.state.clients).is_empty(),
        "selection does not fail a writer"
    );
}

fn xi_mapping_fixture(request: XiMappingRequest, select_originator: bool) -> NotifyFixture {
    let mut fixture = NotifyFixture::new();
    fixture.state.start_instant = std::time::Instant::now() + Duration::from_secs(30);
    let device_id = mapping_device(request);
    let offset = mapping_event_offset(request);
    if select_originator {
        select_xi1_class(&mut fixture, NOTIFY_SENDER, 0x11, device_id, offset);
    }
    select_xi1_class(
        &mut fixture,
        NOTIFY_RECEIVER_DENIED,
        MAPPING_DENIED_SEQUENCE,
        device_id,
        offset,
    );
    select_xi1_class(
        &mut fixture,
        NOTIFY_RECEIVER_ALLOWED,
        MAPPING_ALLOWED_SEQUENCE,
        device_id,
        offset,
    );
    fixture
        .state
        .clients
        .get_mut(&NOTIFY_RECEIVER_DENIED.0)
        .expect("denied mapping receiver")
        .byte_order = ClientByteOrder::BigEndian;
    fixture
}

fn issue_xi_mapping_request(fixture: &mut NotifyFixture, request: XiMappingRequest) {
    let device_state_before = mapping_device_state(&fixture.state);
    fixture.state.clients[&NOTIFY_SENDER.0]
        .last_sequence
        .store(MAPPING_ORIGINATOR_SEQUENCE, Ordering::Relaxed);
    let (minor, body) = mapping_request_body(request);
    let outcome = fixture
        .request(
            NOTIFY_SENDER,
            SequenceNumber(MAPPING_ORIGINATOR_SEQUENCE),
            RequestHeader {
                opcode: 137,
                data: minor,
                length_units: u32::try_from((4 + body.len()).div_ceil(4)).unwrap(),
            },
            &body,
        )
        .expect("XI mapping request through production dispatcher");
    assert!(matches!(outcome, RequestOutcome::Handled));
    assert!(
        crate::core_loop::client_io::failed_writers(&fixture.state.clients).is_empty(),
        "mapping request and Receive denial do not fail a writer"
    );
    let device_state = mapping_device_state(&fixture.state);
    if matches!(request, XiMappingRequest::ChangeKeyboardDevice) {
        assert_eq!(
            device_state, device_state_before,
            "ChangeKeyboardDevice preserves the XI device attachments and enabled state"
        );
    }
}

fn literal_xi1_mapping_notify(
    sequence: u16,
    device_id: u16,
    request: u8,
    first_keycode: u8,
    count: u8,
    order: ClientByteOrder,
) -> Vec<u8> {
    let mut event = literal_event_header(
        crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_MAPPING_NOTIFY_OFFSET,
        u8::try_from(device_id).unwrap(),
        sequence,
        order,
    );
    event.extend_from_slice(&[request, first_keycode, count, 0]);
    super::access_notify_support::append_event_u32(&mut event, order, 0);
    event.extend_from_slice(&[0; 20]);
    finish_literal_event(event)
}

fn literal_xi1_change_device_notify(
    sequence: u16,
    device_id: u16,
    request: u8,
    order: ClientByteOrder,
) -> Vec<u8> {
    let mut event = literal_event_header(
        crate::server::XI_FIRST_EVENT + crate::xinput::XI_CHANGE_DEVICE_NOTIFY_OFFSET,
        u8::try_from(device_id).unwrap(),
        sequence,
        order,
    );
    super::access_notify_support::append_event_u32(&mut event, order, 0);
    event.push(request);
    event.extend_from_slice(&[0; 23]);
    finish_literal_event(event)
}

fn literal_xi1_reply(sequence: u16, minor: u8, status: u8, order: ClientByteOrder) -> Vec<u8> {
    let mut reply = vec![0; 32];
    reply[0] = 1;
    reply[1] = minor;
    match order {
        ClientByteOrder::LittleEndian => {
            reply[2..4].copy_from_slice(&sequence.to_le_bytes());
        }
        ClientByteOrder::BigEndian => {
            reply[2..4].copy_from_slice(&sequence.to_be_bytes());
        }
    }
    reply[8] = status;
    reply
}

fn literal_mapping_stream(
    request: XiMappingRequest,
    client: ClientId,
    sequence: u16,
    order: ClientByteOrder,
    originator_selected: bool,
) -> Vec<u8> {
    let is_originator = client == NOTIFY_SENDER;
    match request {
        XiMappingRequest::ChangeDeviceKeyMapping => {
            let mut frames = literal_mapping_notify(sequence, 1, 8, 1, order);
            if !is_originator || originator_selected {
                frames.extend_from_slice(&literal_xi1_mapping_notify(
                    sequence,
                    mapping_device(request),
                    1,
                    8,
                    1,
                    order,
                ));
            }
            frames
        }
        XiMappingRequest::SetDeviceModifierMapping => {
            let mut frames = Vec::new();
            if !is_originator || originator_selected {
                frames.extend_from_slice(&literal_xi1_mapping_notify(
                    sequence,
                    mapping_device(request),
                    0,
                    0,
                    0,
                    order,
                ));
            }
            frames.extend_from_slice(&literal_mapping_notify(sequence, 0, 0, 0, order));
            if is_originator {
                frames.extend_from_slice(&literal_xi1_reply(sequence, 27, 0, order));
            }
            frames
        }
        XiMappingRequest::SetDeviceButtonMapping => {
            let mut frames = Vec::new();
            if is_originator {
                frames.extend_from_slice(&literal_xi1_reply(sequence, 0, 0, order));
                if originator_selected {
                    frames.extend_from_slice(&literal_xi1_mapping_notify(
                        sequence,
                        mapping_device(request),
                        2,
                        0,
                        0,
                        order,
                    ));
                }
            } else {
                frames.extend_from_slice(&literal_xi1_mapping_notify(
                    sequence,
                    mapping_device(request),
                    2,
                    0,
                    0,
                    order,
                ));
            }
            frames
        }
        XiMappingRequest::ChangeKeyboardDevice => {
            let mut frames = Vec::new();
            if originator_selected || !is_originator {
                frames.extend_from_slice(&literal_xi1_change_device_notify(
                    sequence,
                    mapping_device(request),
                    1,
                    order,
                ));
            }
            frames.extend_from_slice(&literal_mapping_notify(sequence, 1, 0, 0, order));
            if is_originator {
                frames.extend_from_slice(&literal_xi1_reply(sequence, 0, 0, order));
            }
            frames
        }
    }
}

fn expected_mapping_receive(
    client: ClientId,
    request: XiMappingRequest,
    opcodes: Option<RequestOpcodes>,
) -> ExpectedReceive {
    ExpectedReceive {
        client,
        request: opcodes,
        subject: EventSubject::Device(mapping_device(request)),
        events: vec![mapping_event_description(request)],
    }
}

#[test]
fn receive_xi_mapping_fanout_device_subject() {
    // mutation: use EventSubject::Global in X5
    let request = XiMappingRequest::ChangeDeviceKeyMapping;
    let build = || (xi_mapping_fixture(request, false), ());
    let act = |fixture: &mut NotifyFixture, _: &(), _: &std::sync::Mutex<Vec<ExpectedReceive>>| {
        issue_xi_mapping_request(fixture, request);
        assert_eq!(
            fixture.state.keymap_overrides.get(&8).map(Vec::as_slice),
            Some([0x61].as_slice()),
            "the keymap update remains applied"
        );
    };
    let expect = |_: &()| {
        let opcodes = request_opcodes(137, 25);
        let base = MAPPING_CLIENTS
            .into_iter()
            .map(|client| {
                Stream::unchanged(
                    client,
                    literal_mapping_stream(
                        request,
                        client,
                        match client {
                            NOTIFY_SENDER => MAPPING_ORIGINATOR_SEQUENCE,
                            NOTIFY_RECEIVER_DENIED => MAPPING_DENIED_SEQUENCE,
                            _ => MAPPING_ALLOWED_SEQUENCE,
                        },
                        if client == NOTIFY_RECEIVER_DENIED {
                            ClientByteOrder::BigEndian
                        } else {
                            ClientByteOrder::LittleEndian
                        },
                        false,
                    ),
                )
            })
            .collect::<Vec<_>>();
        let mut streams = base;
        streams[1].denied = Some(literal_mapping_notify(
            MAPPING_DENIED_SEQUENCE,
            1,
            8,
            1,
            ClientByteOrder::BigEndian,
        ));
        let receives = vec![
            expected_mapping_receive(NOTIFY_RECEIVER_DENIED, request, opcodes),
            expected_mapping_receive(NOTIFY_RECEIVER_ALLOWED, request, opcodes),
        ];
        CaseExpect::new(
            streams,
            receives.clone(),
            CallOrder::PerClient,
            vec![receives[0].clone()],
        )
    };
    check_receive_case(&ReceiveCase {
        name: "key-map Device subject fanout",
        build: &build,
        act: &act,
        expect: &expect,
    });
}

#[test]
fn receive_xi_mapping_originator_denial_keeps_reply_order() {
    // mutation: skip the check at the button-mapping site only
    // mutation: move X7b's write into the reply buffer
    // mutation: append X7a's button-map copy before the reply
    // mutation: move X5's write ahead of the core MappingNotify
    for request in [
        XiMappingRequest::ChangeDeviceKeyMapping,
        XiMappingRequest::SetDeviceModifierMapping,
        XiMappingRequest::SetDeviceButtonMapping,
    ] {
        let minor = mapping_request_body(request).0;
        let case_name = match request {
            XiMappingRequest::ChangeDeviceKeyMapping => {
                "ChangeDeviceKeyMapping originator denial preserves reply order"
            }
            XiMappingRequest::SetDeviceModifierMapping => {
                "SetDeviceModifierMapping originator denial preserves reply order"
            }
            XiMappingRequest::SetDeviceButtonMapping => {
                "SetDeviceButtonMapping originator denial preserves reply order"
            }
            XiMappingRequest::ChangeKeyboardDevice => unreachable!(),
        };
        let build = || (xi_mapping_fixture(request, true), ());
        let act =
            |fixture: &mut NotifyFixture, _: &(), _: &std::sync::Mutex<Vec<ExpectedReceive>>| {
                issue_xi_mapping_request(fixture, request);
                match request {
                    XiMappingRequest::ChangeDeviceKeyMapping => assert_eq!(
                        fixture.state.keymap_overrides.get(&8).map(Vec::as_slice),
                        Some([0x61].as_slice())
                    ),
                    XiMappingRequest::SetDeviceModifierMapping => assert_eq!(
                        fixture.state.xi1_modifier_map[&mapping_device(request)].0,
                        1
                    ),
                    XiMappingRequest::SetDeviceButtonMapping => {
                        assert_eq!(fixture.state.xi1_button_map[&mapping_device(request)][0], 2)
                    }
                    XiMappingRequest::ChangeKeyboardDevice => unreachable!(),
                }
            };
        let expect = |_: &()| {
            let opcodes = request_opcodes(137, minor);
            let streams = MAPPING_CLIENTS
                .into_iter()
                .map(|client| {
                    let (sequence, order) = match client {
                        NOTIFY_SENDER => {
                            (MAPPING_ORIGINATOR_SEQUENCE, ClientByteOrder::LittleEndian)
                        }
                        NOTIFY_RECEIVER_DENIED => {
                            (MAPPING_DENIED_SEQUENCE, ClientByteOrder::BigEndian)
                        }
                        _ => (MAPPING_ALLOWED_SEQUENCE, ClientByteOrder::LittleEndian),
                    };
                    let bytes = literal_mapping_stream(request, client, sequence, order, true);
                    let denied = if client == NOTIFY_SENDER {
                        Some(match request {
                            XiMappingRequest::ChangeDeviceKeyMapping => {
                                literal_mapping_notify(sequence, 1, 8, 1, order)
                            }
                            XiMappingRequest::SetDeviceModifierMapping => [
                                literal_mapping_notify(sequence, 0, 0, 0, order),
                                literal_xi1_reply(sequence, 27, 0, order),
                            ]
                            .concat(),
                            XiMappingRequest::SetDeviceButtonMapping => {
                                literal_xi1_reply(sequence, 0, 0, order)
                            }
                            XiMappingRequest::ChangeKeyboardDevice => unreachable!(),
                        })
                    } else {
                        None
                    };
                    Stream {
                        client,
                        bytes,
                        denied,
                    }
                })
                .collect::<Vec<_>>();
            let receives = MAPPING_CLIENTS
                .into_iter()
                .map(|client| expected_mapping_receive(client, request, opcodes))
                .collect::<Vec<_>>();
            let mut expectations = CaseExpect::new(
                streams,
                receives.clone(),
                CallOrder::PerClient,
                vec![receives[0].clone()],
            );
            match request {
                XiMappingRequest::SetDeviceModifierMapping => {
                    expectations.after_deny = Some(Box::new(|allow, denied| {
                        assert_eq!(
                            allow.state.xi1_modifier_map, denied.state.xi1_modifier_map,
                            "allow-all and deny preserve the complete XI1 modifier map"
                        );
                    }));
                }
                XiMappingRequest::SetDeviceButtonMapping => {
                    expectations.after_deny = Some(Box::new(|allow, denied| {
                        assert_eq!(
                            allow.state.xi1_button_map, denied.state.xi1_button_map,
                            "allow-all and deny preserve the complete XI1 button map"
                        );
                    }));
                }
                XiMappingRequest::ChangeDeviceKeyMapping
                | XiMappingRequest::ChangeKeyboardDevice => {}
            }
            expectations
        };
        check_receive_case(&ReceiveCase {
            name: case_name,
            build: &build,
            act: &act,
            expect: &expect,
        });
    }
}

#[test]
fn receive_change_device_notify_originator_denial_keeps_core_mapping() {
    // mutation: leave X6 on the unchecked fanout
    // mutation: route the core MappingNotify through the checked fanout
    // mutation: give X6 the request's sequence instead of the recipient's
    let request = XiMappingRequest::ChangeKeyboardDevice;
    let build = || {
        let fixture = xi_mapping_fixture(request, true);
        let before = mapping_device_state(&fixture.state);
        (fixture, before)
    };
    let act = |fixture: &mut NotifyFixture,
               _: &Vec<(u16, Option<u16>, bool)>,
               _: &std::sync::Mutex<Vec<ExpectedReceive>>| {
        issue_xi_mapping_request(fixture, request);
    };
    let expect = |before: &Vec<(u16, Option<u16>, bool)>| {
        let state_before = before.clone();
        let opcodes = request_opcodes(137, 11);
        let mut streams = MAPPING_CLIENTS
            .into_iter()
            .map(|client| {
                let (sequence, order) = match client {
                    NOTIFY_SENDER => (MAPPING_ORIGINATOR_SEQUENCE, ClientByteOrder::LittleEndian),
                    NOTIFY_RECEIVER_DENIED => (MAPPING_DENIED_SEQUENCE, ClientByteOrder::BigEndian),
                    _ => (MAPPING_ALLOWED_SEQUENCE, ClientByteOrder::LittleEndian),
                };
                Stream {
                    client,
                    bytes: literal_mapping_stream(request, client, sequence, order, true),
                    denied: None,
                }
            })
            .collect::<Vec<_>>();
        streams[0].denied = Some(
            [
                literal_mapping_notify(
                    MAPPING_ORIGINATOR_SEQUENCE,
                    1,
                    0,
                    0,
                    ClientByteOrder::LittleEndian,
                ),
                literal_xi1_reply(
                    MAPPING_ORIGINATOR_SEQUENCE,
                    0,
                    0,
                    ClientByteOrder::LittleEndian,
                ),
            ]
            .concat(),
        );
        streams[1].denied = Some(literal_mapping_notify(
            MAPPING_DENIED_SEQUENCE,
            1,
            0,
            0,
            ClientByteOrder::BigEndian,
        ));
        let receives = MAPPING_CLIENTS
            .into_iter()
            .map(|client| expected_mapping_receive(client, request, opcodes))
            .collect::<Vec<_>>();
        let mut expectations = CaseExpect::new(
            streams,
            receives[..].to_vec(),
            CallOrder::PerClient,
            receives[..2].to_vec(),
        );
        expectations.after_deny = Some(Box::new(move |allow, denied| {
            assert_eq!(
                mapping_device_state(&allow.state),
                mapping_device_state(&denied.state),
                "allow-all and deny preserve the same complete device snapshot"
            );
            assert_eq!(
                mapping_device_state(&denied.state),
                state_before,
                "ChangeKeyboardDevice leaves the complete device snapshot unchanged"
            );
        }));
        expectations
    };
    check_receive_case(&ReceiveCase {
        name: "ChangeKeyboardDevice denial preserves core MappingNotify and device snapshot",
        build: &build,
        act: &act,
        expect: &expect,
    });
}

#[test]
fn receive_xi_mapping_after_xkb_change() {
    // mutation: leave X5 on the unchecked fanout
    let request = XiMappingRequest::ChangeDeviceKeyMapping;
    let build = || (xi_mapping_fixture(request, false), ());
    let act = |fixture: &mut NotifyFixture, _: &(), _: &std::sync::Mutex<Vec<ExpectedReceive>>| {
        let keymap_before = fixture.state.keymap_overrides.clone();
        crate::core_loop::xkb_select::send_legacy_map_notify(
            &mut fixture.state,
            crate::core_loop::xkb_select::LegacyCause::MapNotify,
            0x0002,
            8,
            1,
        );
        assert!(
            crate::core_loop::client_io::failed_writers(&fixture.state.clients).is_empty(),
            "server XKB notification does not fail a writer"
        );
        assert_eq!(fixture.state.keymap_overrides, keymap_before);
    };
    let expect = |_: &()| {
        let sender_core_mapping = literal_mapping_notify(0, 1, 8, 1, ClientByteOrder::LittleEndian);
        let denied_full = [
            literal_mapping_notify(MAPPING_DENIED_SEQUENCE, 1, 8, 1, ClientByteOrder::BigEndian),
            literal_xi1_mapping_notify(
                MAPPING_DENIED_SEQUENCE,
                mapping_device(request),
                1,
                8,
                1,
                ClientByteOrder::BigEndian,
            ),
        ]
        .concat();
        let allowed_full = [
            literal_mapping_notify(
                MAPPING_ALLOWED_SEQUENCE,
                1,
                8,
                1,
                ClientByteOrder::LittleEndian,
            ),
            literal_xi1_mapping_notify(
                MAPPING_ALLOWED_SEQUENCE,
                mapping_device(request),
                1,
                8,
                1,
                ClientByteOrder::LittleEndian,
            ),
        ]
        .concat();
        let receives = [NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED]
            .into_iter()
            .map(|client| expected_mapping_receive(client, request, None))
            .collect::<Vec<_>>();
        let mut expectations = CaseExpect::new(
            vec![
                Stream::unchanged(NOTIFY_SENDER, sender_core_mapping),
                Stream {
                    client: NOTIFY_RECEIVER_DENIED,
                    bytes: denied_full,
                    denied: Some(literal_mapping_notify(
                        MAPPING_DENIED_SEQUENCE,
                        1,
                        8,
                        1,
                        ClientByteOrder::BigEndian,
                    )),
                },
                Stream::unchanged(NOTIFY_RECEIVER_ALLOWED, allowed_full),
            ],
            receives.clone(),
            CallOrder::PerClient,
            vec![receives[0].clone()],
        );
        expectations.deny_before_allow = true;
        expectations
    };
    check_receive_case(&ReceiveCase {
        name: "server XKB MappingNotify Receive check",
        build: &build,
        act: &act,
        expect: &expect,
    });
}

const XI_PROPERTY_DEVICE_ID: u16 = crate::xinput::DEVICEID_XTEST_POINTER;
const XI_PROPERTY_REQUEST_SEQUENCE: u16 = 0x31;
const XI_PROPERTY_DENIED_SEQUENCE: u16 = 0x42;
const XI_PROPERTY_ALLOWED_SEQUENCE: u16 = 0x53;
const XI_PROPERTY_VALUE: [u8; 1] = [0x5a];

fn select_xi2_property_event(fixture: &mut NotifyFixture, client: ClientId, sequence: u16) {
    let body = super::xi2_grabs::xi_dynamic_select_events_body(
        ROOT_WINDOW.0,
        0, // XIAllDevices
        crate::xinput::XI2_PROPERTY_EVENT_MASK,
    );
    let outcome = fixture
        .request(
            client,
            SequenceNumber(sequence),
            RequestHeader {
                opcode: 137,
                data: 46,
                length_units: 5,
            },
            &body,
        )
        .expect("XISelectEvents through production dispatcher");
    assert!(matches!(outcome, RequestOutcome::Handled));
    assert_eq!(
        fixture.state.clients[&client.0]
            .xi2_masks
            .get(&(ROOT_WINDOW, 0)),
        Some(&u64::from(crate::xinput::XI2_PROPERTY_EVENT_MASK)),
        "XIAllDevices PropertyEvent selection is stored"
    );
    assert!(
        fixture.drain(client).is_empty(),
        "XISelectEvents has no reply or error"
    );
    assert!(crate::core_loop::client_io::failed_writers(&fixture.state.clients).is_empty());
}

fn xi_property_event_descriptions() -> [EventDescription; 2] {
    [
        EventDescription::extension(35, 137, 12),
        EventDescription::extension(
            crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_PROPERTY_NOTIFY_OFFSET,
            137,
            u16::from(crate::xinput::XI_DEVICE_PROPERTY_NOTIFY_OFFSET),
        ),
    ]
}

fn xi_property_notification_fixture(select_allowed: bool) -> (NotifyFixture, AtomId) {
    let mut fixture = NotifyFixture::new();
    fixture.state.start_instant = std::time::Instant::now() + Duration::from_secs(30);
    fixture
        .state
        .clients
        .get_mut(&NOTIFY_RECEIVER_DENIED.0)
        .unwrap()
        .byte_order = ClientByteOrder::BigEndian;
    fixture
        .state
        .clients
        .get_mut(&NOTIFY_RECEIVER_ALLOWED.0)
        .unwrap()
        .byte_order = ClientByteOrder::LittleEndian;
    let property = AtomId(
        fixture
            .state
            .atoms
            .intern("yserver-test-xi-property-notify", false)
            .0,
    );

    for (client, sequence) in [
        (NOTIFY_RECEIVER_DENIED, 0x11),
        (NOTIFY_RECEIVER_ALLOWED, 0x22),
    ] {
        if client == NOTIFY_RECEIVER_ALLOWED && !select_allowed {
            continue;
        }
        select_xi2_property_event(&mut fixture, client, sequence);
        select_xi1_class(
            &mut fixture,
            client,
            sequence + 1,
            XI_PROPERTY_DEVICE_ID,
            crate::xinput::XI_DEVICE_PROPERTY_NOTIFY_OFFSET,
        );
    }
    fixture.state.clients[&NOTIFY_RECEIVER_DENIED.0]
        .last_sequence
        .store(XI_PROPERTY_DENIED_SEQUENCE, Ordering::Relaxed);
    fixture.state.clients[&NOTIFY_RECEIVER_ALLOWED.0]
        .last_sequence
        .store(XI_PROPERTY_ALLOWED_SEQUENCE, Ordering::Relaxed);
    assert!(fixture.drain(NOTIFY_SENDER).is_empty());
    (fixture, property)
}

fn literal_xi2_property_event(
    sequence: u16,
    device_id: u16,
    property: AtomId,
    what: u8,
    order: ClientByteOrder,
) -> Vec<u8> {
    let mut bytes = vec![35, 137]; // GenericEvent, XI extension opcode
    super::access_notify_support::append_event_u16(&mut bytes, order, sequence);
    super::access_notify_support::append_event_u32(&mut bytes, order, 0); // no payload
    super::access_notify_support::append_event_u16(&mut bytes, order, 12); // XI_PropertyEvent
    super::access_notify_support::append_event_u16(&mut bytes, order, device_id);
    super::access_notify_support::append_event_u32(&mut bytes, order, 0); // pinned timestamp
    super::access_notify_support::append_event_u32(&mut bytes, order, property.0);
    bytes.push(what);
    bytes.extend_from_slice(&[0; 11]);
    bytes
}

fn literal_xi1_property_event(
    sequence: u16,
    device_id: u16,
    property: AtomId,
    order: ClientByteOrder,
) -> Vec<u8> {
    let mut bytes = vec![
        crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_PROPERTY_NOTIFY_OFFSET,
        0, // PropertyNewValue
    ];
    super::access_notify_support::append_event_u16(&mut bytes, order, sequence);
    super::access_notify_support::append_event_u32(&mut bytes, order, 0); // pinned timestamp
    super::access_notify_support::append_event_u32(&mut bytes, order, property.0);
    bytes.extend_from_slice(&[0; 19]);
    bytes.push(u8::try_from(device_id).unwrap());
    bytes
}

fn literal_xi_property_stream(
    sequence: u16,
    device_id: u16,
    property: AtomId,
    order: ClientByteOrder,
) -> Vec<u8> {
    [
        literal_xi2_property_event(sequence, device_id, property, 1, order), // PropertyCreated
        literal_xi1_property_event(sequence, device_id, property, order),
    ]
    .concat()
}

fn issue_xi_property_change(fixture: &mut NotifyFixture, property: AtomId) {
    let body = xi2_change_property_body(
        XI_PROPERTY_DEVICE_ID,
        crate::xinput::XI_PROP_MODE_REPLACE,
        8,
        property.0,
        crate::xinput::XA_INTEGER.0,
        &XI_PROPERTY_VALUE,
    );
    let outcome = fixture
        .request(
            NOTIFY_SENDER,
            SequenceNumber(XI_PROPERTY_REQUEST_SEQUENCE),
            RequestHeader {
                opcode: 137,
                data: 57,
                length_units: u32::try_from((4 + body.len()).div_ceil(4)).unwrap(),
            },
            &body,
        )
        .expect("XIChangeProperty through production dispatcher");
    assert!(matches!(outcome, RequestOutcome::Handled));
    assert!(
        fixture.drain(NOTIFY_SENDER).is_empty(),
        "XIChangeProperty has no reply or error"
    );
    assert_eq!(
        fixture
            .state
            .xi_devices
            .device(XI_PROPERTY_DEVICE_ID)
            .unwrap()
            .properties[&property]
            .data
            .as_slice(),
        XI_PROPERTY_VALUE.as_slice(),
        "the property write remains applied"
    );
    assert!(crate::core_loop::client_io::failed_writers(&fixture.state.clients).is_empty());
    assert!(
        fixture
            .state
            .clients
            .contains_key(&NOTIFY_RECEIVER_DENIED.0)
    );
    assert!(
        fixture
            .state
            .clients
            .contains_key(&NOTIFY_RECEIVER_ALLOWED.0)
    );
}

#[test]
fn receive_xi_property_forms_checked_independently() {
    // mutation: skip the check on the XI1 path (X10)
    // mutation: swap the X9 and X10 blocks
    let descriptions = xi_property_event_descriptions();
    for (name, denied_form) in [
        ("XI2 PropertyEvent denial", 0_usize),
        ("XI1 DevicePropertyNotify denial", 1_usize),
    ] {
        let build = || xi_property_notification_fixture(true);
        let act = |fixture: &mut NotifyFixture,
                   property: &AtomId,
                   _: &std::sync::Mutex<Vec<ExpectedReceive>>| {
            issue_xi_property_change(fixture, *property);
        };
        let expect = |property: &AtomId| {
            let receives = [NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED]
                .into_iter()
                .flat_map(|client| {
                    descriptions.map(|description| ExpectedReceive {
                        client,
                        request: request_opcodes(137, 57),
                        subject: EventSubject::Device(XI_PROPERTY_DEVICE_ID),
                        events: vec![description],
                    })
                })
                .collect::<Vec<_>>();
            let mut streams = vec![
                Stream::unchanged(NOTIFY_SENDER, Vec::new()),
                Stream {
                    client: NOTIFY_RECEIVER_DENIED,
                    bytes: literal_xi_property_stream(
                        XI_PROPERTY_DENIED_SEQUENCE,
                        XI_PROPERTY_DEVICE_ID,
                        *property,
                        ClientByteOrder::BigEndian,
                    ),
                    denied: None,
                },
                Stream::unchanged(
                    NOTIFY_RECEIVER_ALLOWED,
                    literal_xi_property_stream(
                        XI_PROPERTY_ALLOWED_SEQUENCE,
                        XI_PROPERTY_DEVICE_ID,
                        *property,
                        ClientByteOrder::LittleEndian,
                    ),
                ),
            ];
            streams[1].denied = Some(if denied_form == 0 {
                literal_xi1_property_event(
                    XI_PROPERTY_DENIED_SEQUENCE,
                    XI_PROPERTY_DEVICE_ID,
                    *property,
                    ClientByteOrder::BigEndian,
                )
            } else {
                literal_xi2_property_event(
                    XI_PROPERTY_DENIED_SEQUENCE,
                    XI_PROPERTY_DEVICE_ID,
                    *property,
                    1,
                    ClientByteOrder::BigEndian,
                )
            });
            let mut expectations = CaseExpect::new(
                streams,
                receives.clone(),
                CallOrder::Multiset,
                vec![receives[denied_form].clone()],
            );
            expectations.deny_before_allow = true;
            expectations.trace_before_deny_streams = true;
            expectations
        };
        check_receive_case(&ReceiveCase {
            name,
            build: &build,
            act: &act,
            expect: &expect,
        });
    }
}

#[test]
fn receive_xi_property_denial_is_not_disconnect() {
    // mutation: push denied clients into the returned vector
    let descriptions = xi_property_event_descriptions();
    let build = || {
        let (mut fixture, property) = xi_property_notification_fixture(false);
        super::seed_one_prop(
            &mut fixture.state,
            property.0,
            8,
            XI_PROPERTY_VALUE.to_vec(),
        );
        (fixture, property)
    };
    let act = |fixture: &mut NotifyFixture,
               property: &AtomId,
               _: &std::sync::Mutex<Vec<ExpectedReceive>>| {
        let disconnected = crate::core_loop::process_request::props::emit_property_change(
            &mut fixture.state,
            XI_PROPERTY_DEVICE_ID,
            *property,
            crate::xinput::PropWhat::Modified,
        );
        assert!(disconnected.is_empty(), "policy denial is not a disconnect");
        assert_eq!(
            fixture
                .state
                .xi_devices
                .device(XI_PROPERTY_DEVICE_ID)
                .unwrap()
                .properties[property]
                .data,
            XI_PROPERTY_VALUE,
            "direct emission preserves the applied property"
        );
    };
    let expect = |property: &AtomId| {
        let receives = descriptions
            .into_iter()
            .map(|description| ExpectedReceive {
                client: NOTIFY_RECEIVER_DENIED,
                request: None,
                subject: EventSubject::Device(XI_PROPERTY_DEVICE_ID),
                events: vec![description],
            })
            .collect::<Vec<_>>();
        CaseExpect::new(
            vec![
                Stream::unchanged(NOTIFY_SENDER, Vec::new()),
                Stream {
                    client: NOTIFY_RECEIVER_DENIED,
                    bytes: [
                        literal_xi2_property_event(
                            XI_PROPERTY_DENIED_SEQUENCE,
                            XI_PROPERTY_DEVICE_ID,
                            *property,
                            2,
                            ClientByteOrder::BigEndian,
                        ),
                        literal_xi1_property_event(
                            XI_PROPERTY_DENIED_SEQUENCE,
                            XI_PROPERTY_DEVICE_ID,
                            *property,
                            ClientByteOrder::BigEndian,
                        ),
                    ]
                    .concat(),
                    denied: Some(Vec::new()),
                },
                Stream::unchanged(NOTIFY_RECEIVER_ALLOWED, Vec::new()),
            ],
            receives.clone(),
            CallOrder::Multiset,
            receives,
        )
    };
    check_receive_case(&ReceiveCase {
        name: "property denial is not a disconnect",
        build: &build,
        act: &act,
        expect: &expect,
    });
}
