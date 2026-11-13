use super::{
    access_notify_support::*,
    extensions::{ExtensionHook, RecordedExtensionCheck},
    *,
};
use crate::{
    access::{Decision, DenyError, EventDescription, EventSubject},
    backend::recording::RecordedCall,
};
use std::sync::atomic::{AtomicBool, Ordering};

const DENY: DenyError = DenyError {
    code: x11::error::BAD_ACCESS,
    value: 0,
};

fn property_fixture() -> (NotifyFixture, AtomId) {
    let mut fixture = NotifyFixture::new();
    select_property_change(&mut fixture);
    let property = fixture.state.atoms.intern("ACCESS_NOTIFY_TEST", false);
    (fixture, property)
}

fn access_snapshot(
    fixture: &NotifyFixture,
    property: AtomId,
) -> (Vec<u8>, Vec<RecordedCall>, Vec<(u32, u32)>) {
    let value = fixture
        .state
        .resources
        .window_property(NOTIFY_WINDOW, property)
        .expect("changed property");
    let masks = [
        NOTIFY_SENDER,
        NOTIFY_RECEIVER_DENIED,
        NOTIFY_RECEIVER_ALLOWED,
    ]
    .into_iter()
    .map(|client| {
        (
            client.0,
            fixture.state.clients[&client.0]
                .event_masks
                .get(&NOTIFY_WINDOW)
                .copied()
                .unwrap_or(0),
        )
    })
    .collect();
    (value.data.clone(), fixture.backend.calls(), masks)
}

fn receive_checks(calls: &[RecordedExtensionCheck]) -> Vec<&RecordedExtensionCheck> {
    calls
        .iter()
        .filter(|call| call.hook == ExtensionHook::Receive)
        .collect()
}

// Mutation: bypass Receive in checked fanout, deny the whole fanout on one denial,
// or record requestor ownership in place of the subject-window owner.
#[test]
fn receive_property_fanout_is_per_recipient() {
    let (mut baseline, property) = property_fixture();
    assert!(matches!(
        run_property_change(&mut baseline, 7, property, b"abc"),
        RequestOutcome::Handled
    ));
    assert!(baseline.drain(NOTIFY_SENDER).is_empty());
    let baseline_denied = baseline.drain(NOTIFY_RECEIVER_DENIED);
    let baseline_allowed = baseline.drain(NOTIFY_RECEIVER_ALLOWED);
    assert_eq!(
        baseline_denied,
        literal_property_notify(ClientByteOrder::BigEndian, 0x1234, property)
    );
    assert_eq!(
        baseline_allowed,
        literal_property_notify(ClientByteOrder::LittleEndian, 0x5678, property)
    );
    let baseline_effects = access_snapshot(&baseline, property);

    let (mut allow_control, allow_property) = property_fixture();
    assert_eq!(allow_property, property);
    allow_control.install_policy(|_, _| Decision::Allow);
    assert!(matches!(
        run_property_change(&mut allow_control, 7, allow_property, b"abc"),
        RequestOutcome::Handled
    ));
    assert_eq!(
        allow_control.drain(NOTIFY_SENDER),
        baseline.drain(NOTIFY_SENDER)
    );
    let allow_denied_client = allow_control.drain(NOTIFY_RECEIVER_DENIED);
    let allow_allowed_client = allow_control.drain(NOTIFY_RECEIVER_ALLOWED);
    assert_eq!(allow_denied_client, baseline_denied);
    assert_eq!(allow_allowed_client, baseline_allowed);
    assert_eq!(
        access_snapshot(&allow_control, allow_property),
        baseline_effects
    );

    let (mut denied, denied_property) = property_fixture();
    assert_eq!(denied_property, property);
    let calls = denied.install_policy(|_, call| {
        if call.hook == ExtensionHook::Receive && call.client == NOTIFY_RECEIVER_DENIED {
            Decision::Deny(DENY)
        } else {
            Decision::Allow
        }
    });
    assert!(matches!(
        run_property_change(&mut denied, 7, denied_property, b"abc"),
        RequestOutcome::Handled
    ));
    assert!(denied.drain(NOTIFY_SENDER).is_empty());
    assert!(denied.drain(NOTIFY_RECEIVER_DENIED).is_empty());
    assert_eq!(denied.drain(NOTIFY_RECEIVER_ALLOWED), allow_allowed_client);
    assert_eq!(
        access_snapshot(&denied, denied_property),
        access_snapshot(&allow_control, allow_property)
    );
    assert!(denied.state.clients.contains_key(&NOTIFY_RECEIVER_DENIED.0));

    let calls = calls.lock().unwrap();
    let receives = receive_checks(&calls);
    assert_eq!(receives.len(), 2);
    assert!(!calls.iter().any(|call| call.hook == ExtensionHook::Send));
    for call in &receives {
        assert_eq!(call.receive, Some(EventSubject::Window(NOTIFY_WINDOW)));
        assert_eq!(call.events, Some(vec![EventDescription::core(28)]));
        assert_eq!(call.request, request_opcodes(18, 0));
        assert_eq!(call.owner, Some(NOTIFY_RECEIVER_DENIED));
    }
    let mut recipients: Vec<_> = receives.iter().map(|call| call.client).collect();
    recipients.sort_by_key(|client| client.0);
    assert_eq!(
        recipients,
        [NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED]
    );
}

