use super::{
    access_notify_support::{
        NOTIFY_RECEIVER_ALLOWED, NOTIFY_RECEIVER_DENIED, NOTIFY_SENDER, NotifyFixture,
        append_event_u16, append_event_u32, finish_literal_event, request_opcodes,
    },
    extensions::{ExtensionHook, RecordedExtensionCheck},
};
use crate::{
    access::{Decision, DenyError, EventDescription, EventSubject, RequestOpcodes},
    backend::{KeyboardMappingChange, XkbSetEvent, XkbSetOutcome},
    core_loop::{
        InputOrigin, key_fanout::key_event_fanout_to_state, process_request::RequestOutcome,
    },
    host_x11::HostKeyEvent,
};
use std::{
    sync::{Arc, Mutex, atomic::Ordering},
    time::Duration,
};
use yserver_protocol::x11::{ClientId, RequestHeader, SequenceNumber};

const XKB_EVENT_BASE: u8 = 85;
const XKB_MAJOR_OPCODE: u8 = 136;
const XKB_EVENT_DESCRIPTION: EventDescription = EventDescription::extension(85, 136, 0);
const STATE_DETAILS: u16 = 0x3fff;
const REPEAT_KEYS_MASK: u32 = 1;
const PER_KEY_REPEAT_MASK: u32 = 1 << 30;
const REPEAT_KEYCODE: u8 = 200;
const REQUEST_SEQUENCE: u16 = 0x2a6b;
const DENY: DenyError = DenyError {
    code: yserver_protocol::x11::error::BAD_ACCESS,
    value: 0,
};

fn xkb_request(
    fixture: &mut NotifyFixture,
    client: ClientId,
    sequence: u16,
    minor: u8,
    body: &[u8],
) -> RequestOutcome {
    fixture
        .request(
            client,
            SequenceNumber(sequence),
            RequestHeader {
                opcode: XKB_MAJOR_OPCODE,
                data: minor,
                length_units: u32::try_from(1 + body.len().div_ceil(4))
                    .expect("XKB request length fits u32"),
            },
            body,
        )
        .expect("XKB request through process_request")
}

fn initialize_xkb_client(fixture: &mut NotifyFixture, client: ClientId, state: u16, controls: u32) {
    assert!(matches!(
        xkb_request(fixture, client, 0x1010 + client.0 as u16, 0, &[1, 0, 0, 0]),
        RequestOutcome::Handled
    ));
    let reply = fixture.drain(client);
    assert_eq!(reply.len(), 32, "XkbUseExtension reply");
    assert_eq!(reply[0], 1, "XkbUseExtension supported");

    let mut affect_which = 0u16;
    if state != 0 {
        affect_which |= 1 << 2;
    }
    if controls != 0 {
        affect_which |= 1 << 3;
    }
    let mut body = Vec::with_capacity(20);
    body.extend_from_slice(&0x0100u16.to_le_bytes()); // XkbUseCoreKbd
    body.extend_from_slice(&affect_which.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes()); // clear
    body.extend_from_slice(&0u16.to_le_bytes()); // selectAll
    body.extend_from_slice(&0u16.to_le_bytes()); // affectMap
    body.extend_from_slice(&0u16.to_le_bytes()); // map
    if state != 0 {
        body.extend_from_slice(&state.to_le_bytes()); // affectState
        body.extend_from_slice(&state.to_le_bytes()); // state
    }
    if controls != 0 {
        body.extend_from_slice(&controls.to_le_bytes()); // affectControls
        body.extend_from_slice(&controls.to_le_bytes()); // controls
    }
    assert!(matches!(
        xkb_request(fixture, client, 0x1020 + client.0 as u16, 1, &body),
        RequestOutcome::Handled
    ));
    assert!(fixture.drain(client).is_empty(), "XkbSelectEvents is void");
}

