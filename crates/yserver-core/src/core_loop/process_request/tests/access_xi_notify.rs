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
    let mut bytes = vec![81, 0]; // XI_FIRST_EVENT + XI_DevicePresenceNotify
    bytes.extend_from_slice(&sequence.to_le_bytes());
    bytes.extend_from_slice(&[0, 0, 0, 0]); // zero timestamp
    bytes.extend_from_slice(&[change, u8::try_from(device_id).unwrap(), 0, 0]);
    bytes.extend_from_slice(&[0; 20]);
    bytes
}

fn literal_hierarchy_disabled(sequence: u16) -> Vec<u8> {
    let mut bytes = vec![35, 137]; // GenericEvent, XInput major opcode
    bytes.extend_from_slice(&sequence.to_le_bytes());
    bytes.extend_from_slice(&15_u32.to_le_bytes()); // five 12-byte device records
    bytes.extend_from_slice(&11_u16.to_le_bytes()); // XI_HierarchyChanged
    bytes.extend_from_slice(&0_u16.to_le_bytes()); // XIAllDevices
    bytes.extend_from_slice(&[0; 4]); // zero timestamp
    bytes.extend_from_slice(&128_u32.to_le_bytes()); // XI_DeviceDisabled
    bytes.extend_from_slice(&5_u16.to_le_bytes());
    bytes.extend_from_slice(&[0; 10]); // header padding
    // Master pointer: attached to its paired master keyboard.
    bytes.extend_from_slice(&[2, 0, 3, 0, 1, 1, 0, 0, 0, 0, 0, 0]);
    // Master keyboard: attached to its paired master pointer.
    bytes.extend_from_slice(&[3, 0, 2, 0, 2, 1, 0, 0, 0, 0, 0, 0]);
    // Virtual XTEST pointer.
    bytes.extend_from_slice(&[4, 0, 2, 0, 3, 1, 0, 0, 0, 0, 0, 0]);
    // Virtual XTEST keyboard.
    bytes.extend_from_slice(&[5, 0, 3, 0, 4, 1, 0, 0, 0, 0, 0, 0]);
    // Disabled physical pointer facet (the changed device).
    bytes.extend_from_slice(&[6, 0, 2, 0, 3, 0, 0, 0, 128, 0, 0, 0]);
    bytes
}

fn literal_disabled_stream(sequence: u16, device_id: u16) -> Vec<u8> {
    [
        literal_presence_disabled(sequence, device_id),
        literal_presence_disabled(sequence, device_id),
        literal_hierarchy_disabled(sequence),
        literal_hierarchy_disabled(sequence),
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
        xi_hotplug_select_presence(
            &mut fixture.state,
            &mut fixture.backend,
            client.0,
            5,
            ROOT_WINDOW,
        );
        xi_hotplug_select_presence(
            &mut fixture.state,
            &mut fixture.backend,
            client.0,
            6,
            CHILD_WINDOW,
        );
    }
    disable_device(&mut fixture, device_id);
    let expected = literal_disabled_stream(6, device_id);
    for client in [NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED] {
        let actual = fixture.drain(client);
        assert_eq!(actual, expected, "complete hotplug frames for {client:?}");
    }
}
