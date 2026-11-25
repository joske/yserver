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
    resources::ROOT_WINDOW,
};
use std::{sync::Arc, time::Duration};
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