fn notify_fixture(state: u16, controls: u32) -> NotifyFixture {
    let mut fixture = NotifyFixture::new();
    fixture.backend.xkb_info_result = Some((XKB_MAJOR_OPCODE, XKB_EVENT_BASE, 0));
    initialize_xkb_client(&mut fixture, NOTIFY_SENDER, 0, 0);
    initialize_xkb_client(&mut fixture, NOTIFY_RECEIVER_DENIED, state, controls);
    initialize_xkb_client(&mut fixture, NOTIFY_RECEIVER_ALLOWED, state, controls);
    for (client, sequence) in [
        (NOTIFY_RECEIVER_DENIED, 0x1234),
        (NOTIFY_RECEIVER_ALLOWED, 0x5678),
    ] {
        fixture
            .state
            .clients
            .get(&client.0)
            .expect("fixture client")
            .last_sequence
            .store(sequence, Ordering::Relaxed);
    }
    fixture.state.start_instant = std::time::Instant::now() + Duration::from_secs(30);
    fixture
}

fn lock_mods_body(affect: u8, values: u8) -> [u8; 12] {
    [0x00, 0x01, affect, values, 0, 0, 0, 0, 0, 0, 0, 0]
}

fn set_controls_body(delay: u16, interval: u16) -> [u8; 96] {
    let mut body = [0; 96];
    body[28..32].copy_from_slice(&REPEAT_KEYS_MASK.to_le_bytes());
    body[32..34].copy_from_slice(&delay.to_le_bytes());
    body[34..36].copy_from_slice(&interval.to_le_bytes());
    body
}

fn repeat_outcome() -> XkbSetOutcome {
    XkbSetOutcome {
        error: None,
        events: vec![XkbSetEvent::Notification(KeyboardMappingChange {
            map_notify: yserver_protocol::x11::XkbMapNotify::default(),
            num_groups: 2,
            enabled_controls: 0,
            repeats: vec![(200, true)],
            indicator_map_changed: 0,
            indicator_state: 0,
            compat_changed_groups: 0,
            compat_total_si: 0,
        })],
    }
}

fn script_repeat_outcome(fixture: &mut NotifyFixture) {
    let backend = std::mem::take(&mut fixture.backend);
    fixture.backend = backend.with_xkb_set_outcome(repeat_outcome());
}

fn drive_latch_lock(fixture: &mut NotifyFixture) {
    assert!(matches!(
        xkb_request(
            fixture,
            NOTIFY_SENDER,
            REQUEST_SEQUENCE,
            5,
            &lock_mods_body(0x10, 0x10)
        ),
        RequestOutcome::Handled
    ));
    assert_eq!(fixture.drain(NOTIFY_SENDER), Vec::<u8>::new());
    assert_no_writer_failures(fixture);
}

fn drive_set_controls(fixture: &mut NotifyFixture) {
    assert!(matches!(
        xkb_request(
            fixture,
            NOTIFY_SENDER,
            REQUEST_SEQUENCE,
            7,
            &set_controls_body(200, 20)
        ),
        RequestOutcome::Handled
    ));
    assert_eq!(fixture.drain(NOTIFY_SENDER), Vec::<u8>::new());
    assert_no_writer_failures(fixture);
}

fn drive_set_per_key_repeat(fixture: &mut NotifyFixture) {
    script_repeat_outcome(fixture);
    assert!(matches!(
        xkb_request(fixture, NOTIFY_SENDER, REQUEST_SEQUENCE, 9, &[0; 32]),
        RequestOutcome::Handled
    ));
    assert_eq!(fixture.drain(NOTIFY_SENDER), Vec::<u8>::new());
    assert_no_writer_failures(fixture);
}

fn literal_state_notify(
    sequence: u16,
    mods: u8,
    base_mods: u8,
    locked_mods: u8,
    changed: u16,
    keycode: u8,
    event_type: u8,
    request_major: u8,
    request_minor: u8,
) -> Vec<u8> {
    let mut event = vec![XKB_EVENT_BASE, 2]; // XkbStateNotify
    append_event_u16(
        &mut event,
        yserver_protocol::x11::ClientByteOrder::LittleEndian,
        sequence,
    );
    event.extend_from_slice(&[0; 4]); // timestamp
    event.extend_from_slice(&[
        1, // core keyboard device
        mods,
        base_mods,
        0, // latched mods
        locked_mods,
        0, // group
        0,
        0, // base group
        0,
        0, // latched group
        0, // locked group
        mods,
        mods,
        mods,
        mods,
        mods, // compat/grab/lookup state
        0,
        0, // pointer button state
    ]);
    append_event_u16(
        &mut event,
        yserver_protocol::x11::ClientByteOrder::LittleEndian,
        changed,
    );
    event.extend_from_slice(&[keycode, event_type, request_major, request_minor]);
    finish_literal_event(event)
}