// Mutation: cache the first Receive Allow and reuse it for the next delivery.
#[test]
fn receive_notification_revocation_is_immediate() {
    let (mut fixture, property) = property_fixture();
    let revoked = Arc::new(AtomicBool::new(false));
    let is_revoked = Arc::clone(&revoked);
    let calls = fixture.install_policy(move |_, call| {
        if call.hook == ExtensionHook::Receive
            && call.client == NOTIFY_RECEIVER_DENIED
            && is_revoked.load(Ordering::Relaxed)
        {
            Decision::Deny(DENY)
        } else {
            Decision::Allow
        }
    });

    assert!(matches!(
        run_property_change(&mut fixture, 8, property, b"one"),
        RequestOutcome::Handled
    ));
    let first_denied = fixture.drain(NOTIFY_RECEIVER_DENIED);
    let first_allowed = fixture.drain(NOTIFY_RECEIVER_ALLOWED);
    assert_eq!(
        first_denied,
        literal_property_notify(ClientByteOrder::BigEndian, 0x1234, property)
    );
    assert_eq!(
        first_allowed,
        literal_property_notify(ClientByteOrder::LittleEndian, 0x5678, property)
    );

    revoked.store(true, Ordering::Relaxed);
    assert!(matches!(
        run_property_change(&mut fixture, 9, property, b"two"),
        RequestOutcome::Handled
    ));
    assert!(fixture.drain(NOTIFY_SENDER).is_empty());
    assert!(fixture.drain(NOTIFY_RECEIVER_DENIED).is_empty());
    let second_allowed = fixture.drain(NOTIFY_RECEIVER_ALLOWED);

    let (mut allow_control, allow_property) = property_fixture();
    allow_control.install_policy(|_, _| Decision::Allow);
    assert!(matches!(
        run_property_change(&mut allow_control, 8, allow_property, b"one"),
        RequestOutcome::Handled
    ));
    let _ = allow_control.drain(NOTIFY_RECEIVER_DENIED);
    let _ = allow_control.drain(NOTIFY_RECEIVER_ALLOWED);
    assert!(matches!(
        run_property_change(&mut allow_control, 9, allow_property, b"two"),
        RequestOutcome::Handled
    ));
    assert!(allow_control.drain(NOTIFY_SENDER).is_empty());
    assert_eq!(
        allow_control.drain(NOTIFY_RECEIVER_DENIED),
        literal_property_notify(ClientByteOrder::BigEndian, 0x1234, allow_property)
    );
    let allowed_control_second = allow_control.drain(NOTIFY_RECEIVER_ALLOWED);
    assert_eq!(second_allowed, allowed_control_second);
    assert_eq!(
        access_snapshot(&fixture, property),
        access_snapshot(&allow_control, allow_property)
    );
    assert!(
        fixture
            .state
            .clients
            .contains_key(&NOTIFY_RECEIVER_DENIED.0)
    );
    let calls = calls.lock().unwrap();
    assert_eq!(receive_checks(&calls).len(), 4);
    assert_eq!(
        receive_checks(&calls)
            .iter()
            .filter(|call| call.client == NOTIFY_RECEIVER_DENIED)
            .count(),
        2
    );
    assert_eq!(
        fixture
            .state
            .resources
            .window_property(NOTIFY_WINDOW, property)
            .unwrap()
            .data,
        b"two"
    );
}

