use super::{
    access_notify_support::*,
    extensions::{ExtensionHook, RecordedExtensionCheck},
    *,
};
use crate::{
    access::{Decision, DenyError, EventDescription, EventSubject, RequestOpcodes},
    resources::{ROOT_WINDOW, ROOTLESS_SELECTION_WINDOW, SERVER_OWNER},
    rootless::{self, CoordinatePolicy, HostRequest, HostSerial, SelectionEvent},
    server::SelectionOwner,
};
use std::sync::atomic::Ordering;
use yserver_protocol::x11::{
    AtomId, ClientByteOrder, ClientId, CreateWindowRequest, ResourceId, SequenceNumber,
    xfixes as x11xfixes,
};

const A: ClientId = NOTIFY_SENDER;
const B: ClientId = NOTIFY_RECEIVER_DENIED;
const C: ClientId = NOTIFY_RECEIVER_ALLOWED;
const OWNER_WINDOW: ResourceId = ResourceId(0x0057_0001);
const REQUESTOR_A: ResourceId = ResourceId(0x0057_0002);
const REQUESTOR_B: ResourceId = ResourceId(0x0057_0003);
const HOST_WINDOW: ResourceId = ResourceId(0x0057_0010);
const STRUCTURE_NOTIFY: u32 = 1 << 17;
const DENY: DenyError = DenyError {
    code: x11::error::BAD_ACCESS,
    value: 0,
};

fn receive_checks(calls: &[RecordedExtensionCheck]) -> Vec<RecordedExtensionCheck> {
    calls
        .iter()
        .filter(|call| call.hook == ExtensionHook::Receive)
        .cloned()
        .collect()
}