fn literal_controls_notify(
    sequence: u16,
    num_groups: u8,
    changed_controls: u32,
    enabled_controls: u32,
    request_minor: u8,
) -> Vec<u8> {
    let mut event = vec![XKB_EVENT_BASE, 3]; // XkbControlsNotify
    append_event_u16(
        &mut event,
        yserver_protocol::x11::ClientByteOrder::LittleEndian,
        sequence,
    );
    event.extend_from_slice(&[0; 4]); // timestamp
    event.extend_from_slice(&[1, num_groups, 0, 0]);
    append_event_u32(
        &mut event,
        yserver_protocol::x11::ClientByteOrder::LittleEndian,
        changed_controls,
    );
    append_event_u32(
        &mut event,
        yserver_protocol::x11::ClientByteOrder::LittleEndian,
        enabled_controls,
    );
    append_event_u32(
        &mut event,
        yserver_protocol::x11::ClientByteOrder::LittleEndian,
        0,
    ); // enabled control changes
    event.extend_from_slice(&[0, 0, XKB_MAJOR_OPCODE, request_minor]);
    finish_literal_event(event)
}

fn append_frame(stream: &mut Vec<u8>, frame: Vec<u8>) {
    stream.extend(frame);
}

fn literal_request_stream(sequence: u16) -> Vec<u8> {
    let mut stream = literal_state_notify(sequence, 0x10, 0, 0x10, 0x1f09, 0, 0, 136, 5);
    append_frame(
        &mut stream,
        literal_controls_notify(sequence, 1, REPEAT_KEYS_MASK, 1, 7),
    );
    append_frame(
        &mut stream,
        literal_controls_notify(sequence, 2, PER_KEY_REPEAT_MASK, 1, 9),
    );
    stream
}

fn assert_no_writer_failures(fixture: &NotifyFixture) {
    assert!(
        crate::core_loop::client_io::failed_writers(&fixture.state.clients).is_empty(),
        "notification fanout has no failed writers"
    );
}

fn receive_log(calls: &Arc<Mutex<Vec<RecordedExtensionCheck>>>) -> Vec<RecordedExtensionCheck> {
    calls.lock().expect("policy call log").clone()
}

fn assert_receive_calls(
    calls: &Arc<Mutex<Vec<RecordedExtensionCheck>>>,
    clients: &[ClientId],
    request: Option<RequestOpcodes>,
) {
    assert_receive_calls_with_requests(calls, clients, &vec![request; clients.len()]);
}

fn assert_receive_calls_with_requests(
    calls: &Arc<Mutex<Vec<RecordedExtensionCheck>>>,
    clients: &[ClientId],
    requests: &[Option<RequestOpcodes>],
) {
    let log = receive_log(calls);
    let receives: Vec<_> = log
        .iter()
        .filter(|call| call.hook == ExtensionHook::Receive)
        .collect();
    assert_eq!(receives.len(), clients.len(), "whole-log Receive count");
    assert_eq!(requests.len(), clients.len(), "expected request contexts");
    assert!(
        log.iter().all(|call| call.hook != ExtensionHook::Send),
        "server-generated notifications run Receive without Send"
    );
    for ((call, client), request) in receives.iter().zip(clients).zip(requests) {
        assert_eq!(call.client, *client);
        assert_eq!(call.receive, Some(EventSubject::Device(1)));
        assert_eq!(
            call.events,
            Some(vec![EventDescription::extension(85, 136, 0)])
        );
        assert_eq!(call.request, *request);
    }
}