// Mutations: leave the active opcode unset, or skip restoration after a BadLength return.
#[test]
fn receive_request_scope_restores_after_dispatch() {
    let (mut fixture, property) = property_fixture();
    let calls = fixture.install_policy(|_, _| Decision::Allow);
    assert!(matches!(
        run_property_change(&mut fixture, 10, property, b"abc"),
        RequestOutcome::Handled
    ));
    assert_eq!(fixture.state.receive_request, None);
    assert!(
        receive_checks(&calls.lock().unwrap())
            .iter()
            .any(|call| { call.request == request_opcodes(18, 0) })
    );

    let invalid = fixture
        .request(
            NOTIFY_SENDER,
            SequenceNumber(11),
            RequestHeader {
                opcode: 18,
                data: 0,
                length_units: 1,
            },
            &[],
        )
        .expect("invalid-length ChangeProperty returns an X error outcome");
    assert!(matches!(invalid, RequestOutcome::Handled));
    let error = fixture.drain(NOTIFY_SENDER);
    assert_eq!(error[0], 0);
    assert_eq!(error[1], x11::error::BAD_LENGTH);
    assert_eq!(fixture.state.receive_request, None);
}

// Mutation: leave the most recent request opcode active for later host-generated events.
#[test]
fn receive_request_context_does_not_leak_to_host_expose() {
    let (mut fixture, property) = property_fixture();
    let calls = fixture.install_policy(|_, _| Decision::Allow);
    assert!(matches!(
        run_property_change(&mut fixture, 12, property, b"host-context"),
        RequestOutcome::Handled
    ));
    assert_eq!(fixture.state.receive_request, None);
    let _ = fixture.drain(NOTIFY_RECEIVER_DENIED);
    let _ = fixture.drain(NOTIFY_RECEIVER_ALLOWED);

    let invalid = fixture
        .request(
            NOTIFY_SENDER,
            SequenceNumber(13),
            RequestHeader {
                opcode: 18,
                data: 0,
                length_units: 1,
            },
            &[],
        )
        .expect("invalid-length ChangeProperty returns an X error outcome");
    assert!(matches!(invalid, RequestOutcome::Handled));
    let error = fixture.drain(NOTIFY_SENDER);
    assert_eq!(error.len(), 32);
    assert_eq!(error[0], 0);
    assert_eq!(error[1], x11::error::BAD_LENGTH);
    assert_eq!(fixture.state.receive_request, None);

    for client in [NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED] {
        fixture
            .state
            .clients
            .get_mut(&client.0)
            .expect("selected receiver")
            .event_masks
            .insert(NOTIFY_WINDOW, 0x0000_8000);
    }
    fixture
        .state
        .resources
        .window_mut(NOTIFY_WINDOW)
        .expect("selected mapped window")
        .map_state = crate::resources::MapState::Viewable;
    let host_xid = 0x0055_0001;
    let dropped = crate::core_loop::fanout::expose_event_fanout_to_state(
        &mut fixture.state,
        &crate::host_x11::HostXidMap::from([(host_xid, NOTIFY_WINDOW)]),
        crate::host_x11::HostExposeEvent {
            host_xid,
            x: 1,
            y: 2,
            width: 10,
            height: 11,
            count: 0,
        },
    );
    assert!(dropped.is_empty());
    assert_eq!(fixture.drain(NOTIFY_RECEIVER_DENIED)[0], 12);
    assert_eq!(fixture.drain(NOTIFY_RECEIVER_ALLOWED)[0], 12);
    assert_eq!(fixture.state.receive_request, None);

    let calls = calls.lock().unwrap();
    let receives = receive_checks(&calls);
    let property_receives: Vec<_> = receives
        .iter()
        .filter(|call| call.events == Some(vec![EventDescription::core(28)]))
        .collect();
    assert_eq!(property_receives.len(), 2);
    assert!(
        property_receives
            .iter()
            .all(|call| call.request == request_opcodes(18, 0))
    );
    let host_exposes: Vec<_> = receives
        .iter()
        .filter(|call| call.events == Some(vec![EventDescription::core(12)]))
        .collect();
    assert_eq!(host_exposes.len(), 2);
    assert!(host_exposes.iter().all(|call| {
        call.receive == Some(EventSubject::Window(NOTIFY_WINDOW)) && call.request.is_none()
    }));
}