fn create_window(fixture: &mut NotifyFixture, owner: ClientId, window: ResourceId) {
    fixture.state.resources.create_window(
        owner,
        CreateWindowRequest {
            depth: 24,
            window,
            parent: ROOT_WINDOW,
            width: 100,
            height: 80,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
}

fn set_sequence(fixture: &mut NotifyFixture, client: ClientId, sequence: u16) {
    fixture.state.clients[&client.0]
        .last_sequence
        .store(sequence, Ordering::Relaxed);
}

fn set_owner_request(
    fixture: &mut NotifyFixture,
    client: ClientId,
    sequence: u16,
    window: ResourceId,
    selection: AtomId,
    time: u32,
) {
    let mut body = Vec::with_capacity(12);
    body.extend_from_slice(&window.0.to_le_bytes());
    body.extend_from_slice(&selection.0.to_le_bytes());
    body.extend_from_slice(&time.to_le_bytes());
    fixture
        .request(
            client,
            SequenceNumber(sequence),
            RequestHeader {
                opcode: 22,
                data: 0,
                length_units: 4,
            },
            &body,
        )
        .expect("SetSelectionOwner through process_request");
}

fn convert_request(
    fixture: &mut NotifyFixture,
    client: ClientId,
    sequence: u16,
    requestor: ResourceId,
    selection: AtomId,
    target: AtomId,
    property: AtomId,
    time: u32,
) {
    let mut body = Vec::with_capacity(20);
    for word in [requestor.0, selection.0, target.0, property.0, time] {
        body.extend_from_slice(&word.to_le_bytes());
    }
    fixture
        .request(
            client,
            SequenceNumber(sequence),
            RequestHeader {
                opcode: 24,
                data: 0,
                length_units: 6,
            },
            &body,
        )
        .expect("ConvertSelection through process_request");
}

fn selection_owner_fixture() -> (NotifyFixture, AtomId, AtomId, AtomId) {
    let mut fixture = NotifyFixture::new();
    create_window(&mut fixture, B, OWNER_WINDOW);
    create_window(&mut fixture, B, REQUESTOR_B);
    fixture.state.start_instant = std::time::Instant::now() - std::time::Duration::from_secs(60);
    fixture.state.clients.get_mut(&A.0).unwrap().byte_order = ClientByteOrder::BigEndian;
    fixture.state.clients.get_mut(&B.0).unwrap().byte_order = ClientByteOrder::LittleEndian;
    fixture.state.clients.get_mut(&C.0).unwrap().byte_order = ClientByteOrder::BigEndian;
    set_sequence(&mut fixture, A, 0x4567);
    let selection = fixture.state.atoms.intern("ACCESS_NOTIFY_SELECTION", false);
    let target = fixture.state.atoms.intern("ACCESS_NOTIFY_TARGET", false);
    let property = fixture.state.atoms.intern("ACCESS_NOTIFY_PROPERTY", false);
    fixture.state.selections.insert(
        selection,
        SelectionOwner {
            window: OWNER_WINDOW,
            last_time_changed: 1,
            client: Some(A),
        },
    );
    (fixture, selection, target, property)
}

fn literal_selection_notify(
    order: ClientByteOrder,
    sequence: u16,
    time: u32,
    requestor: ResourceId,
    selection: AtomId,
    target: AtomId,
) -> Vec<u8> {
    let word = |value: u32| match order {
        ClientByteOrder::LittleEndian => value.to_le_bytes(),
        ClientByteOrder::BigEndian => value.to_be_bytes(),
    };
    let seq = match order {
        ClientByteOrder::LittleEndian => sequence.to_le_bytes(),
        ClientByteOrder::BigEndian => sequence.to_be_bytes(),
    };
    let mut event = vec![31, 0];
    event.extend_from_slice(&seq);
    for value in [time, requestor.0, selection.0, target.0, 0] {
        event.extend_from_slice(&word(value));
    }
    event.extend_from_slice(&[0; 8]);
    event
}

fn event_frames(bytes: &[u8]) -> Vec<[u8; 32]> {
    assert!(bytes.len().is_multiple_of(32), "complete events: {bytes:?}");
    bytes
        .chunks_exact(32)
        .map(|chunk| chunk.try_into().expect("32-byte event"))
        .collect()
}

// Mutations: ask the window creator; undo ownership after denial; gate XFIXES with the core denial.
#[test]
fn receive_selection_clear_targets_previous_setter() {
    let (mut control, selection, _, _) = selection_owner_fixture();
    let subscriber = fixture_window_for_xfixes(&mut control, B);
    control.state.xfixes_selection_masks.insert(
        (B.0, subscriber, selection),
        1 << x11xfixes::SELECTION_NOTIFY_SET_OWNER,
    );
    create_window(&mut control, C, REQUESTOR_A);
    set_owner_request(&mut control, C, 52, REQUESTOR_A, selection, 77);
    let control_clear = control.drain(A);
    let control_xfixes = control.drain(B);
    assert_eq!(event_frames(&control_clear)[0][0] & 0x7f, 29);
    assert_eq!(control_xfixes[0], crate::nested::XFIXES_FIRST_EVENT);

    let (mut denied, selection, _, _) = selection_owner_fixture();
    let subscriber = fixture_window_for_xfixes(&mut denied, B);
    denied.state.xfixes_selection_masks.insert(
        (B.0, subscriber, selection),
        1 << x11xfixes::SELECTION_NOTIFY_SET_OWNER,
    );
    create_window(&mut denied, C, REQUESTOR_A);
    let calls = denied.install_policy(|_, call| {
        if call.hook == ExtensionHook::Receive
            && call.client == A
            && call.events == Some(vec![EventDescription::core(29)])
        {
            Decision::Deny(DENY)
        } else {
            Decision::Allow
        }
    });
    let backend_before = denied.backend.calls();
    set_owner_request(&mut denied, C, 52, REQUESTOR_A, selection, 77);
    assert!(denied.drain(A).is_empty(), "denied SelectionClear only");
    assert_eq!(
        denied.drain(B),
        control_xfixes,
        "XFIXES remains independent"
    );
    assert!(denied.drain(C).is_empty());
    assert_eq!(denied.backend.calls(), backend_before);
    assert_eq!(
        denied.state.selections[&selection],
        SelectionOwner {
            window: REQUESTOR_A,
            last_time_changed: 77,
            client: Some(C),
        }
    );
    assert!(denied.state.clients.contains_key(&A.0));
    let receives = receive_checks(&calls.lock().unwrap());
    assert_eq!(receives.len(), 1);
    assert_eq!(receives[0].client, A, "setter, not window creator");
    assert_eq!(
        receives[0].receive,
        Some(EventSubject::Window(OWNER_WINDOW))
    );
    assert_eq!(receives[0].events, Some(vec![EventDescription::core(29)]));
    assert_eq!(
        receives[0].request,
        Some(RequestOpcodes {
            major: 22,
            minor: 0
        })
    );
    assert!(
        !calls
            .lock()
            .unwrap()
            .iter()
            .any(|call| call.hook == ExtensionHook::Send)
    );

    // Same-setter ownership transfer is not a SelectionClear site.
    let (mut same_setter, selection, _, _) = selection_owner_fixture();
    create_window(&mut same_setter, A, REQUESTOR_A);
    let same_calls = same_setter.install_policy(|_, _| Decision::Allow);
    set_owner_request(&mut same_setter, A, 53, REQUESTOR_A, selection, 78);
    assert!(
        event_frames(&same_setter.drain(A))
            .iter()
            .all(|event| event[0] & 0x7f != 29)
    );
    assert!(receive_checks(&same_calls.lock().unwrap()).is_empty());
}

fn fixture_window_for_xfixes(fixture: &mut NotifyFixture, owner: ClientId) -> ResourceId {
    let window = ResourceId(0x0057_00f0 + owner.0);
    create_window(fixture, owner, window);
    window
}

// Mutations: silently drop denied requests; answer by window creator; Receive-check the failure; clear owner on denial.
#[test]
fn receive_selection_request_denied_answers_actual_requester() {
    let (mut allowed, selection, target, property) = selection_owner_fixture();
    let owner_before = allowed.state.selections[&selection];
    let backend_before = allowed.backend.calls();
    let allow_calls = allowed.install_policy(|_, _| Decision::Allow);
    convert_request(
        &mut allowed,
        C,
        77,
        REQUESTOR_B,
        selection,
        target,
        property,
        0x1122_3344,
    );
    let owner_event = allowed.drain(A);
    assert_eq!(owner_event.len(), 32);
    assert_eq!(owner_event[0] & 0x7f, 30);
    assert_eq!(
        u32::from_be_bytes(owner_event[8..12].try_into().unwrap()),
        OWNER_WINDOW.0
    );
    assert_eq!(
        u32::from_be_bytes(owner_event[12..16].try_into().unwrap()),
        REQUESTOR_B.0
    );
    assert_eq!(
        u32::from_be_bytes(owner_event[16..20].try_into().unwrap()),
        selection.0
    );
    assert_eq!(
        u32::from_be_bytes(owner_event[20..24].try_into().unwrap()),
        target.0
    );
    assert_eq!(
        u32::from_be_bytes(owner_event[24..28].try_into().unwrap()),
        property.0
    );
    assert!(allowed.drain(B).is_empty());
    assert!(allowed.drain(C).is_empty());
    assert_eq!(allowed.state.selections[&selection], owner_before);
    assert_eq!(allowed.backend.calls(), backend_before);
    let allowed_checks = receive_checks(&allow_calls.lock().unwrap());
    assert_eq!(allowed_checks.len(), 1);
    assert_eq!(allowed_checks[0].client, A);
    assert_eq!(
        allowed_checks[0].receive,
        Some(EventSubject::Window(OWNER_WINDOW))
    );
    assert_eq!(
        allowed_checks[0].events,
        Some(vec![EventDescription::core(30)])
    );
    assert_eq!(
        allowed_checks[0].request,
        Some(RequestOpcodes {
            major: 24,
            minor: 0
        })
    );

    let (mut denied, selection, target, property) = selection_owner_fixture();
    let owner_before = denied.state.selections[&selection];
    let backend_before = denied.backend.calls();
    let calls = denied.install_policy(|_, call| {
        if call.hook == ExtensionHook::Receive
            && (call.client == A || call.events == Some(vec![EventDescription::core(31)]))
        {
            Decision::Deny(DENY)
        } else {
            Decision::Allow
        }
    });
    convert_request(
        &mut denied,
        C,
        77,
        REQUESTOR_B,
        selection,
        target,
        property,
        0x1122_3344,
    );
    assert!(
        denied.drain(A).is_empty(),
        "owner receives no SelectionRequest"
    );
    assert!(
        denied.drain(B).is_empty(),
        "requestor-window creator is not answered"
    );
    let failure = denied.drain(C);
    assert_eq!(
        failure,
        literal_selection_notify(
            ClientByteOrder::BigEndian,
            77,
            0x1122_3344,
            REQUESTOR_B,
            selection,
            target,
        )
    );
    assert_eq!(denied.state.selections[&selection], owner_before);
    assert!(
        denied
            .state
            .resources
            .window_property(REQUESTOR_B, property)
            .is_none()
    );
    assert_eq!(denied.backend.calls(), backend_before);
    assert!(denied.state.clients.contains_key(&A.0));
    assert!(denied.state.clients.contains_key(&B.0));
    assert!(denied.state.clients.contains_key(&C.0));
    let calls = calls.lock().unwrap();
    let receives = receive_checks(&calls);
    assert_eq!(receives.len(), 1, "the exempt failure has no Receive check");
    assert_eq!(receives[0].client, A);
    assert_eq!(
        receives[0].receive,
        Some(EventSubject::Window(OWNER_WINDOW))
    );
    assert_eq!(receives[0].events, Some(vec![EventDescription::core(30)]));
    assert_eq!(
        receives[0].request,
        Some(RequestOpcodes {
            major: 24,
            minor: 0
        })
    );
    assert!(!calls.iter().any(|call| call.hook == ExtensionHook::Send));

    // An absent owner and a disconnected setter produce byte-identical failure events.
    let (mut absent, selection, target, property) = selection_owner_fixture();
    absent.state.selections.remove(&selection);
    convert_request(
        &mut absent,
        C,
        77,
        REQUESTOR_B,
        selection,
        target,
        property,
        0x1122_3344,
    );
    let absent_failure = absent.drain(C);
    let (mut gone, selection, target, property) = selection_owner_fixture();
    gone.state.clients.remove(&A.0);
    gone.peers.remove(&A.0);
    convert_request(
        &mut gone,
        C,
        77,
        REQUESTOR_B,
        selection,
        target,
        property,
        0x1122_3344,
    );
    assert_eq!(gone.drain(C), absent_failure);
}

fn rootless_fixture() -> NotifyFixture {
    let mut fixture = NotifyFixture::new();
    fixture.backend = RecordingBackend::new().with_redirect_activation();
    rootless::enable(
        &mut fixture.state,
        &mut fixture.backend,
        CoordinatePolicy::Opaque,
    )
    .expect("enable rootless mode");
    let _ = rootless::selection::selection_window(&mut fixture.state);
    fixture.state.start_instant = std::time::Instant::now() - std::time::Duration::from_secs(60);
    fixture
}

// Mutations: queue before checking; treat a denied, unqueued bridge request as handled without refusing it.
#[test]
fn receive_bridge_selection_denial_answers_requester() {
    let mut allow = rootless_fixture();
    create_window(&mut allow, C, REQUESTOR_A);
    let selection = allow.state.atoms.intern("ACCESS_BRIDGE_SELECTION", false);
    let target = allow.state.atoms.intern("ACCESS_BRIDGE_TARGET", false);
    let property = allow.state.atoms.intern("ACCESS_BRIDGE_PROPERTY", false);
    rootless::selection::set_selection_owner(&mut allow.state, selection, true).unwrap();
    let owner_before = allow.state.selections[&selection];
    let _ = rootless::selection::take_selection_events(&mut allow.state);
    let allow_calls = allow.install_policy(|_, _| Decision::Allow);
    convert_request(
        &mut allow,
        C,
        61,
        REQUESTOR_A,
        selection,
        target,
        property,
        0x2233_4455,
    );
    assert!(allow.drain(C).is_empty());
    assert_eq!(allow.state.selections[&selection], owner_before);
    assert_eq!(
        rootless::selection::take_selection_events(&mut allow.state),
        [SelectionEvent::Request {
            time: 0x2233_4455,
            requestor: REQUESTOR_A,
            selection,
            target,
            property,
        }]
    );
    let checked = receive_checks(&allow_calls.lock().unwrap());
    assert_eq!(checked.len(), 1);
    assert_eq!(checked[0].client, SERVER_OWNER);
    assert_eq!(
        checked[0].receive,
        Some(EventSubject::Window(ROOTLESS_SELECTION_WINDOW))
    );
    assert_eq!(checked[0].events, Some(vec![EventDescription::core(30)]));
    assert_eq!(
        checked[0].request,
        Some(RequestOpcodes {
            major: 24,
            minor: 0
        })
    );

    let mut denied = rootless_fixture();
    create_window(&mut denied, C, REQUESTOR_A);
    let selection = denied.state.atoms.intern("ACCESS_BRIDGE_SELECTION", false);
    let target = denied.state.atoms.intern("ACCESS_BRIDGE_TARGET", false);
    let property = denied.state.atoms.intern("ACCESS_BRIDGE_PROPERTY", false);
    rootless::selection::set_selection_owner(&mut denied.state, selection, true).unwrap();
    let owner_before = denied.state.selections[&selection];
    let _ = rootless::selection::take_selection_events(&mut denied.state);
    let calls = denied.install_policy(|_, call| {
        if call.hook == ExtensionHook::Receive
            && (call.client == SERVER_OWNER
                || call.events == Some(vec![EventDescription::core(31)]))
        {
            Decision::Deny(DENY)
        } else {
            Decision::Allow
        }
    });
    convert_request(
        &mut denied,
        C,
        61,
        REQUESTOR_A,
        selection,
        target,
        property,
        0x2233_4455,
    );
    assert_eq!(
        denied.drain(C),
        literal_selection_notify(
            ClientByteOrder::LittleEndian,
            61,
            0x2233_4455,
            REQUESTOR_A,
            selection,
            target,
        )
    );
    assert_eq!(denied.state.selections[&selection], owner_before);
    assert!(
        denied
            .state
            .resources
            .window_property(REQUESTOR_A, property)
            .is_none()
    );
    assert!(rootless::selection::take_selection_events(&mut denied.state).is_empty());
    let receives = receive_checks(&calls.lock().unwrap());
    assert_eq!(receives.len(), 1, "denied result is exempt");
    assert_eq!(receives[0].client, SERVER_OWNER);
    assert_eq!(
        receives[0].receive,
        Some(EventSubject::Window(ROOTLESS_SELECTION_WINDOW))
    );
    assert_eq!(receives[0].events, Some(vec![EventDescription::core(30)]));
    assert!(denied.state.clients.contains_key(&C.0));
}

// Mutation: route this denied-owner outcome through note_refusal by the bridge requestor window.
#[test]
fn receive_denied_owner_with_bridge_requestor_answers_issuing_client() {
    let mut fixture = rootless_fixture();
    create_window(&mut fixture, A, OWNER_WINDOW);
    let selection = fixture.state.atoms.intern("ACCESS_BRIDGE_REQUESTOR", false);
    let target = fixture.state.atoms.intern("ACCESS_BRIDGE_TARGET", false);
    let property = fixture.state.atoms.intern("ACCESS_BRIDGE_PROPERTY", false);
    fixture.state.selections.insert(
        selection,
        SelectionOwner {
            window: OWNER_WINDOW,
            last_time_changed: 1,
            client: Some(A),
        },
    );
    let owner_before = fixture.state.selections[&selection];
    let requestor = rootless::selection::selection_window(&mut fixture.state);
    let calls = fixture.install_policy(|_, call| {
        if call.hook == ExtensionHook::Receive
            && (call.client == A || call.events == Some(vec![EventDescription::core(31)]))
        {
            Decision::Deny(DENY)
        } else {
            Decision::Allow
        }
    });
    convert_request(
        &mut fixture,
        C,
        62,
        requestor,
        selection,
        target,
        property,
        0x3344_5566,
    );
    assert!(fixture.drain(A).is_empty());
    assert_eq!(
        fixture.drain(C),
        literal_selection_notify(
            ClientByteOrder::LittleEndian,
            62,
            0x3344_5566,
            requestor,
            selection,
            target,
        )
    );
    assert!(rootless::selection::take_selection_events(&mut fixture.state).is_empty());
    assert_eq!(fixture.state.selections[&selection], owner_before);
    let receives = receive_checks(&calls.lock().unwrap());
    assert_eq!(receives.len(), 1);
    assert_eq!(receives[0].client, A);
    assert_eq!(
        receives[0].receive,
        Some(EventSubject::Window(OWNER_WINDOW))
    );
    assert_eq!(receives[0].events, Some(vec![EventDescription::core(30)]));
    assert_eq!(
        receives[0].request,
        Some(RequestOpcodes {
            major: 24,
            minor: 0
        })
    );
}

// Mutation: answer every denied owner through the socket writer instead of the typed bridge refusal.
#[test]
fn receive_denied_owner_for_bridge_conversion_queues_refusal() {
    let mut fixture = rootless_fixture();
    create_window(&mut fixture, A, OWNER_WINDOW);
    let selection = fixture.state.atoms.intern("ACCESS_BRIDGE_TYPED", false);
    let target = fixture.state.atoms.intern("ACCESS_BRIDGE_TARGET", false);
    let property = fixture.state.atoms.intern("ACCESS_BRIDGE_PROPERTY", false);
    fixture.state.selections.insert(
        selection,
        SelectionOwner {
            window: OWNER_WINDOW,
            last_time_changed: 1,
            client: Some(A),
        },
    );
    let owner_before = fixture.state.selections[&selection];
    let backend_before = fixture.backend.calls();
    let calls = fixture.install_policy(|_, call| {
        if call.hook == ExtensionHook::Receive
            && (call.client == A || call.events == Some(vec![EventDescription::core(31)]))
        {
            Decision::Deny(DENY)
        } else {
            Decision::Allow
        }
    });
    rootless::selection::convert_selection(&mut fixture.state, selection, target, property)
        .expect("typed bridge ConvertSelection");
    assert!(fixture.drain(A).is_empty());
    assert!(fixture.drain(B).is_empty());
    assert!(fixture.drain(C).is_empty());
    assert_eq!(fixture.state.selections[&selection], owner_before);
    assert!(
        fixture
            .state
            .resources
            .window_property(OWNER_WINDOW, property)
            .is_none()
    );
    assert_eq!(fixture.backend.calls(), backend_before);
    assert_eq!(
        rootless::selection::take_selection_events(&mut fixture.state),
        [SelectionEvent::Notify {
            selection,
            target,
            property: AtomId(0),
        }]
    );
    let receives = receive_checks(&calls.lock().unwrap());
    assert_eq!(receives.len(), 1);
    assert_eq!(receives[0].client, A);
    assert_eq!(
        receives[0].receive,
        Some(EventSubject::Window(OWNER_WINDOW))
    );
    assert_eq!(receives[0].events, Some(vec![EventDescription::core(30)]));
    assert_eq!(receives[0].request, None);
}

// Mutations: exempt SERVER_OWNER; queue before checking; treat a denied clear as conversion success; fall through after denial.
#[test]
fn receive_bridge_clear_denial_does_not_queue() {
    fn run(deny: bool) -> (NotifyFixture, AtomId, Vec<u8>, Vec<RecordedExtensionCheck>) {
        let mut fixture = rootless_fixture();
        let server_peer = install_client(&mut fixture.state, SERVER_OWNER.0);
        fixture.peers.insert(SERVER_OWNER.0, server_peer);
        create_window(&mut fixture, C, REQUESTOR_A);
        create_window(&mut fixture, B, REQUESTOR_B);
        let selection = fixture.state.atoms.intern("ACCESS_BRIDGE_CLEAR", false);
        rootless::selection::watch_selection(&mut fixture.state, selection, true).unwrap();
        rootless::selection::set_selection_owner(&mut fixture.state, selection, true).unwrap();
        let _ = rootless::selection::take_selection_events(&mut fixture.state);
        fixture.state.xfixes_selection_masks.insert(
            (B.0, REQUESTOR_B, selection),
            1 << x11xfixes::SELECTION_NOTIFY_SET_OWNER,
        );
        let calls = if deny {
            Some(fixture.install_policy(|_, call| {
                if call.hook == ExtensionHook::Receive
                    && call.client == SERVER_OWNER
                    && call.events == Some(vec![EventDescription::core(29)])
                {
                    Decision::Deny(DENY)
                } else {
                    Decision::Allow
                }
            }))
        } else {
            None
        };
        let time = fixture.state.timestamp_now();
        set_owner_request(&mut fixture, C, 63, REQUESTOR_A, selection, time);
        let xfixes = fixture.drain(B);
        let recorded = calls.map_or_else(Vec::new, |calls| calls.lock().unwrap().clone());
        (fixture, selection, xfixes, recorded)
    }

    let (mut control, selection, control_xfixes, _) = run(false);
    assert!(
        !control_xfixes.is_empty(),
        "the XFIXES control event is delivered"
    );
    let events = rootless::selection::take_selection_events(&mut control.state);
    assert!(events.iter().any(|event| matches!(event, SelectionEvent::Owner { selection: s, owner: Some(w), .. } if *s == selection && *w == REQUESTOR_A)));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, SelectionEvent::Clear { .. }))
            .count(),
        1,
        "Allow control queues the bridge SelectionClear"
    );

    let (mut denied, selection, denied_xfixes, calls) = run(true);
    assert_eq!(denied_xfixes.len(), control_xfixes.len());
    assert_eq!(&denied_xfixes[..16], &control_xfixes[..16]);
    assert_eq!(
        &denied_xfixes[24..],
        &control_xfixes[24..],
        "XFIXES event padding survives core denial"
    );
    assert!(denied.drain(A).is_empty());
    assert!(denied.drain(C).is_empty());
    assert_eq!(denied.state.selections[&selection].window, REQUESTOR_A);
    assert_eq!(denied.state.selections[&selection].client, Some(C));
    let events = rootless::selection::take_selection_events(&mut denied.state);
    assert!(events.iter().any(|event| matches!(event, SelectionEvent::Owner { selection: s, owner: Some(w), .. } if *s == selection && *w == REQUESTOR_A)));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, SelectionEvent::Clear { .. }))
    );
    let receives = receive_checks(&calls);
    assert_eq!(
        receives.len(),
        1,
        "denied bridge clear is handled, not retried"
    );
    assert_eq!(receives[0].client, SERVER_OWNER);
    assert_eq!(
        receives[0].receive,
        Some(EventSubject::Window(ROOTLESS_SELECTION_WINDOW))
    );
    assert_eq!(receives[0].events, Some(vec![EventDescription::core(29)]));
    assert_eq!(
        receives[0].request,
        Some(RequestOpcodes {
            major: 22,
            minor: 0
        })
    );
}