fn install_deny_for(
    fixture: &mut NotifyFixture,
    client: ClientId,
) -> Arc<Mutex<Vec<RecordedExtensionCheck>>> {
    fixture.install_policy(move |_, check| {
        if check.hook == ExtensionHook::Receive
            && check.client == client
            && check.receive == Some(EventSubject::Device(1))
            && check.events == Some(vec![XKB_EVENT_DESCRIPTION])
        {
            Decision::Deny(DENY)
        } else {
            Decision::Allow
        }
    })
}

// Mutation: change K11's encoded request_minor from 5; this literal byte
// baseline must fail even though the frame is still a valid XKB event.
#[test]
fn receive_xkb_state_no_policy_bytes() {
    let mut fixture = notify_fixture(STATE_DETAILS, REPEAT_KEYS_MASK | PER_KEY_REPEAT_MASK);
    assert!(!fixture.state.policies_installed());

    let mut denied_stream = Vec::new();
    let mut allowed_stream = Vec::new();
    drive_latch_lock(&mut fixture);
    append_frame(&mut denied_stream, fixture.drain(NOTIFY_RECEIVER_DENIED));
    append_frame(&mut allowed_stream, fixture.drain(NOTIFY_RECEIVER_ALLOWED));
    assert_eq!(
        denied_stream,
        literal_state_notify(0x1234, 0x10, 0, 0x10, 0x1f09, 0, 0, 136, 5)
    );
    assert_eq!(
        allowed_stream,
        literal_state_notify(0x5678, 0x10, 0, 0x10, 0x1f09, 0, 0, 136, 5)
    );

    drive_set_controls(&mut fixture);
    append_frame(&mut denied_stream, fixture.drain(NOTIFY_RECEIVER_DENIED));
    append_frame(&mut allowed_stream, fixture.drain(NOTIFY_RECEIVER_ALLOWED));
    let expected_denied_controls = literal_controls_notify(0x1234, 1, REPEAT_KEYS_MASK, 1, 7);
    let expected_allowed_controls = literal_controls_notify(0x5678, 1, REPEAT_KEYS_MASK, 1, 7);
    assert_eq!(
        denied_stream,
        [
            literal_state_notify(0x1234, 0x10, 0, 0x10, 0x1f09, 0, 0, 136, 5),
            expected_denied_controls.clone(),
        ]
        .concat()
    );
    assert_eq!(
        allowed_stream,
        [
            literal_state_notify(0x5678, 0x10, 0, 0x10, 0x1f09, 0, 0, 136, 5),
            expected_allowed_controls.clone(),
        ]
        .concat()
    );

    drive_set_per_key_repeat(&mut fixture);
    append_frame(&mut denied_stream, fixture.drain(NOTIFY_RECEIVER_DENIED));
    append_frame(&mut allowed_stream, fixture.drain(NOTIFY_RECEIVER_ALLOWED));
    assert_eq!(denied_stream, literal_request_stream(0x1234));
    assert_eq!(allowed_stream, literal_request_stream(0x5678));
    assert_eq!(fixture.backend.xkb_mods, (0x10, 0, 0, 0x10));
    assert_eq!(fixture.state.keyboard_control.repeat_delay, 200);
    assert_eq!(fixture.state.keyboard_control.repeat_interval, 20);
    assert!(fixture.state.keyboard_control.auto_repeats[25] & 1 != 0);

    // Physical-key K11 also keeps its complete no-policy event bytes. The
    // event has no request opcodes and follows the earlier request events.
    fixture.backend.xkb_mods = (0x40, 0x40, 0, 0);
    let dropped = key_event_fanout_to_state(
        &mut fixture.state,
        &mut fixture.backend,
        HostKeyEvent {
            origin: InputOrigin::NestedHost,
            pressed: true,
            keycode: 38,
            time: 0x5678,
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            state: 0,
        },
    );
    assert!(dropped.is_empty());
    append_frame(&mut denied_stream, fixture.drain(NOTIFY_RECEIVER_DENIED));
    append_frame(&mut allowed_stream, fixture.drain(NOTIFY_RECEIVER_ALLOWED));
    assert_eq!(
        denied_stream,
        [
            literal_request_stream(0x1234),
            literal_state_notify(0x1234, 0x40, 0x40, 0, 0x1f0b, 38, 2, 0, 0),
        ]
        .concat()
    );
    assert_eq!(
        allowed_stream,
        [
            literal_request_stream(0x5678),
            literal_state_notify(0x5678, 0x40, 0x40, 0, 0x1f0b, 38, 2, 0, 0),
        ]
        .concat()
    );
    assert_no_writer_failures(&fixture);
}

