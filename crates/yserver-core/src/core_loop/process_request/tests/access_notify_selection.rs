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

fn literal_synthetic_selection_notify(
    order: ClientByteOrder,
    sequence: u16,
    time: u32,
    requestor: ResourceId,
    selection: AtomId,
    target: AtomId,
    property: AtomId,
) -> Vec<u8> {
    let mut event = literal_selection_notify(order, sequence, time, requestor, selection, target);
    event[0] |= 0x80;
    let property_bytes = match order {
        ClientByteOrder::LittleEndian => property.0.to_le_bytes(),
        ClientByteOrder::BigEndian => property.0.to_be_bytes(),
    };
    event[20..24].copy_from_slice(&property_bytes);
    event
}

fn literal_selection_clear(
    order: ClientByteOrder,
    sequence: u16,
    time: u32,
    window: ResourceId,
    selection: AtomId,
) -> Vec<u8> {
    let word = |value: u32| match order {
        ClientByteOrder::LittleEndian => value.to_le_bytes(),
        ClientByteOrder::BigEndian => value.to_be_bytes(),
    };
    let seq = match order {
        ClientByteOrder::LittleEndian => sequence.to_le_bytes(),
        ClientByteOrder::BigEndian => sequence.to_be_bytes(),
    };
    let mut event = vec![29, 0];
    event.extend_from_slice(&seq);
    event.extend_from_slice(&word(time));
    event.extend_from_slice(&word(window.0));
    event.extend_from_slice(&word(selection.0));
    event.resize(32, 0);
    event
}

fn literal_selection_request(
    order: ClientByteOrder,
    sequence: u16,
    time: u32,
    owner: ResourceId,
    requestor: ResourceId,
    selection: AtomId,
    target: AtomId,
    property: AtomId,
) -> Vec<u8> {
    let word = |value: u32| match order {
        ClientByteOrder::LittleEndian => value.to_le_bytes(),
        ClientByteOrder::BigEndian => value.to_be_bytes(),
    };
    let seq = match order {
        ClientByteOrder::LittleEndian => sequence.to_le_bytes(),
        ClientByteOrder::BigEndian => sequence.to_be_bytes(),
    };
    let mut event = vec![30, 0];
    event.extend_from_slice(&seq);
    for value in [
        time,
        owner.0,
        requestor.0,
        selection.0,
        target.0,
        property.0,
    ] {
        event.extend_from_slice(&word(value));
    }
    event.resize(32, 0);
    event
}

fn event_frames(bytes: &[u8]) -> Vec<[u8; 32]> {
    assert!(bytes.len().is_multiple_of(32), "complete events: {bytes:?}");
    bytes
        .chunks_exact(32)
        .map(|chunk| chunk.try_into().expect("32-byte event"))
        .collect()
}

fn literal_configure_notify(
    order: ClientByteOrder,
    event_type: u8,
    sequence: u16,
    event_window: ResourceId,
    window: ResourceId,
    sibling: ResourceId,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
    border_width: u16,
    override_redirect: bool,
) -> [u8; 32] {
    let mut event = literal_event_header(event_type, 0, sequence, order);
    append_event_u32(&mut event, order, event_window.0);
    append_event_u32(&mut event, order, window.0);
    append_event_u32(&mut event, order, sibling.0);
    event.extend_from_slice(&match order {
        ClientByteOrder::LittleEndian => x.to_le_bytes(),
        ClientByteOrder::BigEndian => x.to_be_bytes(),
    });
    event.extend_from_slice(&match order {
        ClientByteOrder::LittleEndian => y.to_le_bytes(),
        ClientByteOrder::BigEndian => y.to_be_bytes(),
    });
    append_event_u16(&mut event, order, width);
    append_event_u16(&mut event, order, height);
    append_event_u16(&mut event, order, border_width);
    event.push(u8::from(override_redirect));
    finish_literal_event(event)
        .try_into()
        .expect("ConfigureNotify is 32 bytes")
}