fn rootless_host_fixture() -> (NotifyFixture, ResourceId, rootless::ToplevelId) {
    let mut fixture = NotifyFixture::new();
    fixture.backend = RecordingBackend::new().with_redirect_activation();
    rootless::enable(
        &mut fixture.state,
        &mut fixture.backend,
        CoordinatePolicy::Global { origin: (0, 0) },
    )
    .expect("enable rootless mode");
    fixture.state.start_instant = std::time::Instant::now() - std::time::Duration::from_secs(60);
    create_window(&mut fixture, A, HOST_WINDOW);

    let protocols = fixture.state.atoms.intern("WM_PROTOCOLS", false);
    let atom_type = fixture.state.atoms.intern("ATOM", false);
    let delete_window = fixture.state.atoms.intern("WM_DELETE_WINDOW", false);
    let mut property = Vec::with_capacity(24);
    property.extend_from_slice(&HOST_WINDOW.0.to_le_bytes());
    property.extend_from_slice(&protocols.0.to_le_bytes());
    property.extend_from_slice(&atom_type.0.to_le_bytes());
    property.extend_from_slice(&[32, 0, 0, 0]);
    property.extend_from_slice(&1u32.to_le_bytes());
    property.extend_from_slice(&delete_window.0.to_le_bytes());
    fixture
        .request(
            A,
            SequenceNumber(2),
            RequestHeader {
                opcode: 18,
                data: 0,
                length_units: 7,
            },
            &property,
        )
        .expect("WM_PROTOCOLS ChangeProperty");
    let _ = fixture.state.resources.map_window(HOST_WINDOW);
    rootless::reconcile_and_dispatch(&mut fixture.state, &mut fixture.backend);
    let id = fixture
        .state
        .rootless
        .as_ref()
        .and_then(|rootless| rootless.toplevel_id(HOST_WINDOW))
        .expect("mapped host top-level");
    for client in [A, B] {
        fixture
            .state
            .clients
            .get_mut(&client.0)
            .unwrap()
            .event_masks
            .insert(HOST_WINDOW, STRUCTURE_NOTIFY);
    }
    fixture.state.clients.get_mut(&B.0).unwrap().byte_order = ClientByteOrder::BigEndian;
    let _ = fixture.drain(A);
    let _ = fixture.drain(B);
    (fixture, HOST_WINDOW, id)
}