// Mutations: leave K11 unchecked, use EventSubject::Global, or describe the
// xkbType byte as the event type; each must invalidate the denial/log checks.
#[test]
fn receive_xkb_state_denied_one_recipient() {
    let mut denied = notify_fixture(STATE_DETAILS, 0);
    let calls = install_deny_for(&mut denied, NOTIFY_RECEIVER_DENIED);
    drive_latch_lock(&mut denied);
    assert_eq!(denied.drain(NOTIFY_RECEIVER_DENIED), Vec::<u8>::new());
    assert_eq!(
        denied.drain(NOTIFY_RECEIVER_ALLOWED),
        literal_state_notify(0x5678, 0x10, 0, 0x10, 0x1f09, 0, 0, 136, 5)
    );
    assert_receive_calls(
        &calls,
        &[NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED],
        request_opcodes(136, 5),
    );
    let denied_effect = (denied.backend.xkb_mods, denied.state.xkb_announced);

    let mut allowed = notify_fixture(STATE_DETAILS, 0);
    let allow_calls = allowed.install_policy(|_, _| Decision::Allow);
    drive_latch_lock(&mut allowed);
    assert_eq!(
        allowed.drain(NOTIFY_RECEIVER_DENIED),
        literal_state_notify(0x1234, 0x10, 0, 0x10, 0x1f09, 0, 0, 136, 5)
    );
    assert_eq!(
        allowed.drain(NOTIFY_RECEIVER_ALLOWED),
        literal_state_notify(0x5678, 0x10, 0, 0x10, 0x1f09, 0, 0, 136, 5)
    );
    assert_receive_calls(
        &allow_calls,
        &[NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED],
        request_opcodes(136, 5),
    );
    assert_eq!(
        denied_effect.0, allowed.backend.xkb_mods,
        "denial keeps modifiers"
    );
    assert_eq!(
        denied_effect.1, allowed.state.xkb_announced,
        "denial keeps the state effect"
    );
}

// Mutation: move xkb_announced advancement after the Receive denial
// short-circuit; denying every selected copy must still leave the snapshot new.
#[test]
fn receive_xkb_state_denied_snapshot_advances() {
    let mut denied = notify_fixture(STATE_DETAILS, 0);
    let calls = denied.install_policy(|_, check| {
        if check.hook == ExtensionHook::Receive {
            Decision::Deny(DENY)
        } else {
            Decision::Allow
        }
    });
    drive_latch_lock(&mut denied);
    assert_eq!(denied.drain(NOTIFY_RECEIVER_DENIED), Vec::<u8>::new());
    assert_eq!(denied.drain(NOTIFY_RECEIVER_ALLOWED), Vec::<u8>::new());
    assert_receive_calls(
        &calls,
        &[NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED],
        request_opcodes(136, 5),
    );
    let denied_effect = (denied.backend.xkb_mods, denied.state.xkb_announced);
    assert_eq!(denied.state.xkb_announced, (0, (0x10, 0, 0, 0x10)));

    let mut allowed = notify_fixture(STATE_DETAILS, 0);
    let allow_calls = allowed.install_policy(|_, _| Decision::Allow);
    drive_latch_lock(&mut allowed);
    assert_eq!(
        allowed.drain(NOTIFY_RECEIVER_DENIED),
        literal_state_notify(0x1234, 0x10, 0, 0x10, 0x1f09, 0, 0, 136, 5)
    );
    assert_eq!(
        allowed.drain(NOTIFY_RECEIVER_ALLOWED),
        literal_state_notify(0x5678, 0x10, 0, 0x10, 0x1f09, 0, 0, 136, 5)
    );
    assert_receive_calls(
        &allow_calls,
        &[NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED],
        request_opcodes(136, 5),
    );
    assert_eq!(denied_effect.0, allowed.backend.xkb_mods);
    assert_eq!(denied_effect.1, allowed.state.xkb_announced);
}