fn literal_wm_delete_client_message(
    order: ClientByteOrder,
    sequence: u16,
    window: ResourceId,
    protocols: AtomId,
    delete_window: AtomId,
) -> Vec<u8> {
    let mut event = literal_event_header(33 | 0x80, 32, sequence, order);
    append_event_u32(&mut event, order, window.0);
    append_event_u32(&mut event, order, protocols.0);
    append_event_u32(&mut event, order, delete_window.0);
    append_event_u32(&mut event, order, 0); // timestamp normalized after bounds check
    event.extend_from_slice(&[0; 12]);
    event
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
    assert!(!control.state.policies_installed());
    let control_before = control.state.timestamp_now();
    let selection_time = control_before;
    set_owner_request(&mut control, C, 52, REQUESTOR_A, selection, selection_time);
    let control_bounds = (control_before, control.state.timestamp_now());
    let control_clear = control.drain(A);
    let control_xfixes = control.drain(B);
    assert_eq!(
        control_clear,
        literal_selection_clear(
            ClientByteOrder::BigEndian,
            0x4567,
            selection_time,
            OWNER_WINDOW,
            selection,
        )
    );
    assert_xfixes_event_matches_expected(
        &control_xfixes,
        control_bounds,
        0,
        subscriber,
        REQUESTOR_A,
        selection,
        selection_time,
    );

    let (mut allow, selection, _, _) = selection_owner_fixture();
    let subscriber = fixture_window_for_xfixes(&mut allow, B);
    allow.state.xfixes_selection_masks.insert(
        (B.0, subscriber, selection),
        1 << x11xfixes::SELECTION_NOTIFY_SET_OWNER,
    );
    create_window(&mut allow, C, REQUESTOR_A);
    let allow_calls = allow.install_policy(|_, _| Decision::Allow);
    let allow_before = allow.state.timestamp_now();
    let allow_time = allow_before;
    set_owner_request(&mut allow, C, 52, REQUESTOR_A, selection, allow_time);
    let allow_bounds = (allow_before, allow.state.timestamp_now());
    let allow_clear = allow.drain(A);
    let allow_xfixes = allow.drain(B);
    assert_eq!(
        allow_clear,
        literal_selection_clear(
            ClientByteOrder::BigEndian,
            0x4567,
            allow_time,
            OWNER_WINDOW,
            selection,
        )
    );
    assert_xfixes_event_matches_expected(
        &allow_xfixes,
        allow_bounds,
        0,
        subscriber,
        REQUESTOR_A,
        selection,
        allow_time,
    );
    let mut allow_clear_without_time = allow_clear.clone();
    let mut control_clear_without_time = control_clear.clone();
    allow_clear_without_time[4..8].fill(0);
    control_clear_without_time[4..8].fill(0);
    assert_eq!(allow_clear_without_time, control_clear_without_time);
    assert_xfixes_event_matches_except_timestamps(
        &allow_xfixes,
        allow_bounds,
        &control_xfixes,
        control_bounds,
    );
    assert_eq!(receive_checks(&allow_calls.lock().unwrap()).len(), 1);

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
    let denied_before = denied.state.timestamp_now();
    let denied_time = denied_before;
    set_owner_request(&mut denied, C, 52, REQUESTOR_A, selection, denied_time);
    let denied_bounds = (denied_before, denied.state.timestamp_now());
    let denied_xfixes = denied.drain(B);
    assert!(denied.drain(A).is_empty(), "denied SelectionClear only");
    assert_eq!(
        denied_xfixes[0],
        crate::nested::XFIXES_FIRST_EVENT,
        "XFIXES remains independent"
    );
    assert_xfixes_event_matches_expected(
        &denied_xfixes,
        denied_bounds,
        0,
        subscriber,
        REQUESTOR_A,
        selection,
        denied_time,
    );
    assert_xfixes_event_matches_except_timestamps(
        &denied_xfixes,
        denied_bounds,
        &control_xfixes,
        control_bounds,
    );
    assert!(denied.drain(C).is_empty());
    assert_eq!(denied.backend.calls(), backend_before);
    assert_eq!(
        denied.state.selections[&selection],
        SelectionOwner {
            window: REQUESTOR_A,
            last_time_changed: denied_time,
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

pub(super) fn run_xfixes_owner_change(
    deny_receive: bool,
) -> (Vec<Vec<u8>>, Vec<RecordedExtensionCheck>, (u32, u32)) {
    let (mut fixture, selection, _, _) = selection_owner_fixture();
    fixture.state.start_instant = std::time::Instant::now()
        - std::time::Duration::from_secs(if deny_receive { 65 } else { 60 });
    let subscriber = fixture_window_for_xfixes(&mut fixture, NOTIFY_RECEIVER_DENIED);
    fixture.state.xfixes_selection_masks.insert(
        (NOTIFY_RECEIVER_DENIED.0, subscriber, selection),
        1 << x11xfixes::SELECTION_NOTIFY_SET_OWNER,
    );
    let requestor = ResourceId(0x0057_0004);
    create_window(&mut fixture, NOTIFY_RECEIVER_ALLOWED, requestor);
    let checks = deny_receive.then(|| {
        fixture.install_policy(|_, call| {
            if call.hook == ExtensionHook::Receive {
                Decision::Deny(DENY)
            } else {
                Decision::Allow
            }
        })
    });
    let before = fixture.state.timestamp_now();
    let timestamp = before;
    set_owner_request(
        &mut fixture,
        NOTIFY_RECEIVER_ALLOWED,
        52,
        requestor,
        selection,
        timestamp,
    );
    let after = fixture.state.timestamp_now();
    let outputs: Vec<Vec<u8>> = [
        NOTIFY_SENDER,
        NOTIFY_RECEIVER_DENIED,
        NOTIFY_RECEIVER_ALLOWED,
    ]
    .into_iter()
    .map(|client| fixture.drain(client))
    .collect();
    if !deny_receive {
        assert_xfixes_event_matches_expected(
            &outputs[1],
            (before, after),
            0,
            subscriber,
            requestor,
            selection,
            timestamp,
        );
    }
    let calls = checks.map_or_else(Vec::new, |calls| calls.lock().unwrap().clone());
    (outputs, calls, (before, after))
}

fn timestamp_is_between(value: u32, (before, after): (u32, u32)) -> bool {
    value.wrapping_sub(before) <= after.wrapping_sub(before)
}

pub(super) fn assert_xfixes_event_matches_expected(
    actual: &[u8],
    bounds: (u32, u32),
    sequence: u16,
    window: ResourceId,
    owner: ResourceId,
    selection: AtomId,
    selection_timestamp: u32,
) {
    let timestamp = u32::from_le_bytes(
        actual
            .get(16..20)
            .expect("XFIXES current-time field")
            .try_into()
            .unwrap(),
    );
    assert!(
        timestamp_is_between(timestamp, bounds),
        "XFIXES current time {timestamp} is within {bounds:?}"
    );
    let mut expected = literal_event_header(
        crate::nested::XFIXES_FIRST_EVENT,
        x11xfixes::SELECTION_NOTIFY_SET_OWNER,
        sequence,
        ClientByteOrder::LittleEndian,
    );
    append_event_u32(&mut expected, ClientByteOrder::LittleEndian, window.0);
    append_event_u32(&mut expected, ClientByteOrder::LittleEndian, owner.0);
    append_event_u32(&mut expected, ClientByteOrder::LittleEndian, selection.0);
    append_event_u32(&mut expected, ClientByteOrder::LittleEndian, 0); // dynamic current time
    append_event_u32(
        &mut expected,
        ClientByteOrder::LittleEndian,
        selection_timestamp,
    );
    expected.extend_from_slice(&[0; 8]);
    let mut actual_without_current_time = actual.to_vec();
    actual_without_current_time[16..20].fill(0);
    assert_eq!(actual_without_current_time, expected);
}

pub(super) fn assert_xfixes_event_matches_except_timestamps(
    actual: &[u8],
    actual_bounds: (u32, u32),
    expected: &[u8],
    expected_bounds: (u32, u32),
) {
    assert_eq!(actual.len(), 32, "complete XFIXES SelectionNotify");
    assert_eq!(expected.len(), 32, "complete XFIXES SelectionNotify");
    for (bytes, bounds, label) in [
        (actual, actual_bounds, "actual"),
        (expected, expected_bounds, "expected"),
    ] {
        for offset in [16, 20] {
            let timestamp = u32::from_le_bytes(
                bytes[offset..offset + 4]
                    .try_into()
                    .expect("XFIXES timestamp"),
            );
            assert!(
                timestamp_is_between(timestamp, bounds),
                "{label} timestamp {timestamp} is within {bounds:?}"
            );
        }
    }
    let mut actual_without_timestamps = actual.to_vec();
    let mut expected_without_timestamps = expected.to_vec();
    actual_without_timestamps[16..24].fill(0);
    expected_without_timestamps[16..24].fill(0);
    assert_eq!(
        actual_without_timestamps, expected_without_timestamps,
        "all non-timestamp XFIXES event bytes match"
    );
}

// Mutations: silently drop denied requests; answer by window creator; Receive-check the failure; clear owner on denial.
#[test]
fn receive_selection_request_denied_answers_actual_requester() {
    let (mut baseline, selection, target, property) = selection_owner_fixture();
    assert!(!baseline.state.policies_installed());
    convert_request(
        &mut baseline,
        C,
        77,
        REQUESTOR_B,
        selection,
        target,
        property,
        0x1122_3344,
    );
    let baseline_owner_event = baseline.drain(A);
    assert!(baseline.drain(B).is_empty());
    assert!(baseline.drain(C).is_empty());
    assert_eq!(
        baseline_owner_event,
        literal_selection_request(
            ClientByteOrder::BigEndian,
            0x4567,
            0x1122_3344,
            OWNER_WINDOW,
            REQUESTOR_B,
            selection,
            target,
            property,
        )
    );

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
    assert_eq!(owner_event, baseline_owner_event);
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

// Mutation: restore note_refusal(requestor) before the issuer check.
#[test]
fn no_owner_socket_conversion_with_bridge_requestor_answers_issuer() {
    let mut fixture = rootless_fixture();
    let requestor = rootless::selection::selection_window(&mut fixture.state);
    let selection = fixture
        .state
        .atoms
        .intern("ACCESS_NO_OWNER_SELECTION", false);
    let target = fixture.state.atoms.intern("ACCESS_NO_OWNER_TARGET", false);
    let property = fixture
        .state
        .atoms
        .intern("ACCESS_NO_OWNER_PROPERTY", false);

    convert_request(
        &mut fixture,
        C,
        63,
        requestor,
        selection,
        target,
        property,
        0x4455_6677,
    );

    let received = fixture.drain(C);
    assert_eq!(received.len(), 32, "one complete SelectionNotify");
    assert_eq!(received[0], 31, "SelectionNotify without the synthetic bit");
    assert_eq!(
        received,
        literal_selection_notify(
            ClientByteOrder::LittleEndian,
            63,
            0x4455_6677,
            requestor,
            selection,
            target,
        )
    );
    assert!(
        rootless::selection::take_selection_events(&mut fixture.state).is_empty(),
        "socket conversion is answered to its issuer, not the host queue"
    );
}

// Mutation: always call send_selection_failure for no-owner conversions.
#[test]
fn no_owner_bridge_conversion_queues_refusal() {
    let mut fixture = rootless_fixture();
    let selection = fixture
        .state
        .atoms
        .intern("ACCESS_NO_OWNER_SELECTION", false);
    let target = fixture.state.atoms.intern("ACCESS_NO_OWNER_TARGET", false);
    let property = fixture
        .state
        .atoms
        .intern("ACCESS_NO_OWNER_PROPERTY", false);

    rootless::selection::convert_selection(&mut fixture.state, selection, target, property)
        .expect("typed bridge ConvertSelection");

    assert_eq!(
        rootless::selection::take_selection_events(&mut fixture.state),
        [SelectionEvent::Notify {
            selection,
            target,
            property: AtomId(0),
        }]
    );
    for client in [A, B, C] {
        assert!(
            fixture.drain(client).is_empty(),
            "no socket bytes for {client:?}"
        );
    }
}

// Mutations: queue before checking; treat a denied, unqueued bridge request as handled without refusing it.
#[test]
fn receive_bridge_selection_denial_answers_requester() {
    let mut baseline = rootless_fixture();
    create_window(&mut baseline, C, REQUESTOR_A);
    let selection = baseline
        .state
        .atoms
        .intern("ACCESS_BRIDGE_SELECTION", false);
    let target = baseline.state.atoms.intern("ACCESS_BRIDGE_TARGET", false);
    let property = baseline.state.atoms.intern("ACCESS_BRIDGE_PROPERTY", false);
    rootless::selection::set_selection_owner(&mut baseline.state, selection, true).unwrap();
    let owner_before = baseline.state.selections[&selection];
    let _ = rootless::selection::take_selection_events(&mut baseline.state);
    assert!(!baseline.state.policies_installed());
    convert_request(
        &mut baseline,
        C,
        61,
        REQUESTOR_A,
        selection,
        target,
        property,
        0x2233_4455,
    );
    assert!(baseline.drain(C).is_empty());
    assert_eq!(baseline.state.selections[&selection], owner_before);
    let baseline_events = rootless::selection::take_selection_events(&mut baseline.state);
    assert_eq!(
        baseline_events,
        [SelectionEvent::Request {
            time: 0x2233_4455,
            requestor: REQUESTOR_A,
            selection,
            target,
            property,
        }]
    );

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
        baseline_events
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

// Mutations: exempt SERVER_OWNER; queue before checking; treat a denied clear as conversion success; fall through after denial; compare independent fixture timestamps instead of bounding each event's timestamps.
#[test]
fn receive_bridge_clear_denial_does_not_queue() {
    fn run(
        policy: Option<bool>,
    ) -> (
        NotifyFixture,
        AtomId,
        Vec<u8>,
        Vec<RecordedExtensionCheck>,
        (u32, u32),
    ) {
        let mut fixture = rootless_fixture();
        fixture.state.start_instant = std::time::Instant::now()
            - std::time::Duration::from_secs(if policy == Some(true) { 65 } else { 60 });
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
        let calls = policy.map(|deny| {
            fixture.install_policy(move |_, call| {
                if deny
                    && call.hook == ExtensionHook::Receive
                    && call.client == SERVER_OWNER
                    && call.events == Some(vec![EventDescription::core(29)])
                {
                    Decision::Deny(DENY)
                } else {
                    Decision::Allow
                }
            })
        });
        let before = fixture.state.timestamp_now();
        let time = before;
        set_owner_request(&mut fixture, C, 63, REQUESTOR_A, selection, time);
        let after = fixture.state.timestamp_now();
        let xfixes = fixture.drain(B);
        let recorded = calls.map_or_else(Vec::new, |calls| calls.lock().unwrap().clone());
        (fixture, selection, xfixes, recorded, (before, after))
    }

    let (mut control, selection, control_xfixes, _, control_bounds) = run(None);
    assert!(
        !control_xfixes.is_empty(),
        "the XFIXES control event is delivered"
    );
    let events = rootless::selection::take_selection_events(&mut control.state);
    match events.as_slice() {
        [
            SelectionEvent::Clear {
                selection: clear_selection,
                time: clear_time,
            },
            SelectionEvent::Owner {
                selection: owner_selection,
                owner: Some(owner),
                time: owner_time,
            },
        ] => {
            assert_eq!(*clear_selection, selection);
            assert_eq!(*owner_selection, selection);
            assert_eq!(*owner, REQUESTOR_A);
            assert!(timestamp_is_between(*owner_time, control_bounds));
            assert!(timestamp_is_between(*clear_time, control_bounds));
        }
        other => panic!("complete no-policy bridge selection records: {other:?}"),
    }

    let (mut allow, allow_selection, allow_xfixes, allow_calls, allow_bounds) = run(Some(false));
    assert_eq!(allow_selection, selection);
    assert_xfixes_event_matches_except_timestamps(
        &allow_xfixes,
        allow_bounds,
        &control_xfixes,
        control_bounds,
    );
    let allow_events = rootless::selection::take_selection_events(&mut allow.state);
    match allow_events.as_slice() {
        [
            SelectionEvent::Clear {
                selection: clear_selection,
                time: clear_time,
            },
            SelectionEvent::Owner {
                selection: owner_selection,
                owner: Some(owner),
                time: owner_time,
            },
        ] => {
            assert_eq!(*clear_selection, selection);
            assert_eq!(*owner_selection, selection);
            assert_eq!(*owner, REQUESTOR_A);
            assert!(timestamp_is_between(*owner_time, allow_bounds));
            assert!(timestamp_is_between(*clear_time, allow_bounds));
        }
        other => panic!("complete recording-Allow bridge selection records: {other:?}"),
    }
    assert_eq!(
        events
            .iter()
            .map(|event| match event {
                SelectionEvent::Owner {
                    selection, owner, ..
                } => (*selection, *owner),
                SelectionEvent::Clear { selection, .. } => (*selection, None),
                other => panic!("unexpected baseline selection record {other:?}"),
            })
            .collect::<Vec<_>>(),
        allow_events
            .iter()
            .map(|event| match event {
                SelectionEvent::Owner {
                    selection, owner, ..
                } => (*selection, *owner),
                SelectionEvent::Clear { selection, .. } => (*selection, None),
                other => panic!("unexpected Allow selection record {other:?}"),
            })
            .collect::<Vec<_>>(),
        "recording-Allow bridge queue parity"
    );
    assert_eq!(receive_checks(&allow_calls).len(), 1);

    let (mut denied, selection, denied_xfixes, calls, denied_bounds) = run(Some(true));
    assert_xfixes_event_matches_except_timestamps(
        &denied_xfixes,
        denied_bounds,
        &control_xfixes,
        control_bounds,
    );
    assert!(denied.drain(A).is_empty());
    assert!(denied.drain(C).is_empty());
    assert_eq!(denied.state.selections[&selection].window, REQUESTOR_A);
    assert_eq!(denied.state.selections[&selection].client, Some(C));
    let events = rootless::selection::take_selection_events(&mut denied.state);
    match events.as_slice() {
        [
            SelectionEvent::Owner {
                selection: event_selection,
                owner: Some(owner),
                time,
            },
        ] => {
            assert_eq!(*event_selection, selection);
            assert_eq!(*owner, REQUESTOR_A);
            assert!(timestamp_is_between(*time, denied_bounds));
        }
        other => panic!("complete denied bridge selection records: {other:?}"),
    }
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
    set_sequence(&mut fixture, A, 2);
    set_sequence(&mut fixture, B, 0x1234);
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

// Mutations: format 32->16 for ClientMessage; invert synthetic
// ConfigureNotify override_redirect; bypass raw checks; describe sent type
// with bit 7; exempt host SelectionNotify(None); or let ClientMessage time
// escape its fixture bounds.
#[test]
fn receive_rootless_outgoing_core_events_are_checked() {
    let (mut baseline, baseline_window, baseline_id) = rootless_host_fixture();
    assert!(!baseline.state.policies_installed());
    host_request(
        &mut baseline,
        HostRequest::Moved {
            id: baseline_id,
            x: 30,
            y: 40,
            serial: HostSerial(7),
        },
    );
    let baseline_a_move = baseline.drain(A);
    let baseline_b_move = baseline.drain(B);
    let expected_a_move = vec![
        literal_configure_notify(
            ClientByteOrder::LittleEndian,
            22,
            2,
            baseline_window,
            baseline_window,
            ResourceId(0x0000_0107),
            30,
            40,
            100,
            80,
            0,
            false,
        ),
        literal_configure_notify(
            ClientByteOrder::LittleEndian,
            22 | 0x80,
            2,
            baseline_window,
            baseline_window,
            ResourceId(0x0000_0107),
            30,
            40,
            100,
            80,
            0,
            false,
        ),
    ];
    let expected_b_move = vec![
        literal_configure_notify(
            ClientByteOrder::BigEndian,
            22,
            0x1234,
            baseline_window,
            baseline_window,
            ResourceId(0x0000_0107),
            30,
            40,
            100,
            80,
            0,
            false,
        ),
        literal_configure_notify(
            ClientByteOrder::BigEndian,
            22 | 0x80,
            0x1234,
            baseline_window,
            baseline_window,
            ResourceId(0x0000_0107),
            30,
            40,
            100,
            80,
            0,
            false,
        ),
    ];
    assert_eq!(
        event_frames(&baseline_a_move),
        expected_a_move,
        "complete no-policy ConfigureNotify frames for A"
    );
    assert_eq!(
        event_frames(&baseline_b_move),
        expected_b_move,
        "complete no-policy ConfigureNotify frames for B"
    );
    assert_eq!(
        baseline.state.resources.window(baseline_window).unwrap().x,
        30
    );
    let baseline_close_before = baseline.state.timestamp_now();
    host_request(
        &mut baseline,
        HostRequest::CloseRequested { id: baseline_id },
    );
    let baseline_close_after = baseline.state.timestamp_now();
    let baseline_close = baseline.drain(A);
    let baseline_timestamp = u32::from_le_bytes(baseline_close[16..20].try_into().unwrap());
    assert!(
        timestamp_is_between(
            baseline_timestamp,
            (baseline_close_before, baseline_close_after)
        ),
        "baseline ClientMessage timestamp {baseline_timestamp} is within ({baseline_close_before}, {baseline_close_after})"
    );
    let protocols = baseline.state.atoms.intern("WM_PROTOCOLS", false);
    let delete_window = baseline.state.atoms.intern("WM_DELETE_WINDOW", false);
    let mut baseline_close_without_timestamp = baseline_close.clone();
    baseline_close_without_timestamp[16..20].fill(0);
    assert_eq!(
        baseline_close_without_timestamp,
        literal_wm_delete_client_message(
            ClientByteOrder::LittleEndian,
            2,
            baseline_window,
            protocols,
            delete_window,
        ),
        "no-policy full ClientMessage frame with bounded timestamp normalized"
    );
    let baseline_requestor = ResourceId(0x0057_0011);
    create_window(&mut baseline, B, baseline_requestor);
    let baseline_selection = baseline
        .state
        .atoms
        .intern("ACCESS_HOST_NOTIFY_SELECTION", false);
    let baseline_target = baseline
        .state
        .atoms
        .intern("ACCESS_HOST_NOTIFY_TARGET", false);
    let baseline_property = baseline
        .state
        .atoms
        .intern("ACCESS_HOST_NOTIFY_PROPERTY", false);
    let mut baseline_notify = Vec::new();
    for property_value in [baseline_property, AtomId(0)] {
        rootless::selection::send_selection_notify(
            &mut baseline.state,
            0x4455_6677,
            baseline_window,
            [baseline_selection, baseline_target, property_value],
        )
        .unwrap();
        assert_eq!(
            baseline.drain(A),
            literal_synthetic_selection_notify(
                ClientByteOrder::LittleEndian,
                2,
                0x4455_6677,
                baseline_window,
                baseline_selection,
                baseline_target,
                property_value,
            ),
            "no-policy bridge-owned synthetic SelectionNotify"
        );
        rootless::selection::send_selection_notify(
            &mut baseline.state,
            0x4455_6677,
            baseline_requestor,
            [baseline_selection, baseline_target, property_value],
        )
        .unwrap();
        baseline_notify.push(baseline.drain(B));
        assert_eq!(
            baseline_notify.last().unwrap(),
            &literal_synthetic_selection_notify(
                ClientByteOrder::BigEndian,
                0x1234,
                0x4455_6677,
                baseline_requestor,
                baseline_selection,
                baseline_target,
                property_value,
            ),
            "no-policy socket-owned synthetic SelectionNotify"
        );
    }

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
    assert_eq!(control_a_move, baseline_a_move);
    assert_eq!(control_b_move, baseline_b_move);
    assert_eq!(control.state.resources.window(window).unwrap().x, 30);
    let control_geometry = host_geometry(&control, window);
    let control_wm_state = host_wm_state(&mut control, window);
    let control_backend = control.backend.calls();
    let control_protocols = control.state.atoms.intern("WM_PROTOCOLS", false);
    let delete_window = control.state.atoms.intern("WM_DELETE_WINDOW", false);
    let control_close_before = control.state.timestamp_now();
    host_request(&mut control, HostRequest::CloseRequested { id });
    let control_close_after = control.state.timestamp_now();
    let control_close = control.drain(A);
    let control_timestamp = u32::from_le_bytes(control_close[16..20].try_into().unwrap());
    assert!(
        timestamp_is_between(
            control_timestamp,
            (control_close_before, control_close_after)
        ),
        "recording-Allow ClientMessage timestamp {control_timestamp} is within ({control_close_before}, {control_close_after})"
    );
    let mut baseline_close_without_timestamp = baseline_close.clone();
    let mut control_close_without_timestamp = control_close.clone();
    baseline_close_without_timestamp[16..20].fill(0);
    control_close_without_timestamp[16..20].fill(0);
    assert_eq!(
        control_close_without_timestamp, baseline_close_without_timestamp,
        "all non-timestamp ClientMessage bytes match"
    );
    let mut control_close_without_timestamp = control_close.clone();
    control_close_without_timestamp[16..20].fill(0);
    assert_eq!(
        control_close_without_timestamp,
        literal_wm_delete_client_message(
            ClientByteOrder::LittleEndian,
            2,
            window,
            control_protocols,
            delete_window,
        ),
        "recording-Allow full ClientMessage frame with bounded timestamp normalized"
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
    assert_eq!(control_notify, baseline_notify);
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
