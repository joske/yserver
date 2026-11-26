use super::{
    access_notify_support::{
        NOTIFY_RECEIVER_ALLOWED, NOTIFY_RECEIVER_DENIED, NOTIFY_SENDER, NotifyFixture,
        finish_literal_event, literal_event_header,
    },
    extensions::{ExtensionHook, RecordedExtensionCheck},
    xi_hotplug::{
        xi_hotplug_create_child, xi_hotplug_select_hierarchy, xi_hotplug_select_presence,
        xi_hotplug_set_enabled,
    },
    *,
};
use crate::{
    access::{Decision, DenyError, EventDescription, EventSubject, RequestOpcodes},
    core_loop::process_request::RequestOutcome,
    resources::ROOT_WINDOW,
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use yserver_protocol::x11::{ClientId, ResourceId};

const CHILD_WINDOW: ResourceId = ResourceId(0x0071_0001);
const DENY: DenyError = DenyError {
    code: yserver_protocol::x11::error::BAD_ACCESS,
    value: 0,
};

fn hotplug_fixture() -> (NotifyFixture, u16) {
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

fn disable_device(fixture: &mut NotifyFixture, device_id: u16) {
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

fn literal_hierarchy_disabled(sequence: u16) -> Vec<u8> {
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

fn receive_checks(
    calls: &Arc<std::sync::Mutex<Vec<RecordedExtensionCheck>>>,
) -> Vec<RecordedExtensionCheck> {
    calls
        .lock()
        .unwrap()
        .iter()
        .filter(|call| call.hook == ExtensionHook::Receive)
        .cloned()
        .collect()
}

fn select_hierarchy_on_root_and_child(fixture: &mut NotifyFixture, client: ClientId) {
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
    let (mut allowed, allowed_device) = hotplug_fixture();
    select_hierarchy_on_root_and_child(&mut allowed, NOTIFY_RECEIVER_DENIED);
    let allowed_calls = allowed.install_policy(|_, _| Decision::Allow);
    disable_device(&mut allowed, allowed_device);
    assert!(allowed.drain(NOTIFY_SENDER).is_empty());
    let all_allow = allowed.drain(NOTIFY_RECEIVER_DENIED);
    assert_eq!(
        all_allow,
        [literal_hierarchy_disabled(4), literal_hierarchy_disabled(4)].concat()
    );
    let allowed_receives = receive_checks(&allowed_calls);
    assert_eq!(allowed_receives.len(), 2);
    assert!(
        allowed_receives
            .iter()
            .any(|call| { call.receive == Some(EventSubject::Window(ROOT_WINDOW)) })
    );
    assert!(
        allowed_receives
            .iter()
            .any(|call| { call.receive == Some(EventSubject::Window(CHILD_WINDOW)) })
    );

    let (mut denied, denied_device) = hotplug_fixture();
    select_hierarchy_on_root_and_child(&mut denied, NOTIFY_RECEIVER_DENIED);
    let calls = denied.install_policy(|_, call| {
        if call.hook == ExtensionHook::Receive
            && call.client == NOTIFY_RECEIVER_DENIED
            && call.receive == Some(EventSubject::Window(CHILD_WINDOW))
            && call.events.as_ref().is_some_and(|events| {
                events.as_slice() == [EventDescription::extension(35, 137, 11)]
            })
        {
            Decision::Deny(DENY)
        } else {
            Decision::Allow
        }
    });
    disable_device(&mut denied, denied_device);
    assert!(
        crate::core_loop::client_io::failed_writers(&denied.state.clients).is_empty(),
        "denying the hierarchy copy does not queue a writer for disconnect"
    );
    assert!(denied.drain(NOTIFY_SENDER).is_empty());
    let received = denied.drain(NOTIFY_RECEIVER_DENIED);
    assert_eq!(
        received,
        literal_hierarchy_disabled(4),
        "only the root-selected copy remains"
    );
    assert!(
        !denied
            .state
            .xi_devices
            .device(denied_device)
            .unwrap()
            .enabled,
        "denying the notification leaves the device disabled"
    );
    assert_eq!(
        denied
            .state
            .xi_devices
            .device(denied_device)
            .unwrap()
            .properties[&denied.state.xi_device_enabled_atom]
            .data,
        [0],
        "denying the notification leaves Device Enabled at zero"
    );
    assert_eq!(
        denied.state.xi_devices.attachment(denied_device),
        None,
        "the disabled live facet is floated after its hierarchy snapshot is published"
    );
    assert!(denied.state.clients.contains_key(&NOTIFY_RECEIVER_DENIED.0));

    let receives = receive_checks(&calls);
    assert_eq!(receives.len(), 2);
    assert!(
        receives
            .iter()
            .any(|call| call.receive == Some(EventSubject::Window(ROOT_WINDOW)))
    );
    assert!(
        receives
            .iter()
            .any(|call| call.receive == Some(EventSubject::Window(CHILD_WINDOW)))
    );
    assert!(
        receives
            .iter()
            .all(|call| { call.events == Some(vec![EventDescription::extension(35, 137, 11)]) })
    );
}

#[test]
fn receive_xi_presence_denial_keeps_other_client() {
    // mutation: skip the receive check in X1 (use fanout_event_to_clients)
    let (mut allowed, allowed_device) = hotplug_fixture();
    xi_hotplug_select_presence(
        &mut allowed.state,
        &mut allowed.backend,
        NOTIFY_RECEIVER_DENIED.0,
        3,
        ROOT_WINDOW,
    );
    xi_hotplug_select_presence(
        &mut allowed.state,
        &mut allowed.backend,
        NOTIFY_RECEIVER_ALLOWED.0,
        3,
        CHILD_WINDOW,
    );
    let allowed_calls = allowed.install_policy(|_, _| Decision::Allow);
    disable_device(&mut allowed, allowed_device);
    assert!(allowed.drain(NOTIFY_SENDER).is_empty());
    let all_allow_denied = allowed.drain(NOTIFY_RECEIVER_DENIED);
    let all_allow_other = allowed.drain(NOTIFY_RECEIVER_ALLOWED);
    assert_eq!(
        all_allow_denied,
        literal_presence_disabled(3, allowed_device)
    );
    assert_eq!(
        all_allow_other,
        literal_presence_disabled(3, allowed_device)
    );
    let allowed_receives = receive_checks(&allowed_calls);
    assert_eq!(allowed_receives.len(), 2);
    assert!(allowed_receives.iter().any(|call| {
        call.client == NOTIFY_RECEIVER_DENIED
            && call.receive == Some(EventSubject::Window(ROOT_WINDOW))
    }));
    assert!(allowed_receives.iter().any(|call| {
        call.client == NOTIFY_RECEIVER_ALLOWED
            && call.receive == Some(EventSubject::Window(CHILD_WINDOW))
    }));

    let (mut denied, denied_device) = hotplug_fixture();
    xi_hotplug_select_presence(
        &mut denied.state,
        &mut denied.backend,
        NOTIFY_RECEIVER_DENIED.0,
        3,
        ROOT_WINDOW,
    );
    xi_hotplug_select_presence(
        &mut denied.state,
        &mut denied.backend,
        NOTIFY_RECEIVER_ALLOWED.0,
        3,
        CHILD_WINDOW,
    );
    let calls = denied.install_policy(|_, call| {
        if call.hook == ExtensionHook::Receive
            && call.client == NOTIFY_RECEIVER_DENIED
            && call.receive == Some(EventSubject::Window(ROOT_WINDOW))
            && call.events.as_ref().is_some_and(|events| {
                events.as_slice()
                    == [EventDescription::extension(
                        crate::server::XI_FIRST_EVENT + 15,
                        137,
                        15,
                    )]
            })
        {
            Decision::Deny(DENY)
        } else {
            Decision::Allow
        }
    });
    disable_device(&mut denied, denied_device);
    assert!(
        crate::core_loop::client_io::failed_writers(&denied.state.clients).is_empty(),
        "denying the presence copy does not queue a writer for disconnect"
    );
    assert!(denied.drain(NOTIFY_SENDER).is_empty());
    let denied_bytes = denied.drain(NOTIFY_RECEIVER_DENIED);
    let allowed_bytes = denied.drain(NOTIFY_RECEIVER_ALLOWED);
    assert!(
        denied_bytes.is_empty(),
        "denied client gets no event or error"
    );
    assert_eq!(allowed_bytes, all_allow_other);
    assert!(
        !denied
            .state
            .xi_devices
            .device(denied_device)
            .unwrap()
            .enabled,
        "denying one copy does not undo the device disable"
    );
    assert_eq!(
        denied
            .state
            .xi_devices
            .device(denied_device)
            .unwrap()
            .properties[&denied.state.xi_device_enabled_atom]
            .data,
        [0]
    );
    assert_eq!(
        denied.state.xi_devices.attachment(denied_device),
        None,
        "the disabled live facet is floated after its presence publication"
    );
    assert!(denied.state.clients.contains_key(&NOTIFY_RECEIVER_DENIED.0));
    assert!(
        denied
            .state
            .clients
            .contains_key(&NOTIFY_RECEIVER_ALLOWED.0)
    );

    let receives = receive_checks(&calls);
    assert_eq!(receives.len(), 2);
    assert!(receives.iter().any(|call| {
        call.client == NOTIFY_RECEIVER_DENIED
            && call.receive == Some(EventSubject::Window(ROOT_WINDOW))
    }));
    assert!(receives.iter().any(|call| {
        call.client == NOTIFY_RECEIVER_ALLOWED
            && call.receive == Some(EventSubject::Window(CHILD_WINDOW))
    }));
    assert!(receives.iter().all(|call| {
        call.events
            == Some(vec![EventDescription::extension(
                crate::server::XI_FIRST_EVENT + 15,
                137,
                15,
            )])
    }));
}

#[test]
fn receive_xi_presence_from_server_work_has_no_request() {
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

fn is_device_changed_receive(call: &RecordedExtensionCheck) -> bool {
    call.hook == ExtensionHook::Receive
        && call
            .events
            .as_ref()
            .is_some_and(|events| events.as_slice() == [EventDescription::extension(35, 137, 1)])
}

#[test]
fn receive_xi_device_changed_denial_per_window() {
    // mutation: drop the subject loop variable and check W(ROOT_WINDOW)
    let (mut allowed, device_id) = hotplug_fixture();
    select_device_changed(
        &mut allowed,
        NOTIFY_RECEIVER_DENIED,
        3,
        ROOT_WINDOW,
        crate::xinput::DEVICEID_MASTER_POINTER,
    );
    select_device_changed(
        &mut allowed,
        NOTIFY_RECEIVER_DENIED,
        4,
        CHILD_WINDOW,
        crate::xinput::DEVICEID_MASTER_POINTER,
    );
    let allowed_calls = allowed.install_policy(|_, _| Decision::Allow);
    assert!(
        crate::xinput::hotplug::announce_xi2_slave_switch(
            &mut allowed.state,
            crate::xinput::DEVICEID_MASTER_POINTER,
            device_id,
        )
        .is_empty()
    );
    assert!(allowed.drain(NOTIFY_SENDER).is_empty());
    let all_allow_wire = allowed.drain(NOTIFY_RECEIVER_DENIED);
    let event = literal_device_changed_event(
        4,
        crate::xinput::DEVICEID_MASTER_POINTER,
        device_id,
        7,
        &literal_physical_pointer_classes(device_id, ClientByteOrder::LittleEndian),
        ClientByteOrder::LittleEndian,
    );
    assert_eq!(
        all_allow_wire,
        [event.clone(), event.clone()].concat(),
        "explicit-Allow control emits two complete literal frames"
    );
    assert_eq!(
        allowed
            .state
            .xi_last_slave(crate::xinput::DEVICEID_MASTER_POINTER),
        Some(device_id)
    );
    let allowed_receives = receive_checks(&allowed_calls);
    assert_eq!(allowed_receives.len(), 2);
    assert!(allowed_receives.iter().any(|call| {
        call.receive == Some(EventSubject::Window(ROOT_WINDOW))
            && call.request.is_none()
            && call.events == Some(vec![device_changed_description()])
    }));
    assert!(allowed_receives.iter().any(|call| {
        call.receive == Some(EventSubject::Window(CHILD_WINDOW))
            && call.request.is_none()
            && call.events == Some(vec![device_changed_description()])
    }));

    let (mut denied, device_id) = hotplug_fixture();
    select_device_changed(
        &mut denied,
        NOTIFY_RECEIVER_DENIED,
        3,
        ROOT_WINDOW,
        crate::xinput::DEVICEID_MASTER_POINTER,
    );
    select_device_changed(
        &mut denied,
        NOTIFY_RECEIVER_DENIED,
        4,
        CHILD_WINDOW,
        crate::xinput::DEVICEID_MASTER_POINTER,
    );
    let calls = denied.install_policy(|_, call| {
        if call.hook == ExtensionHook::Receive
            && call.client == NOTIFY_RECEIVER_DENIED
            && call.receive == Some(EventSubject::Window(CHILD_WINDOW))
            && call.events.as_ref().is_some_and(|events| {
                events.as_slice() == [EventDescription::extension(35, 137, 1)]
            })
        {
            Decision::Deny(DENY)
        } else {
            Decision::Allow
        }
    });
    assert!(
        crate::xinput::hotplug::announce_xi2_slave_switch(
            &mut denied.state,
            crate::xinput::DEVICEID_MASTER_POINTER,
            device_id,
        )
        .is_empty()
    );
    let expected_event = literal_device_changed_event(
        4,
        crate::xinput::DEVICEID_MASTER_POINTER,
        device_id,
        7,
        &literal_physical_pointer_classes(device_id, ClientByteOrder::LittleEndian),
        ClientByteOrder::LittleEndian,
    );
    assert!(
        crate::core_loop::client_io::failed_writers(&denied.state.clients).is_empty(),
        "denying one copy does not queue a writer for disconnect"
    );
    assert!(denied.drain(NOTIFY_SENDER).is_empty());
    let received_wire = denied.drain(NOTIFY_RECEIVER_DENIED);
    assert_eq!(
        received_wire, expected_event,
        "only the root-selected literal frame remains"
    );
    assert_eq!(
        denied
            .state
            .xi_last_slave(crate::xinput::DEVICEID_MASTER_POINTER),
        Some(device_id),
        "denying a copy leaves the master switch applied"
    );
    assert!(denied.state.clients.contains_key(&NOTIFY_RECEIVER_DENIED.0));
    let denied_receives = receive_checks(&calls)
        .into_iter()
        .filter(is_device_changed_receive)
        .collect::<Vec<_>>();
    assert_eq!(denied_receives.len(), 2);
    assert!(denied_receives.iter().any(|call| {
        call.receive == Some(EventSubject::Window(ROOT_WINDOW))
            && call.events == Some(vec![EventDescription::extension(35, 137, 1)])
            && call.request.is_none()
    }));
    assert!(denied_receives.iter().any(|call| {
        call.receive == Some(EventSubject::Window(CHILD_WINDOW))
            && call.events == Some(vec![EventDescription::extension(35, 137, 1)])
            && call.request.is_none()
    }));
}

#[test]
fn receive_xi_bootstrap_denies_one_master_only() {
    // mutation: check once for the whole buffer instead of per master
    // mutation: pass EventSubject::Global to the bootstrap Receive check
    // mutation: skip the bootstrap Receive check (the explicit-Allow count must fail)
    // mutation: return RequestOutcome::Disconnect from XISelectEvents
    let (mut allowed, pointer_id, keyboard_id) = bootstrap_device_changed_fixture();
    let allow_calls = allowed.install_policy(|_, _| Decision::Allow);
    select_device_changed(
        &mut allowed,
        NOTIFY_RECEIVER_DENIED,
        20,
        ROOT_WINDOW,
        0, // XIAllDevices
    );
    let all_allow_wire = allowed.drain(NOTIFY_RECEIVER_DENIED);
    let all_allow_literal = literal_bootstrap_device_changed_stream(
        20,
        pointer_id,
        keyboard_id,
        ClientByteOrder::LittleEndian,
    );
    assert_eq!(
        all_allow_wire, all_allow_literal,
        "explicit-Allow bootstrap control emits both complete literal frames in order"
    );
    let allow_receives = receive_checks(&allow_calls)
        .into_iter()
        .filter(is_device_changed_receive)
        .collect::<Vec<_>>();
    assert_eq!(allow_receives.len(), 2);
    assert!(allow_receives.iter().all(|call| {
        call.client == NOTIFY_RECEIVER_DENIED
            && call.receive == Some(EventSubject::Window(ROOT_WINDOW))
            && call.events == Some(vec![EventDescription::extension(35, 137, 1)])
            && call.request
                == Some(RequestOpcodes {
                    major: 137,
                    minor: 46,
                })
    }));

    let (mut denied, _, denied_keyboard_id) = bootstrap_device_changed_fixture();
    let matched = Arc::new(AtomicUsize::new(0));
    let matched_for_policy = Arc::clone(&matched);
    let calls = denied.install_policy(move |_, call| {
        if call.hook == ExtensionHook::Receive
            && call.client == NOTIFY_RECEIVER_DENIED
            && call.receive == Some(EventSubject::Window(ROOT_WINDOW))
            && is_device_changed_receive(call)
            && matched_for_policy.fetch_add(1, Ordering::Relaxed) == 0
        {
            Decision::Deny(DENY)
        } else {
            Decision::Allow
        }
    });
    select_device_changed(
        &mut denied,
        NOTIFY_RECEIVER_DENIED,
        20,
        ROOT_WINDOW,
        0, // XIAllDevices
    );
    let received_wire = denied.drain(NOTIFY_RECEIVER_DENIED);
    let expected_keyboard = literal_device_changed_event(
        20,
        crate::xinput::DEVICEID_MASTER_KEYBOARD,
        denied_keyboard_id,
        1,
        &literal_keyboard_classes(denied_keyboard_id, ClientByteOrder::LittleEndian),
        ClientByteOrder::LittleEndian,
    );
    assert_eq!(
        received_wire, expected_keyboard,
        "keyboard literal frame survives pointer denial"
    );
    assert!(denied.state.clients.contains_key(&NOTIFY_RECEIVER_DENIED.0));
    assert!(crate::core_loop::client_io::failed_writers(&denied.state.clients).is_empty());
    assert_eq!(
        denied.state.clients[&NOTIFY_RECEIVER_DENIED.0]
            .xi2_masks
            .get(&(ROOT_WINDOW, 0)),
        Some(&u64::from(crate::xinput::XI2_DEVICE_CHANGED_MASK)),
        "denial does not undo XISelectEvents"
    );
    let receives = receive_checks(&calls)
        .into_iter()
        .filter(is_device_changed_receive)
        .collect::<Vec<_>>();
    assert_eq!(receives.len(), 2);
    assert!(receives.iter().all(|call| {
        call.client == NOTIFY_RECEIVER_DENIED
            && call.receive == Some(EventSubject::Window(ROOT_WINDOW))
            && call.events == Some(vec![EventDescription::extension(35, 137, 1)])
            && call.request
                == Some(RequestOpcodes {
                    major: 137,
                    minor: 46,
                })
    }));

    let (mut deny_all_events, _, _) = bootstrap_device_changed_fixture();
    let calls = deny_all_events.install_policy(|_, call| {
        if call.hook == ExtensionHook::Receive
            && call.client == NOTIFY_RECEIVER_DENIED
            && call.receive == Some(EventSubject::Window(ROOT_WINDOW))
            && is_device_changed_receive(call)
        {
            Decision::Deny(DENY)
        } else {
            Decision::Allow
        }
    });
    select_device_changed(
        &mut deny_all_events,
        NOTIFY_RECEIVER_DENIED,
        20,
        ROOT_WINDOW,
        0, // XIAllDevices
    );
    assert!(deny_all_events.drain(NOTIFY_RECEIVER_DENIED).is_empty());
    assert!(
        deny_all_events
            .state
            .clients
            .contains_key(&NOTIFY_RECEIVER_DENIED.0)
    );
    assert!(crate::core_loop::client_io::failed_writers(&deny_all_events.state.clients).is_empty());
    assert_eq!(
        deny_all_events.state.clients[&NOTIFY_RECEIVER_DENIED.0]
            .xi2_masks
            .get(&(ROOT_WINDOW, 0)),
        Some(&u64::from(crate::xinput::XI2_DEVICE_CHANGED_MASK))
    );
    let receives = receive_checks(&calls)
        .into_iter()
        .filter(is_device_changed_receive)
        .collect::<Vec<_>>();
    assert_eq!(receives.len(), 2);
    assert!(receives.iter().all(|call| {
        call.client == NOTIFY_RECEIVER_DENIED
            && call.receive == Some(EventSubject::Window(ROOT_WINDOW))
            && call.events == Some(vec![EventDescription::extension(35, 137, 1)])
            && call.request
                == Some(RequestOpcodes {
                    major: 137,
                    minor: 46,
                })
    }));
}

#[test]
fn receive_xi_device_changed_baseline_frames() {
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

#[derive(Clone, Copy, Debug)]
enum XiMappingRequest {
    ChangeDeviceKeyMapping,
    SetDeviceModifierMapping,
    SetDeviceButtonMapping,
    ChangeKeyboardDevice,
}

#[derive(Clone, Debug)]
enum XiMappingPolicy {
    None,
    AllowAll,
    Deny(Vec<ClientId>),
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

struct XiMappingRun {
    outputs: Vec<Vec<u8>>,
    calls: Vec<RecordedExtensionCheck>,
    keymap: Option<Vec<u32>>,
    modifier_mapping: Option<(u8, Vec<u8>)>,
    button_mapping: Option<Vec<u8>>,
    device_state: Vec<(u16, Option<u16>, bool)>,
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

fn run_xi_mapping_request(
    request: XiMappingRequest,
    select_originator: bool,
    policy: XiMappingPolicy,
) -> XiMappingRun {
    let mut fixture = xi_mapping_fixture(request, select_originator);
    let calls = match policy {
        XiMappingPolicy::None => None,
        XiMappingPolicy::AllowAll => Some(fixture.install_policy(|_, _| Decision::Allow)),
        XiMappingPolicy::Deny(clients) => {
            let subject = EventSubject::Device(mapping_device(request));
            let events = vec![mapping_event_description(request)];
            Some(fixture.install_policy(move |_, call| {
                if call.hook == ExtensionHook::Receive
                    && clients.contains(&call.client)
                    && call.receive == Some(subject)
                    && call.events.as_ref() == Some(&events)
                {
                    Decision::Deny(DENY)
                } else {
                    Decision::Allow
                }
            }))
        }
    };

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
    let outputs = MAPPING_CLIENTS
        .into_iter()
        .map(|client| fixture.drain(client))
        .collect();
    let calls = calls.map_or_else(Vec::new, |calls| calls.lock().unwrap().clone());
    XiMappingRun {
        outputs,
        calls,
        keymap: fixture.state.keymap_overrides.get(&8).cloned(),
        modifier_mapping: fixture
            .state
            .xi1_modifier_map
            .get(&mapping_device(request))
            .cloned(),
        button_mapping: fixture
            .state
            .xi1_button_map
            .get(&mapping_device(request))
            .cloned(),
        device_state,
    }
}

fn literal_mapping_notify(
    sequence: u16,
    request: u8,
    first_keycode: u8,
    count: u8,
    order: ClientByteOrder,
) -> Vec<u8> {
    let mut event = literal_event_header(34, 0, sequence, order);
    event.extend_from_slice(&[request, first_keycode, count]);
    finish_literal_event(event)
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

fn mapping_run_output(run: &XiMappingRun, client: ClientId) -> &[u8] {
    let index = MAPPING_CLIENTS
        .iter()
        .position(|candidate| *candidate == client)
        .expect("mapping fixture client");
    &run.outputs[index]
}

fn assert_mapping_receive_calls(
    calls: &[RecordedExtensionCheck],
    request: XiMappingRequest,
    clients: &[ClientId],
    request_opcodes: Option<RequestOpcodes>,
) {
    let receives: Vec<_> = calls
        .iter()
        .filter(|call| call.hook == ExtensionHook::Receive)
        .collect();
    assert_eq!(receives.len(), clients.len());
    for client in clients {
        assert!(receives.iter().any(|call| {
            call.client == *client
                && call.receive == Some(EventSubject::Device(mapping_device(request)))
                && call.events == Some(vec![mapping_event_description(request)])
                && call.request == request_opcodes
        }));
    }
}

#[test]
fn receive_xi_mapping_fanout_device_subject() {
    // mutation: use EventSubject::Global in X5
    let baseline = run_xi_mapping_request(
        XiMappingRequest::ChangeDeviceKeyMapping,
        false,
        XiMappingPolicy::AllowAll,
    );
    for (client, sequence, order) in [
        (
            NOTIFY_SENDER,
            MAPPING_ORIGINATOR_SEQUENCE,
            ClientByteOrder::LittleEndian,
        ),
        (
            NOTIFY_RECEIVER_DENIED,
            MAPPING_DENIED_SEQUENCE,
            ClientByteOrder::BigEndian,
        ),
        (
            NOTIFY_RECEIVER_ALLOWED,
            MAPPING_ALLOWED_SEQUENCE,
            ClientByteOrder::LittleEndian,
        ),
    ] {
        assert_eq!(
            mapping_run_output(&baseline, client),
            literal_mapping_stream(
                XiMappingRequest::ChangeDeviceKeyMapping,
                client,
                sequence,
                order,
                false,
            ),
            "allow control matches literal key-map output for client {}",
            client.0
        );
    }
    assert_eq!(baseline.keymap.as_deref(), Some([0x61].as_slice()));
    assert_mapping_receive_calls(
        &baseline.calls,
        XiMappingRequest::ChangeDeviceKeyMapping,
        &[NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED],
        Some(RequestOpcodes {
            major: 137,
            minor: 25,
        }),
    );

    let denied = run_xi_mapping_request(
        XiMappingRequest::ChangeDeviceKeyMapping,
        false,
        XiMappingPolicy::Deny(vec![NOTIFY_RECEIVER_DENIED]),
    );
    assert_eq!(
        mapping_run_output(&denied, NOTIFY_RECEIVER_DENIED),
        literal_mapping_notify(MAPPING_DENIED_SEQUENCE, 1, 8, 1, ClientByteOrder::BigEndian,),
        "denial drops only the selected DeviceMappingNotify copy"
    );
    assert_eq!(
        mapping_run_output(&denied, NOTIFY_SENDER),
        literal_mapping_notify(
            MAPPING_ORIGINATOR_SEQUENCE,
            1,
            8,
            1,
            ClientByteOrder::LittleEndian,
        ),
        "requester receives exactly the core MappingNotify"
    );
    assert_eq!(
        mapping_run_output(&denied, NOTIFY_RECEIVER_ALLOWED),
        mapping_run_output(&baseline, NOTIFY_RECEIVER_ALLOWED),
        "allowed recipient stays byte-identical to literal baseline"
    );
    assert_eq!(denied.keymap.as_deref(), Some([0x61].as_slice()));
    assert_eq!(denied.keymap, baseline.keymap);
    assert_mapping_receive_calls(
        &denied.calls,
        XiMappingRequest::ChangeDeviceKeyMapping,
        &[NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED],
        Some(RequestOpcodes {
            major: 137,
            minor: 25,
        }),
    );
}

#[test]
fn receive_xi_mapping_originator_denial_keeps_reply_order() {
    // mutation: skip the check at the button-mapping site only
    // mutation: move X7b's write into the reply buffer
    for request in [
        XiMappingRequest::ChangeDeviceKeyMapping,
        XiMappingRequest::SetDeviceModifierMapping,
        XiMappingRequest::SetDeviceButtonMapping,
    ] {
        let all_allow = run_xi_mapping_request(request, true, XiMappingPolicy::AllowAll);
        let denied =
            run_xi_mapping_request(request, true, XiMappingPolicy::Deny(vec![NOTIFY_SENDER]));
        let minor = mapping_request_body(request).0;
        assert_mapping_receive_calls(
            &all_allow.calls,
            request,
            &MAPPING_CLIENTS,
            Some(RequestOpcodes { major: 137, minor }),
        );
        for (client, sequence, order) in [
            (
                NOTIFY_SENDER,
                MAPPING_ORIGINATOR_SEQUENCE,
                ClientByteOrder::LittleEndian,
            ),
            (
                NOTIFY_RECEIVER_DENIED,
                MAPPING_DENIED_SEQUENCE,
                ClientByteOrder::BigEndian,
            ),
            (
                NOTIFY_RECEIVER_ALLOWED,
                MAPPING_ALLOWED_SEQUENCE,
                ClientByteOrder::LittleEndian,
            ),
        ] {
            assert_eq!(
                mapping_run_output(&all_allow, client),
                literal_mapping_stream(request, client, sequence, order, true),
                "all-Allow literal frames for {request:?}, client {}",
                client.0
            );
            let expected_denied = if client == NOTIFY_SENDER {
                match request {
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
                }
            } else {
                literal_mapping_stream(request, client, sequence, order, true)
            };
            assert_eq!(
                mapping_run_output(&denied, client),
                expected_denied,
                "denial removes only the XI copy for {request:?}, client {}",
                client.0
            );
        }
        match request {
            XiMappingRequest::ChangeDeviceKeyMapping => {
                assert_eq!(denied.keymap.as_deref(), Some([0x61].as_slice()));
                assert_eq!(denied.keymap, all_allow.keymap);
            }
            XiMappingRequest::SetDeviceModifierMapping => {
                assert_eq!(denied.modifier_mapping, all_allow.modifier_mapping);
                assert_eq!(
                    denied.modifier_mapping.as_ref().map(|mapping| mapping.0),
                    Some(1)
                );
            }
            XiMappingRequest::SetDeviceButtonMapping => {
                assert_eq!(denied.button_mapping, all_allow.button_mapping);
                assert_eq!(denied.button_mapping.as_ref().unwrap()[0], 2);
            }
            XiMappingRequest::ChangeKeyboardDevice => unreachable!(),
        }
        assert_mapping_receive_calls(
            &denied.calls,
            request,
            &MAPPING_CLIENTS,
            Some(RequestOpcodes { major: 137, minor }),
        );
    }
}

#[test]
fn receive_change_device_notify_originator_denial_keeps_core_mapping() {
    // mutation: leave X6 on the unchecked fanout
    // mutation: route the core MappingNotify through the checked fanout
    let request = XiMappingRequest::ChangeKeyboardDevice;
    let all_allow = run_xi_mapping_request(request, true, XiMappingPolicy::AllowAll);
    let denied = run_xi_mapping_request(
        request,
        true,
        XiMappingPolicy::Deny(vec![NOTIFY_SENDER, NOTIFY_RECEIVER_DENIED]),
    );
    assert_mapping_receive_calls(
        &all_allow.calls,
        request,
        &MAPPING_CLIENTS,
        Some(RequestOpcodes {
            major: 137,
            minor: 11,
        }),
    );
    for (client, sequence, order) in [
        (
            NOTIFY_SENDER,
            MAPPING_ORIGINATOR_SEQUENCE,
            ClientByteOrder::LittleEndian,
        ),
        (
            NOTIFY_RECEIVER_DENIED,
            MAPPING_DENIED_SEQUENCE,
            ClientByteOrder::BigEndian,
        ),
        (
            NOTIFY_RECEIVER_ALLOWED,
            MAPPING_ALLOWED_SEQUENCE,
            ClientByteOrder::LittleEndian,
        ),
    ] {
        assert_eq!(
            mapping_run_output(&all_allow, client),
            literal_mapping_stream(request, client, sequence, order, true),
            "all-Allow literal ChangeDeviceNotify and core MappingNotify frames"
        );
        let expected_denied = if client == NOTIFY_SENDER {
            [
                literal_mapping_notify(sequence, 1, 0, 0, order),
                literal_xi1_reply(sequence, 0, 0, order),
            ]
            .concat()
        } else if client == NOTIFY_RECEIVER_DENIED {
            literal_mapping_notify(sequence, 1, 0, 0, order)
        } else {
            literal_mapping_stream(request, client, sequence, order, true)
        };
        assert_eq!(
            mapping_run_output(&denied, client),
            expected_denied,
            "denied ChangeDeviceNotify leaves core MappingNotify and reply"
        );
    }
    assert_eq!(denied.device_state, all_allow.device_state);
    assert_mapping_receive_calls(
        &denied.calls,
        request,
        &MAPPING_CLIENTS,
        Some(RequestOpcodes {
            major: 137,
            minor: 11,
        }),
    );
    assert!(
        denied
            .calls
            .iter()
            .filter(|call| call.hook == ExtensionHook::Receive)
            .all(|call| call.events
                == Some(vec![EventDescription::extension(
                    crate::server::XI_FIRST_EVENT + crate::xinput::XI_CHANGE_DEVICE_NOTIFY_OFFSET,
                    137,
                    u16::from(crate::xinput::XI_CHANGE_DEVICE_NOTIFY_OFFSET),
                )]))
    );
}

#[test]
fn receive_xi_mapping_change_baseline_order() {
    // mutation: append X7a's button-map copy before the reply
    // mutation: give X6 the request's sequence instead of the recipient's
    // mutation: move X5's write ahead of the core MappingNotify
    for request in [
        XiMappingRequest::SetDeviceButtonMapping,
        XiMappingRequest::ChangeKeyboardDevice,
        XiMappingRequest::SetDeviceModifierMapping,
        XiMappingRequest::ChangeDeviceKeyMapping,
    ] {
        let run = run_xi_mapping_request(request, true, XiMappingPolicy::None);
        for (client, sequence, order) in [
            (
                NOTIFY_SENDER,
                MAPPING_ORIGINATOR_SEQUENCE,
                ClientByteOrder::LittleEndian,
            ),
            (
                NOTIFY_RECEIVER_DENIED,
                MAPPING_DENIED_SEQUENCE,
                ClientByteOrder::BigEndian,
            ),
            (
                NOTIFY_RECEIVER_ALLOWED,
                MAPPING_ALLOWED_SEQUENCE,
                ClientByteOrder::LittleEndian,
            ),
        ] {
            assert_eq!(
                mapping_run_output(&run, client),
                literal_mapping_stream(request, client, sequence, order, true),
                "no-policy complete literal stream for {request:?}, client {}",
                client.0
            );
        }
        assert!(run.calls.is_empty(), "no policy means no Receive calls");
        match request {
            XiMappingRequest::ChangeDeviceKeyMapping => {
                assert_eq!(run.keymap.as_deref(), Some([0x61].as_slice()));
            }
            XiMappingRequest::SetDeviceModifierMapping => {
                assert_eq!(
                    run.modifier_mapping.as_ref().map(|mapping| mapping.0),
                    Some(1)
                );
            }
            XiMappingRequest::SetDeviceButtonMapping => {
                assert_eq!(run.button_mapping.as_ref().unwrap()[0], 2);
            }
            XiMappingRequest::ChangeKeyboardDevice => {}
        }
    }
}

#[test]
fn receive_xi_mapping_after_xkb_change() {
    // mutation: leave X5 on the unchecked fanout
    let request = XiMappingRequest::ChangeDeviceKeyMapping;
    let run_xkb_notify = |policy: XiMappingPolicy| {
        let mut fixture = xi_mapping_fixture(request, false);
        let calls = match policy {
            XiMappingPolicy::None => None,
            XiMappingPolicy::AllowAll => Some(fixture.install_policy(|_, _| Decision::Allow)),
            XiMappingPolicy::Deny(clients) => {
                let subject = EventSubject::Device(mapping_device(request));
                let events = vec![mapping_event_description(request)];
                Some(fixture.install_policy(move |_, call| {
                    if call.hook == ExtensionHook::Receive
                        && clients.contains(&call.client)
                        && call.receive == Some(subject)
                        && call.events.as_ref() == Some(&events)
                    {
                        Decision::Deny(DENY)
                    } else {
                        Decision::Allow
                    }
                }))
            }
        };
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
        let outputs: Vec<Vec<u8>> = MAPPING_CLIENTS
            .into_iter()
            .map(|client| fixture.drain(client))
            .collect();
        let calls = calls.map_or_else(Vec::new, |calls| calls.lock().unwrap().clone());
        (outputs, calls)
    };

    let no_policy = run_xkb_notify(XiMappingPolicy::None);
    let all_allow = run_xkb_notify(XiMappingPolicy::AllowAll);
    let denied = run_xkb_notify(XiMappingPolicy::Deny(vec![NOTIFY_RECEIVER_DENIED]));
    let denied_client_baseline = [
        literal_mapping_notify(MAPPING_DENIED_SEQUENCE, 1, 8, 1, ClientByteOrder::BigEndian),
        literal_xi1_mapping_notify(
            MAPPING_DENIED_SEQUENCE,
            crate::xinput::DEVICEID_MASTER_KEYBOARD,
            1,
            8,
            1,
            ClientByteOrder::BigEndian,
        ),
    ]
    .concat();
    let allowed_client_baseline = [
        literal_mapping_notify(
            MAPPING_ALLOWED_SEQUENCE,
            1,
            8,
            1,
            ClientByteOrder::LittleEndian,
        ),
        literal_xi1_mapping_notify(
            MAPPING_ALLOWED_SEQUENCE,
            crate::xinput::DEVICEID_MASTER_KEYBOARD,
            1,
            8,
            1,
            ClientByteOrder::LittleEndian,
        ),
    ]
    .concat();
    let sender_core_mapping = literal_mapping_notify(0, 1, 8, 1, ClientByteOrder::LittleEndian);
    let expected_baseline = vec![
        sender_core_mapping.clone(),
        denied_client_baseline.clone(),
        allowed_client_baseline.clone(),
    ];
    assert_eq!(
        no_policy.0, expected_baseline,
        "complete no-policy XKB streams"
    );
    assert_eq!(
        all_allow.0, expected_baseline,
        "complete explicit-Allow XKB streams"
    );
    assert_eq!(
        denied.0,
        vec![
            sender_core_mapping,
            literal_mapping_notify(MAPPING_DENIED_SEQUENCE, 1, 8, 1, ClientByteOrder::BigEndian,),
            allowed_client_baseline,
        ],
        "denial removes only A's XI copy while all clients keep core MappingNotify"
    );

    assert!(
        no_policy.1.is_empty(),
        "empty policy list adds no Receive calls"
    );
    assert_mapping_receive_calls(
        &all_allow.1,
        request,
        &[NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED],
        None,
    );
    assert_mapping_receive_calls(
        &denied.1,
        request,
        &[NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED],
        None,
    );
}

const XI_PROPERTY_DEVICE_ID: u16 = crate::xinput::DEVICEID_XTEST_POINTER;
const XI_PROPERTY_REQUEST_SEQUENCE: u16 = 0x31;
const XI_PROPERTY_DENIED_SEQUENCE: u16 = 0x42;
const XI_PROPERTY_ALLOWED_SEQUENCE: u16 = 0x53;
const XI_PROPERTY_VALUE: [u8; 1] = [0x5a];

#[derive(Clone, Copy)]
enum XiPropertyPolicy {
    None,
    AllowAll,
    Deny(EventDescription),
}

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

fn run_xi_property_change(
    policy: XiPropertyPolicy,
) -> (Vec<u8>, Vec<u8>, Vec<RecordedExtensionCheck>, AtomId) {
    let (mut fixture, property) = xi_property_notification_fixture(true);
    let calls = match policy {
        XiPropertyPolicy::None => None,
        XiPropertyPolicy::AllowAll => Some(fixture.install_policy(|_, _| Decision::Allow)),
        XiPropertyPolicy::Deny(description) => {
            let events = vec![description];
            Some(fixture.install_policy(move |_, call| {
                if call.hook == ExtensionHook::Receive
                    && call.client == NOTIFY_RECEIVER_DENIED
                    && call.receive == Some(EventSubject::Device(XI_PROPERTY_DEVICE_ID))
                    && call.events.as_ref() == Some(&events)
                {
                    Decision::Deny(DENY)
                } else {
                    Decision::Allow
                }
            }))
        }
    };
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

    let denied = fixture.drain(NOTIFY_RECEIVER_DENIED);
    let allowed = fixture.drain(NOTIFY_RECEIVER_ALLOWED);
    let calls = calls.map_or_else(Vec::new, |calls| calls.lock().unwrap().clone());
    (denied, allowed, calls, property)
}

fn assert_xi_property_receive_calls(calls: &[RecordedExtensionCheck]) {
    let receives: Vec<_> = calls
        .iter()
        .filter(|call| call.hook == ExtensionHook::Receive)
        .collect();
    let descriptions = xi_property_event_descriptions();
    assert_eq!(receives.len(), 4, "one check per form for both recipients");
    for client in [NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED] {
        for description in descriptions {
            assert_eq!(
                receives
                    .iter()
                    .filter(|call| {
                        call.client == client
                            && call.receive == Some(EventSubject::Device(XI_PROPERTY_DEVICE_ID))
                            && call.events.as_deref() == Some(std::slice::from_ref(&description))
                    })
                    .count(),
                1,
                "one matching Receive check for client {} and {description:?}",
                client.0
            );
        }
    }
    assert!(receives.iter().all(|call| {
        call.request
            == Some(RequestOpcodes {
                major: 137,
                minor: 57,
            })
    }));
}

#[test]
fn receive_xi_property_forms_checked_independently() {
    // mutation: skip the check on the XI1 path (X10)
    let allow_all = run_xi_property_change(XiPropertyPolicy::AllowAll);
    let property = allow_all.3;
    let denied_baseline = literal_xi_property_stream(
        XI_PROPERTY_DENIED_SEQUENCE,
        XI_PROPERTY_DEVICE_ID,
        property,
        ClientByteOrder::BigEndian,
    );
    let allowed_baseline = literal_xi_property_stream(
        XI_PROPERTY_ALLOWED_SEQUENCE,
        XI_PROPERTY_DEVICE_ID,
        property,
        ClientByteOrder::LittleEndian,
    );
    let deny_xi2 = run_xi_property_change(XiPropertyPolicy::Deny(EventDescription::extension(
        35, 137, 12,
    )));
    assert_eq!(
        deny_xi2.0,
        literal_xi1_property_event(
            XI_PROPERTY_DENIED_SEQUENCE,
            XI_PROPERTY_DEVICE_ID,
            deny_xi2.3,
            ClientByteOrder::BigEndian,
        ),
        "XI1 literal copy remains after denying XI2"
    );
    assert_eq!(
        deny_xi2.1,
        literal_xi_property_stream(
            XI_PROPERTY_ALLOWED_SEQUENCE,
            XI_PROPERTY_DEVICE_ID,
            deny_xi2.3,
            ClientByteOrder::LittleEndian,
        ),
        "allowed recipient keeps both literal copies"
    );
    assert_xi_property_receive_calls(&deny_xi2.2);

    let deny_xi1 = run_xi_property_change(XiPropertyPolicy::Deny(EventDescription::extension(
        crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_PROPERTY_NOTIFY_OFFSET,
        137,
        u16::from(crate::xinput::XI_DEVICE_PROPERTY_NOTIFY_OFFSET),
    )));
    assert_eq!(
        deny_xi1.0,
        literal_xi2_property_event(
            XI_PROPERTY_DENIED_SEQUENCE,
            XI_PROPERTY_DEVICE_ID,
            deny_xi1.3,
            1,
            ClientByteOrder::BigEndian,
        ),
        "XI2 literal copy remains after denying XI1"
    );
    assert_eq!(
        deny_xi1.1,
        literal_xi_property_stream(
            XI_PROPERTY_ALLOWED_SEQUENCE,
            XI_PROPERTY_DEVICE_ID,
            deny_xi1.3,
            ClientByteOrder::LittleEndian,
        ),
        "allowed recipient keeps both literal copies"
    );
    assert_xi_property_receive_calls(&deny_xi1.2);

    assert_eq!(
        allow_all.0, denied_baseline,
        "all-Allow literal stream for A"
    );
    assert_eq!(
        allow_all.1, allowed_baseline,
        "all-Allow literal stream for B"
    );
    assert_xi_property_receive_calls(&allow_all.2);
}

#[test]
fn receive_xi_property_baseline_order() {
    // mutation: swap the X9 and X10 blocks
    let run = run_xi_property_change(XiPropertyPolicy::None);
    let property = run.3;
    assert_eq!(
        run.0,
        literal_xi_property_stream(
            XI_PROPERTY_DENIED_SEQUENCE,
            XI_PROPERTY_DEVICE_ID,
            property,
            ClientByteOrder::BigEndian,
        ),
        "complete no-policy XI2 then XI1 stream for A"
    );
    assert_eq!(
        run.1,
        literal_xi_property_stream(
            XI_PROPERTY_ALLOWED_SEQUENCE,
            XI_PROPERTY_DEVICE_ID,
            property,
            ClientByteOrder::LittleEndian,
        ),
        "complete no-policy XI2 then XI1 stream for B"
    );
    assert!(run.2.is_empty(), "empty policy list means no policy calls");
}

#[test]
fn receive_xi_property_denial_is_not_disconnect() {
    // mutation: push denied clients into the returned vector
    let (mut fixture, property) = xi_property_notification_fixture(false);
    super::seed_one_prop(
        &mut fixture.state,
        property.0,
        8,
        XI_PROPERTY_VALUE.to_vec(),
    );
    let descriptions = xi_property_event_descriptions();
    let calls = fixture.install_policy(|_, call| {
        if call.hook == ExtensionHook::Receive {
            Decision::Deny(DENY)
        } else {
            Decision::Allow
        }
    });
    let disconnected = crate::core_loop::process_request::props::emit_property_change(
        &mut fixture.state,
        XI_PROPERTY_DEVICE_ID,
        property,
        crate::xinput::PropWhat::Modified,
    );
    assert!(disconnected.is_empty(), "policy denial is not a disconnect");
    assert!(fixture.drain(NOTIFY_RECEIVER_DENIED).is_empty());
    assert!(fixture.drain(NOTIFY_RECEIVER_ALLOWED).is_empty());
    assert!(
        fixture
            .state
            .clients
            .contains_key(&NOTIFY_RECEIVER_DENIED.0)
    );
    assert!(crate::core_loop::client_io::failed_writers(&fixture.state.clients).is_empty());
    assert_eq!(
        fixture
            .state
            .xi_devices
            .device(XI_PROPERTY_DEVICE_ID)
            .unwrap()
            .properties[&property]
            .data,
        XI_PROPERTY_VALUE,
        "direct emission publishes an already-applied change"
    );
    let receives = receive_checks(&calls);
    assert_eq!(receives.len(), 2);
    assert!(receives.iter().all(|call| {
        call.client == NOTIFY_RECEIVER_DENIED
            && call.receive == Some(EventSubject::Device(XI_PROPERTY_DEVICE_ID))
            && call.request.is_none()
    }));
    for description in descriptions {
        assert_eq!(
            receives
                .iter()
                .filter(|call| call.events.as_deref() == Some(std::slice::from_ref(&description)))
                .count(),
            1
        );
    }
}