// Mutation: pass Some(RequestOpcodes { major: 0, minor: 0 }) on K11's key
// path; the physical event must be recorded with no request context.
#[test]
fn receive_xkb_state_from_key_has_no_request() {
    let mut fixture = notify_fixture(STATE_DETAILS, 0);
    let calls = fixture.install_policy(|_, _| Decision::Allow);
    fixture.backend.xkb_mods = (0x40, 0x40, 0, 0);
    let dropped = key_event_fanout_to_state(
        &mut fixture.state,
        &mut fixture.backend,
        HostKeyEvent {
            origin: InputOrigin::NestedHost,
            pressed: true,
            keycode: 38,
            time: 0x5678,
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            state: 0,
        },
    );
    assert!(dropped.is_empty());
    assert_eq!(
        fixture.drain(NOTIFY_RECEIVER_DENIED),
        literal_state_notify(0x1234, 0x40, 0x40, 0, 0x1f03, 38, 2, 0, 0)
    );
    assert_eq!(
        fixture.drain(NOTIFY_RECEIVER_ALLOWED),
        literal_state_notify(0x5678, 0x40, 0x40, 0, 0x1f03, 38, 2, 0, 0)
    );
    assert_receive_calls(
        &calls,
        &[NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED],
        None,
    );
    assert_no_writer_failures(&fixture);
}

// Mutation: leave K12 unchecked; the denied recipient's empty stream and the
// per-request Receive log must both fail.
#[test]
fn receive_xkb_controls_denied_one_recipient() {
    let mut denied = notify_fixture(0, REPEAT_KEYS_MASK);
    let calls = install_deny_for(&mut denied, NOTIFY_RECEIVER_DENIED);
    drive_set_controls(&mut denied);
    assert_eq!(denied.drain(NOTIFY_RECEIVER_DENIED), Vec::<u8>::new());
    assert_eq!(
        denied.drain(NOTIFY_RECEIVER_ALLOWED),
        literal_controls_notify(0x5678, 1, REPEAT_KEYS_MASK, 1, 7)
    );
    assert_receive_calls(
        &calls,
        &[NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED],
        request_opcodes(136, 7),
    );
    let denied_effect = (
        denied.state.keyboard_control.global_auto_repeat,
        denied.state.keyboard_control.repeat_delay,
        denied.state.keyboard_control.repeat_interval,
        denied.state.keyboard_control.auto_repeats,
    );

    let mut allowed = notify_fixture(0, REPEAT_KEYS_MASK);
    let allow_calls = allowed.install_policy(|_, _| Decision::Allow);
    drive_set_controls(&mut allowed);
    assert_eq!(
        allowed.drain(NOTIFY_RECEIVER_DENIED),
        literal_controls_notify(0x1234, 1, REPEAT_KEYS_MASK, 1, 7)
    );
    assert_eq!(
        allowed.drain(NOTIFY_RECEIVER_ALLOWED),
        literal_controls_notify(0x5678, 1, REPEAT_KEYS_MASK, 1, 7)
    );
    assert_receive_calls(
        &allow_calls,
        &[NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED],
        request_opcodes(136, 7),
    );
    assert_eq!(
        denied_effect,
        (
            allowed.state.keyboard_control.global_auto_repeat,
            allowed.state.keyboard_control.repeat_delay,
            allowed.state.keyboard_control.repeat_interval,
            allowed.state.keyboard_control.auto_repeats,
        ),
        "denial keeps SetControls state and enabled-controls value"
    );
    assert_eq!((denied_effect.1, denied_effect.2), (200, 20));
}