fn host_request(fixture: &mut NotifyFixture, request: HostRequest) {
    rootless::host_request(&mut fixture.state, &mut fixture.backend, request);
}

fn host_geometry(fixture: &NotifyFixture, window: ResourceId) -> (i16, i16, u16, u16) {
    let window = fixture.state.resources.window(window).unwrap();
    (window.x, window.y, window.width, window.height)
}

fn host_wm_state(fixture: &mut NotifyFixture, window: ResourceId) -> Option<Vec<u8>> {
    let wm_state = fixture.state.atoms.intern("WM_STATE", false);
    fixture
        .state
        .resources
        .window_property(window, wm_state)
        .map(|value| value.data.clone())
}

// Mutations: bypass raw checks; describe sent type with bit 7; exempt host SelectionNotify(None) by type/property.
#[test]
fn receive_rootless_outgoing_core_events_are_checked() {
    let (mut control, window, id) = rootless_host_fixture();
    let control_calls = control.install_policy(|_, _| Decision::Allow);
    host_request(
        &mut control,
        HostRequest::Moved {
            id,
            x: 30,
            y: 40,
            serial: HostSerial(7),
        },
    );
    let control_a_move = control.drain(A);
    let control_b_move = control.drain(B);
    assert!(
        event_frames(&control_a_move)
            .iter()
            .any(|event| event[0] == (22 | 0x80))
    );
    assert!(
        event_frames(&control_b_move)
            .iter()
            .any(|event| event[0] == (22 | 0x80))
    );
    assert_eq!(control.state.resources.window(window).unwrap().x, 30);
    let control_geometry = host_geometry(&control, window);
    let control_wm_state = host_wm_state(&mut control, window);
    let control_backend = control.backend.calls();
    let delete_window = control.state.atoms.intern("WM_DELETE_WINDOW", false);
    host_request(&mut control, HostRequest::CloseRequested { id });
    let control_close = control.drain(A);
    assert_eq!(event_frames(&control_close)[0][0], 33 | 0x80);
    assert_eq!(
        u32::from_le_bytes(control_close[12..16].try_into().unwrap()),
        delete_window.0
    );
    assert!(
        receive_checks(&control_calls.lock().unwrap())
            .iter()
            .any(|call| {
                call.client == A
                    && call.receive == Some(EventSubject::Window(window))
                    && call.events == Some(vec![EventDescription::core(33)])
                    && call.request.is_none()
            })
    );
    let requestor_b = ResourceId(0x0057_0011);
    create_window(&mut control, B, requestor_b);
    let selection = control
        .state
        .atoms
        .intern("ACCESS_HOST_NOTIFY_SELECTION", false);
    let target = control
        .state
        .atoms
        .intern("ACCESS_HOST_NOTIFY_TARGET", false);
    let property = control
        .state
        .atoms
        .intern("ACCESS_HOST_NOTIFY_PROPERTY", false);
    let mut control_notify = Vec::new();
    for property_value in [property, AtomId(0)] {
        rootless::selection::send_selection_notify(
            &mut control.state,
            0x4455_6677,
            window,
            [selection, target, property_value],
        )
        .unwrap();
        assert_eq!(control.drain(A).len(), 32);
        rootless::selection::send_selection_notify(
            &mut control.state,
            0x4455_6677,
            requestor_b,
            [selection, target, property_value],
        )
        .unwrap();
        control_notify.push(control.drain(B));
    }
    assert!(
        receive_checks(&control_calls.lock().unwrap())
            .iter()
            .any(|call| {
                call.client == B
                    && call.receive == Some(EventSubject::Window(requestor_b))
                    && call.events == Some(vec![EventDescription::core(31)])
                    && call.request.is_none()
            })
    );

    let (mut denied, window, id) = rootless_host_fixture();
    let calls = denied.install_policy(|_, call| {
        if call.hook == ExtensionHook::Receive
            && call.client == A
            && matches!(
                call.events.as_deref(),
                Some([EventDescription {
                    type_code: 22 | 31 | 33,
                    ..
                }])
            )
        {
            Decision::Deny(DENY)
        } else {
            Decision::Allow
        }
    });
    host_request(
        &mut denied,
        HostRequest::Moved {
            id,
            x: 30,
            y: 40,
            serial: HostSerial(7),
        },
    );
    assert!(
        denied.drain(A).is_empty(),
        "denied recipient sees no ConfigureNotify"
    );
    assert_eq!(
        denied.drain(B),
        control_b_move,
        "allowed subscriber keeps bytes"
    );
    assert_eq!(denied.state.resources.window(window).unwrap().x, 30);
    assert_eq!(denied.state.resources.window(window).unwrap().y, 40);
    assert_eq!(host_geometry(&denied, window), control_geometry);
    assert_eq!(host_wm_state(&mut denied, window), control_wm_state);
    assert_eq!(denied.backend.calls(), control_backend);
    let delete_window = denied.state.atoms.intern("WM_DELETE_WINDOW", false);
    host_request(&mut denied, HostRequest::CloseRequested { id });
    assert!(
        denied.drain(A).is_empty(),
        "denied ClientMessage is omitted"
    );
    assert!(
        denied.drain(B).is_empty(),
        "mask-zero routing remains window-creator only"
    );
    assert_eq!(host_geometry(&denied, window), control_geometry);
    assert_eq!(host_wm_state(&mut denied, window), control_wm_state);
    assert_eq!(denied.backend.calls(), control_backend);

    // The typed host API emits both successful-property and failure SelectionNotify values.
    let requestor_b = ResourceId(0x0057_0011);
    create_window(&mut denied, B, requestor_b);
    let selection = denied
        .state
        .atoms
        .intern("ACCESS_HOST_NOTIFY_SELECTION", false);
    let target = denied
        .state
        .atoms
        .intern("ACCESS_HOST_NOTIFY_TARGET", false);
    let property = denied
        .state
        .atoms
        .intern("ACCESS_HOST_NOTIFY_PROPERTY", false);
    let before_receive_count = receive_checks(&calls.lock().unwrap()).len();
    for (index, property_value) in [property, AtomId(0)].into_iter().enumerate() {
        rootless::selection::send_selection_notify(
            &mut denied.state,
            0x4455_6677,
            window,
            [selection, target, property_value],
        )
        .unwrap();
        assert!(denied.drain(A).is_empty());
        rootless::selection::send_selection_notify(
            &mut denied.state,
            0x4455_6677,
            requestor_b,
            [selection, target, property_value],
        )
        .unwrap();
        let allowed = denied.drain(B);
        assert_eq!(allowed.len(), 32);
        assert_eq!(allowed, control_notify[index]);
        assert_eq!(allowed[0], 31 | 0x80, "server synthetic bit is preserved");
        assert_eq!(
            u32::from_be_bytes(allowed[20..24].try_into().unwrap()),
            property_value.0
        );
    }
    assert_eq!(denied.state.resources.window(window).unwrap().x, 30);
    assert_eq!(denied.state.resources.window(window).unwrap().y, 40);
    let recorded = calls.lock().unwrap();
    let receives = receive_checks(&recorded);
    assert_eq!(receives.len(), before_receive_count + 4);
    assert!(receives.iter().any(|call| {
        call.client == A
            && call.receive == Some(EventSubject::Window(window))
            && call.events == Some(vec![EventDescription::core(22)])
            && call.request.is_none()
    }));
    assert!(receives.iter().any(|call| {
        call.client == A
            && call.receive == Some(EventSubject::Window(window))
            && call.events == Some(vec![EventDescription::core(33)])
            && call.request.is_none()
    }));
    let notify_receives: Vec<_> = receives[before_receive_count..].iter().collect();
    assert_eq!(
        notify_receives
            .iter()
            .map(|call| call.client)
            .collect::<Vec<_>>(),
        [A, B, A, B]
    );
    for call in notify_receives {
        assert_eq!(call.events, Some(vec![EventDescription::core(31)]));
        assert_eq!(call.request, None);
        let subject_window = if call.client == A {
            window
        } else {
            requestor_b
        };
        assert_eq!(call.receive, Some(EventSubject::Window(subject_window)));
    }
    assert!(!recorded.iter().any(|call| call.hook == ExtensionHook::Send));
    let close_frames = event_frames(&control_close);
    assert_eq!(close_frames[0][0], 33 | 0x80);
    assert_eq!(
        delete_window.0,
        u32::from_le_bytes(close_frames[0][12..16].try_into().unwrap())
    );
}
