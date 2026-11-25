use super::{
    access_notify_support::{
        NOTIFY_RECEIVER_ALLOWED, NOTIFY_RECEIVER_DENIED, NOTIFY_SENDER, NotifyFixture,
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