// Mutation: leave K9 unchecked; the denied client must not receive its
// per-key ControlsNotify while the core repeat bitmap still changes.
#[test]
fn receive_xkb_per_key_repeat_denied() {
    let mut denied = notify_fixture(0, PER_KEY_REPEAT_MASK);
    let calls = install_deny_for(&mut denied, NOTIFY_RECEIVER_DENIED);
    drive_set_per_key_repeat(&mut denied);
    assert_eq!(denied.drain(NOTIFY_RECEIVER_DENIED), Vec::<u8>::new());
    assert_eq!(
        denied.drain(NOTIFY_RECEIVER_ALLOWED),
        literal_controls_notify(0x5678, 2, PER_KEY_REPEAT_MASK, 1, 9)
    );
    assert_receive_calls(
        &calls,
        &[NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED],
        request_opcodes(136, 9),
    );
    let repeat_bit = 1u8 << (REPEAT_KEYCODE & 7);
    let denied_effect = (
        denied.state.keyboard_control.auto_repeats[usize::from(REPEAT_KEYCODE >> 3)] & repeat_bit,
        denied.state.keyboard_control.global_auto_repeat,
    );
    assert_eq!(denied_effect.0, repeat_bit);

    let mut allowed = notify_fixture(0, PER_KEY_REPEAT_MASK);
    let allow_calls = allowed.install_policy(|_, _| Decision::Allow);
    drive_set_per_key_repeat(&mut allowed);
    assert_eq!(
        allowed.drain(NOTIFY_RECEIVER_DENIED),
        literal_controls_notify(0x1234, 2, PER_KEY_REPEAT_MASK, 1, 9)
    );
    assert_eq!(
        allowed.drain(NOTIFY_RECEIVER_ALLOWED),
        literal_controls_notify(0x5678, 2, PER_KEY_REPEAT_MASK, 1, 9)
    );
    assert_receive_calls(
        &allow_calls,
        &[NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED],
        request_opcodes(136, 9),
    );
    assert_eq!(
        denied_effect,
        (
            allowed.state.keyboard_control.auto_repeats[usize::from(REPEAT_KEYCODE >> 3)]
                & repeat_bit,
            allowed.state.keyboard_control.global_auto_repeat,
        ),
        "denial keeps repeat bits and enabled-controls value"
    );
}

// Mutation: bypass K11 Receive when a policy is installed; the allow-all
// policy must still log one Receive per selected recipient per event.
#[test]
fn receive_xkb_state_allow_control() {
    let mut fixture = notify_fixture(STATE_DETAILS, REPEAT_KEYS_MASK | PER_KEY_REPEAT_MASK);
    let calls = fixture.install_policy(|_, _| Decision::Allow);
    let mut denied_stream = Vec::new();
    let mut allowed_stream = Vec::new();

    drive_latch_lock(&mut fixture);
    append_frame(&mut denied_stream, fixture.drain(NOTIFY_RECEIVER_DENIED));
    append_frame(&mut allowed_stream, fixture.drain(NOTIFY_RECEIVER_ALLOWED));
    drive_set_controls(&mut fixture);
    append_frame(&mut denied_stream, fixture.drain(NOTIFY_RECEIVER_DENIED));
    append_frame(&mut allowed_stream, fixture.drain(NOTIFY_RECEIVER_ALLOWED));
    drive_set_per_key_repeat(&mut fixture);
    append_frame(&mut denied_stream, fixture.drain(NOTIFY_RECEIVER_DENIED));
    append_frame(&mut allowed_stream, fixture.drain(NOTIFY_RECEIVER_ALLOWED));

    assert_eq!(denied_stream, literal_request_stream(0x1234));
    assert_eq!(allowed_stream, literal_request_stream(0x5678));
    assert_receive_calls_with_requests(
        &calls,
        &[
            NOTIFY_RECEIVER_DENIED,
            NOTIFY_RECEIVER_ALLOWED,
            NOTIFY_RECEIVER_DENIED,
            NOTIFY_RECEIVER_ALLOWED,
            NOTIFY_RECEIVER_DENIED,
            NOTIFY_RECEIVER_ALLOWED,
        ],
        &[
            request_opcodes(136, 5),
            request_opcodes(136, 5),
            request_opcodes(136, 7),
            request_opcodes(136, 7),
            request_opcodes(136, 9),
            request_opcodes(136, 9),
        ],
    );
}
