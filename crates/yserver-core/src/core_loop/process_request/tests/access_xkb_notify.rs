use super::{
    access_notify_support::{
        NOTIFY_RECEIVER_ALLOWED, NOTIFY_RECEIVER_DENIED, NOTIFY_SENDER, NotifyFixture,
        append_event_u16, append_event_u32, finish_literal_event, property_change_body,
        request_opcodes,
    },
    extensions::{ExtensionHook, RecordedExtensionCheck},
};
use crate::{
    access::{Decision, DenyError, EventDescription, EventSubject, RequestOpcodes},
    backend::{KeyboardMappingChange, XkbNewKeyboardInfo, XkbSetEvent, XkbSetOutcome},
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
const NON_XKB_CLIENT: ClientId = ClientId(4);
const KBD_BY_NAME_SEQUENCE: u16 = 0x3b7c;
const SET_MAP_SEQUENCE: u16 = 0x4c8d;
const DENY: DenyError = DenyError {
    code: yserver_protocol::x11::error::BAD_ACCESS,
    value: 0,
};

fn initialize_xkb_notifications(
    fixture: &mut NotifyFixture,
    client: ClientId,
    new_keyboard: bool,
    map_mask: u16,
) {
    assert!(matches!(
        xkb_request(fixture, client, 0x1010 + client.0 as u16, 0, &[1, 0, 0, 0]),
        RequestOutcome::Handled
    ));
    let reply = fixture.drain(client);
    assert_eq!(reply.len(), 32, "XkbUseExtension reply");
    assert_eq!(reply[0], 1, "XkbUseExtension supported");

    let which = (if new_keyboard { 1_u16 } else { 0 }) | (if map_mask != 0 { 2_u16 } else { 0 });
    let mut body = Vec::with_capacity(12);
    body.extend_from_slice(&0x0100u16.to_le_bytes()); // XkbUseCoreKbd
    body.extend_from_slice(&which.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes()); // clear
    body.extend_from_slice(&(if new_keyboard { 1_u16 } else { 0 }).to_le_bytes()); // selectAll
    body.extend_from_slice(&map_mask.to_le_bytes()); // affectMap
    body.extend_from_slice(&map_mask.to_le_bytes()); // map
    assert!(matches!(
        xkb_request(fixture, client, 0x1020 + client.0 as u16, 1, &body),
        RequestOutcome::Handled
    ));
    assert!(fixture.drain(client).is_empty(), "XkbSelectEvents is void");
}

fn xkb_event_fixture(
    new_keyboard_clients: &[ClientId],
    map_clients: &[(ClientId, u16)],
    add_non_xkb_client: bool,
) -> NotifyFixture {
    let mut fixture = NotifyFixture::new();
    fixture.backend.xkb_info_result = Some((XKB_MAJOR_OPCODE, XKB_EVENT_BASE, 0));
    for client in [
        NOTIFY_SENDER,
        NOTIFY_RECEIVER_DENIED,
        NOTIFY_RECEIVER_ALLOWED,
    ] {
        let new_keyboard = new_keyboard_clients.contains(&client);
        let map_mask = map_clients
            .iter()
            .find_map(|(selected, mask)| (*selected == client).then_some(*mask))
            .unwrap_or(0);
        initialize_xkb_notifications(&mut fixture, client, new_keyboard, map_mask);
    }
    if add_non_xkb_client {
        let peer = super::install_client(&mut fixture.state, NON_XKB_CLIENT.0);
        fixture.peers.insert(NON_XKB_CLIENT.0, peer);
        fixture
            .state
            .clients
            .get(&NON_XKB_CLIENT.0)
            .expect("plain X11 client")
            .last_sequence
            .store(0x6a2f, Ordering::Relaxed);
    }
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

fn xkb_new_keyboard_info(changed: u16) -> XkbNewKeyboardInfo {
    XkbNewKeyboardInfo {
        min_keycode: 8,
        max_keycode: 255,
        old_min_keycode: 8,
        old_max_keycode: 255,
        changed,
    }
}

fn drive_rules_names_change(
    fixture: &mut NotifyFixture,
    sequence: u16,
    names: &[u8],
) -> RequestOutcome {
    let property = fixture.state.atoms.intern("_XKB_RULES_NAMES", false);
    let body = property_change_body(crate::resources::ROOT_WINDOW, property, names);
    fixture
        .request(
            NOTIFY_SENDER,
            SequenceNumber(sequence),
            RequestHeader {
                opcode: 18,
                data: 0,
                length_units: u32::try_from(1 + body.len().div_ceil(4))
                    .expect("ChangeProperty length fits u32"),
            },
            &body,
        )
        .expect("_XKB_RULES_NAMES ChangeProperty through process_request")
}

fn script_xkb_set_outcome(fixture: &mut NotifyFixture, outcome: XkbSetOutcome) {
    let backend = std::mem::take(&mut fixture.backend);
    fixture.backend = backend.with_xkb_set_outcome(outcome);
}

fn drive_xkb_set_map(fixture: &mut NotifyFixture, sequence: u16) -> RequestOutcome {
    let mut body = [0; 32];
    body[..2].copy_from_slice(&0x0100u16.to_le_bytes()); // XkbUseCoreKbd
    fixture
        .request(
            NOTIFY_SENDER,
            SequenceNumber(sequence),
            RequestHeader {
                opcode: XKB_MAJOR_OPCODE,
                data: 8, // XkbSetMap
                length_units: 9,
            },
            &body,
        )
        .expect("XkbSetMap through process_request")
}

fn literal_mapping_notify(
    sequence: u16,
    order: yserver_protocol::x11::ClientByteOrder,
    request: u8,
    first_keycode: u8,
    count: u8,
) -> Vec<u8> {
    let mut event = vec![34, 0]; // MappingNotify, detail
    append_event_u16(&mut event, order, sequence);
    event.extend_from_slice(&[request, first_keycode, count]);
    finish_literal_event(event)
}

fn literal_xkb_new_keyboard_notify(
    sequence: u16,
    order: yserver_protocol::x11::ClientByteOrder,
    request_major: u8,
    request_minor: u8,
    changed: u16,
) -> Vec<u8> {
    let mut event = vec![XKB_EVENT_BASE, 0]; // XkbNewKeyboardNotify
    append_event_u16(&mut event, order, sequence);
    event.extend_from_slice(&[0; 4]); // timestamp
    event.extend_from_slice(&[1, 1, 8, 255, 8, 255, request_major, request_minor]);
    append_event_u16(&mut event, order, changed);
    finish_literal_event(event)
}

fn literal_xkb_map_notify(
    sequence: u16,
    order: yserver_protocol::x11::ClientByteOrder,
    device_id: u8,
    changed: u16,
    n_types: u8,
    first_key_sym: u8,
    n_key_syms: u8,
    first_mod_map_key: u8,
    n_mod_map_keys: u8,
) -> Vec<u8> {
    let mut event = vec![XKB_EVENT_BASE, 1]; // XkbMapNotify
    append_event_u16(&mut event, order, sequence);
    event.extend_from_slice(&[0; 4]); // timestamp
    event.extend_from_slice(&[device_id, 0]); // device, pointer-button actions
    append_event_u16(&mut event, order, changed);
    event.extend_from_slice(&[
        8,
        255,
        0,
        n_types,
        first_key_sym,
        n_key_syms,
        0,
        0, // key actions
        0,
        0, // key behaviors
        0,
        0, // explicit components
        first_mod_map_key,
        n_mod_map_keys,
        0,
        0, // virtual-modifier map
    ]);
    append_event_u16(&mut event, order, 0); // virtual modifiers
    finish_literal_event(event)
}

fn literal_mapping_pair(sequence: u16, order: yserver_protocol::x11::ClientByteOrder) -> Vec<u8> {
    [
        literal_mapping_notify(sequence, order, 1, 8, 248),
        literal_mapping_notify(sequence, order, 0, 0, 0),
    ]
    .concat()
}

fn literal_rules_names_stream(
    sequence: u16,
    order: yserver_protocol::x11::ClientByteOrder,
) -> Vec<u8> {
    [
        literal_mapping_pair(sequence, order),
        literal_xkb_new_keyboard_notify(sequence, order, 0, 0, 1),
        literal_xkb_map_notify(sequence, order, 1, 0x0007, 4, 8, 248, 8, 248),
    ]
    .concat()
}

fn literal_kbd_by_name_stream(
    sequence: u16,
    order: yserver_protocol::x11::ClientByteOrder,
    include_reply: bool,
) -> Vec<u8> {
    let mut stream = [
        literal_mapping_pair(sequence, order),
        literal_xkb_new_keyboard_notify(sequence, order, XKB_MAJOR_OPCODE, 23, 1),
    ]
    .concat();
    if include_reply {
        let mut reply = vec![0; 32];
        reply[0] = 1;
        match order {
            yserver_protocol::x11::ClientByteOrder::LittleEndian => {
                reply[2..4].copy_from_slice(&sequence.to_le_bytes());
            }
            yserver_protocol::x11::ClientByteOrder::BigEndian => {
                reply[2..4].copy_from_slice(&sequence.to_be_bytes());
            }
        }
        stream.extend(reply);
    }
    stream
}

fn literal_map_change_stream(
    sequence: u16,
    order: yserver_protocol::x11::ClientByteOrder,
) -> Vec<u8> {
    [
        literal_xkb_map_notify(sequence, order, 1, 0x0002, 0, 8, 1, 0, 0),
        literal_mapping_notify(sequence, order, 1, 8, 1),
    ]
    .concat()
}

fn kbd_by_name_backend(fixture: &mut NotifyFixture) {
    let mut reply = vec![0; 32];
    reply[0] = 1;
    let backend = std::mem::take(&mut fixture.backend);
    fixture.backend = backend.with_kbd_by_name_result(reply, Some(xkb_new_keyboard_info(1)));
}

fn run_xkb_kbd_by_name(
    install_policy: bool,
    deny_one: bool,
) -> (
    Vec<Vec<u8>>,
    Vec<RecordedExtensionCheck>,
    Vec<ClientId>,
    bool,
) {
    let mut fixture = NotifyFixture::new();
    fixture.backend.xkb_info_result = Some((XKB_MAJOR_OPCODE, XKB_EVENT_BASE, 0));
    initialize_xkb_notifications(&mut fixture, NOTIFY_SENDER, false, 0);
    fixture
        .state
        .clients
        .get_mut(&NOTIFY_RECEIVER_ALLOWED.0)
        .expect("big-endian non-XKB client")
        .byte_order = yserver_protocol::x11::ClientByteOrder::BigEndian;
    for (client, sequence) in [
        (NOTIFY_RECEIVER_DENIED, 0x1234),
        (NOTIFY_RECEIVER_ALLOWED, 0x5678),
    ] {
        fixture
            .state
            .clients
            .get(&client.0)
            .expect("non-XKB client")
            .last_sequence
            .store(sequence, Ordering::Relaxed);
    }
    fixture.state.start_instant = std::time::Instant::now() + Duration::from_secs(30);
    fixture.state.keymap_overrides.insert(8, vec![0x0061]);
    kbd_by_name_backend(&mut fixture);
    assert!(!crate::core_loop::xkb_select::xkb_initialized(
        &fixture.state,
        NOTIFY_RECEIVER_DENIED
    ));
    assert!(!crate::core_loop::xkb_select::xkb_initialized(
        &fixture.state,
        NOTIFY_RECEIVER_ALLOWED
    ));
    let receive_order: Vec<_> = fixture
        .state
        .clients
        .keys()
        .copied()
        .map(ClientId)
        .collect();
    let calls = install_policy.then(|| {
        fixture.install_policy(move |_, check| {
            if deny_one
                && check.hook == ExtensionHook::Receive
                && check.client == NOTIFY_RECEIVER_DENIED
                && check.receive == Some(EventSubject::Device(1))
                && check.events == Some(vec![XKB_EVENT_DESCRIPTION])
            {
                Decision::Deny(DENY)
            } else {
                Decision::Allow
            }
        })
    });
    assert!(matches!(
        xkb_request(&mut fixture, NOTIFY_SENDER, KBD_BY_NAME_SEQUENCE, 23, &[]),
        RequestOutcome::Handled
    ));
    assert_no_writer_failures(&fixture);
    let outputs = [
        NOTIFY_SENDER,
        NOTIFY_RECEIVER_DENIED,
        NOTIFY_RECEIVER_ALLOWED,
    ]
    .into_iter()
    .map(|client| fixture.drain(client))
    .collect();
    let calls = calls.map_or_else(Vec::new, |calls| calls.lock().unwrap().clone());
    let keymap_cleared = fixture.state.keymap_overrides.is_empty();
    (outputs, calls, receive_order, keymap_cleared)
}

fn assert_xkb_receive_log(
    calls: &[RecordedExtensionCheck],
    clients: &[ClientId],
    subject: EventSubject,
    request: Option<RequestOpcodes>,
) {
    let receive_count = calls
        .iter()
        .filter(|call| call.hook == ExtensionHook::Receive)
        .count();
    assert_eq!(receive_count, clients.len(), "whole-log Receive count");
    assert!(
        calls.iter().all(|call| call.hook != ExtensionHook::Send),
        "server-generated notifications run Receive without Send"
    );
    let receives: Vec<_> = calls
        .iter()
        .filter(|call| call.hook == ExtensionHook::Receive)
        .collect();
    for (call, client) in receives.into_iter().zip(clients) {
        assert_eq!(call.client, *client);
        assert_eq!(call.receive, Some(subject));
        assert_eq!(call.events, Some(vec![XKB_EVENT_DESCRIPTION]));
        assert_eq!(call.request, request);
    }
}

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
    let mut body = [0; 32];
    body[..2].copy_from_slice(&0x0100u16.to_le_bytes()); // XkbUseCoreKbd
    body[6] = 8; // minimum legal keycode
    body[7] = u8::MAX;
    assert!(matches!(
        xkb_request(fixture, NOTIFY_SENDER, REQUEST_SEQUENCE, 9, &body),
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

fn literal_request_stream_prefix(sequence: u16, frame_count: usize) -> Vec<u8> {
    literal_request_stream(sequence)
        .chunks_exact(32)
        .take(frame_count)
        .flatten()
        .copied()
        .collect()
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
    assert_eq!(denied_stream, literal_request_stream_prefix(0x1234, 1));
    assert_eq!(allowed_stream, literal_request_stream_prefix(0x5678, 1));

    drive_set_controls(&mut fixture);
    append_frame(&mut denied_stream, fixture.drain(NOTIFY_RECEIVER_DENIED));
    append_frame(&mut allowed_stream, fixture.drain(NOTIFY_RECEIVER_ALLOWED));
    assert_eq!(denied_stream, literal_request_stream_prefix(0x1234, 2));
    assert_eq!(allowed_stream, literal_request_stream_prefix(0x5678, 2));

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

// Mutation: put xkb_announced advancement after a denial-driven early return;
// denying every selected copy must still leave the snapshot new.
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

// Mutations: leave K9 unchecked, or change another repeat bit only in the
// denied run; the stream and full bitmap comparison must catch both.
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
    let repeat_byte = usize::from(REPEAT_KEYCODE >> 3);
    let denied_effect = (
        denied.state.keyboard_control.auto_repeats,
        denied.state.keyboard_control.global_auto_repeat,
    );
    assert_ne!(denied_effect.0[repeat_byte] & repeat_bit, 0);

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
            allowed.state.keyboard_control.auto_repeats,
            allowed.state.keyboard_control.global_auto_repeat,
        ),
        "denial keeps the full repeat bitmap and enabled-controls value"
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

// Mutation: change the literal K4 MapNotify changed mask from KeySyms (0x0002).
// The byte baseline covers K1-K4 event order and each recipient's sequence.
#[test]
fn receive_xkb_new_keyboard_no_policy_bytes() {
    let selected = [
        NOTIFY_SENDER,
        NOTIFY_RECEIVER_DENIED,
        NOTIFY_RECEIVER_ALLOWED,
    ];
    let map_selected = selected.map(|client| (client, 0x0007));
    let mut fixture = xkb_event_fixture(&selected, &map_selected, true);
    assert!(!fixture.state.policies_installed());
    let backend = std::mem::take(&mut fixture.backend);
    fixture.backend = backend.with_keymap_rmlvo_result((8, 255));

    let outcome =
        drive_rules_names_change(&mut fixture, REQUEST_SEQUENCE, b"evdev\0pc105\0de\0\0\0");
    assert!(matches!(outcome, RequestOutcome::Handled));
    assert_eq!(
        fixture.drain(NOTIFY_SENDER),
        literal_rules_names_stream(
            REQUEST_SEQUENCE,
            yserver_protocol::x11::ClientByteOrder::LittleEndian
        )
    );
    assert_eq!(
        fixture.drain(NOTIFY_RECEIVER_DENIED),
        literal_rules_names_stream(0x1234, yserver_protocol::x11::ClientByteOrder::LittleEndian)
    );
    assert_eq!(
        fixture.drain(NOTIFY_RECEIVER_ALLOWED),
        literal_rules_names_stream(0x5678, yserver_protocol::x11::ClientByteOrder::LittleEndian)
    );
    assert_eq!(
        fixture.drain(NON_XKB_CLIENT),
        literal_mapping_pair(0x6a2f, yserver_protocol::x11::ClientByteOrder::LittleEndian)
    );
    assert_no_writer_failures(&fixture);

    kbd_by_name_backend(&mut fixture);
    let outcome = xkb_request(&mut fixture, NOTIFY_SENDER, KBD_BY_NAME_SEQUENCE, 23, &[]);
    assert!(matches!(outcome, RequestOutcome::Handled));
    assert_eq!(
        fixture.drain(NOTIFY_SENDER),
        literal_kbd_by_name_stream(
            KBD_BY_NAME_SEQUENCE,
            yserver_protocol::x11::ClientByteOrder::LittleEndian,
            true,
        )
    );
    for (client, sequence) in [
        (NOTIFY_RECEIVER_DENIED, 0x1234),
        (NOTIFY_RECEIVER_ALLOWED, 0x5678),
    ] {
        assert_eq!(
            fixture.drain(client),
            literal_kbd_by_name_stream(
                sequence,
                yserver_protocol::x11::ClientByteOrder::LittleEndian,
                false,
            )
        );
    }
    assert_eq!(
        fixture.drain(NON_XKB_CLIENT),
        literal_kbd_by_name_stream(
            0x6a2f,
            yserver_protocol::x11::ClientByteOrder::LittleEndian,
            false,
        )
    );
    assert_no_writer_failures(&fixture);

    script_xkb_set_outcome(
        &mut fixture,
        XkbSetOutcome {
            error: None,
            events: vec![
                XkbSetEvent::NewKeyboard(xkb_new_keyboard_info(1)),
                XkbSetEvent::Repeats(vec![(REPEAT_KEYCODE, true)]),
            ],
        },
    );
    let outcome = drive_xkb_set_map(&mut fixture, SET_MAP_SEQUENCE);
    assert!(matches!(outcome, RequestOutcome::Handled));
    for (client, sequence) in [
        (NOTIFY_SENDER, SET_MAP_SEQUENCE),
        (NOTIFY_RECEIVER_DENIED, 0x1234),
        (NOTIFY_RECEIVER_ALLOWED, 0x5678),
    ] {
        assert_eq!(
            fixture.drain(client),
            literal_xkb_new_keyboard_notify(
                sequence,
                yserver_protocol::x11::ClientByteOrder::LittleEndian,
                XKB_MAJOR_OPCODE,
                8,
                1,
            )
        );
    }
    assert_eq!(
        fixture.drain(NON_XKB_CLIENT),
        literal_mapping_pair(0x6a2f, yserver_protocol::x11::ClientByteOrder::LittleEndian)
    );
    assert_ne!(
        fixture.state.keyboard_control.auto_repeats[usize::from(REPEAT_KEYCODE >> 3)]
            & (1 << (REPEAT_KEYCODE & 7)),
        0,
        "scripted Repeats still updates core key repeat state"
    );
    assert_no_writer_failures(&fixture);

    script_xkb_set_outcome(
        &mut fixture,
        XkbSetOutcome {
            error: None,
            events: vec![XkbSetEvent::Notification(KeyboardMappingChange {
                map_notify: yserver_protocol::x11::XkbMapNotify {
                    device_id: 1,
                    changed: 0x0002,
                    min_keycode: 8,
                    max_keycode: 255,
                    first_key_sym: 8,
                    n_key_syms: 1,
                    ..Default::default()
                },
                num_groups: 2,
                enabled_controls: 0,
                repeats: Vec::new(),
                indicator_map_changed: 0,
                indicator_state: 0,
                compat_changed_groups: 0,
                compat_total_si: 0,
            })],
        },
    );
    let outcome = drive_xkb_set_map(&mut fixture, 0x5d9e);
    assert!(matches!(outcome, RequestOutcome::Handled));
    for (client, sequence) in [
        (NOTIFY_SENDER, 0x5d9e),
        (NOTIFY_RECEIVER_DENIED, 0x1234),
        (NOTIFY_RECEIVER_ALLOWED, 0x5678),
    ] {
        assert_eq!(
            fixture.drain(client),
            literal_map_change_stream(
                sequence,
                yserver_protocol::x11::ClientByteOrder::LittleEndian,
            )
        );
    }
    assert_eq!(
        fixture.drain(NON_XKB_CLIENT),
        literal_mapping_notify(
            0x6a2f,
            yserver_protocol::x11::ClientByteOrder::LittleEndian,
            1,
            8,
            1,
        )
    );
    assert_no_writer_failures(&fixture);
}

// Mutations: leave K2 unchecked, or encode the NewKeyboardNotify for an
// uninitialised client in little-endian order; the literal streams and log fail.
#[test]
fn receive_xkb_kbd_by_name_all_clients() {
    let (baseline, baseline_calls, _, baseline_cleared) = run_xkb_kbd_by_name(false, false);
    assert!(baseline_calls.is_empty(), "no policy means no Receive call");
    assert!(baseline_cleared, "successful load clears keymap overrides");
    assert_eq!(
        baseline[0],
        literal_kbd_by_name_stream(
            KBD_BY_NAME_SEQUENCE,
            yserver_protocol::x11::ClientByteOrder::LittleEndian,
            true,
        )
    );
    assert_eq!(
        baseline[1],
        literal_kbd_by_name_stream(
            0x1234,
            yserver_protocol::x11::ClientByteOrder::LittleEndian,
            false,
        )
    );
    assert_eq!(
        baseline[2],
        literal_kbd_by_name_stream(
            0x5678,
            yserver_protocol::x11::ClientByteOrder::BigEndian,
            false,
        )
    );

    let (denied, calls, receive_order, denied_cleared) = run_xkb_kbd_by_name(true, true);
    assert!(denied_cleared, "Receive denial keeps keymap replacement");
    assert_eq!(
        denied[0],
        literal_kbd_by_name_stream(
            KBD_BY_NAME_SEQUENCE,
            yserver_protocol::x11::ClientByteOrder::LittleEndian,
            true,
        )
    );
    assert_eq!(
        denied[1],
        literal_mapping_pair(0x1234, yserver_protocol::x11::ClientByteOrder::LittleEndian),
        "only the denied non-XKB client's NewKeyboardNotify is dropped"
    );
    assert_eq!(
        denied[2],
        literal_kbd_by_name_stream(
            0x5678,
            yserver_protocol::x11::ClientByteOrder::BigEndian,
            false,
        )
    );
    assert_xkb_receive_log(
        &calls,
        &receive_order,
        EventSubject::Device(1),
        request_opcodes(XKB_MAJOR_OPCODE, 23),
    );

    let (allowed, allow_calls, allow_order, allow_cleared) = run_xkb_kbd_by_name(true, false);
    assert!(allow_cleared);
    assert_eq!(
        allowed[0],
        literal_kbd_by_name_stream(
            KBD_BY_NAME_SEQUENCE,
            yserver_protocol::x11::ClientByteOrder::LittleEndian,
            true,
        )
    );
    assert_eq!(
        allowed[1],
        literal_kbd_by_name_stream(
            0x1234,
            yserver_protocol::x11::ClientByteOrder::LittleEndian,
            false,
        )
    );
    assert_eq!(
        allowed[2],
        literal_kbd_by_name_stream(
            0x5678,
            yserver_protocol::x11::ClientByteOrder::BigEndian,
            false,
        )
    );
    assert_xkb_receive_log(
        &allow_calls,
        &allow_order,
        EventSubject::Device(1),
        request_opcodes(XKB_MAJOR_OPCODE, 23),
    );
}

// Mutations: pass None instead of state.receive_request at K1, or check K1 as
// EventSubject::Global; both break the per-recipient Receive assertions.
#[test]
fn receive_xkb_rules_change_denies_new_keyboard_and_map() {
    let selected = [
        NOTIFY_SENDER,
        NOTIFY_RECEIVER_DENIED,
        NOTIFY_RECEIVER_ALLOWED,
    ];
    let map_selected = selected.map(|client| (client, 0x0007));
    let mut denied = xkb_event_fixture(&selected, &map_selected, false);
    let backend = std::mem::take(&mut denied.backend);
    denied.backend = backend.with_keymap_rmlvo_result((8, 255));
    denied.state.keymap_overrides.insert(38, vec![0x0071]);
    let calls = install_deny_for(&mut denied, NOTIFY_RECEIVER_DENIED);
    assert!(matches!(
        drive_rules_names_change(&mut denied, REQUEST_SEQUENCE, b"evdev\0pc105\0de\0\0\0"),
        RequestOutcome::Handled
    ));
    assert_eq!(
        denied.drain(NOTIFY_SENDER),
        literal_rules_names_stream(
            REQUEST_SEQUENCE,
            yserver_protocol::x11::ClientByteOrder::LittleEndian
        )
    );
    assert_eq!(
        denied.drain(NOTIFY_RECEIVER_DENIED),
        literal_mapping_pair(0x1234, yserver_protocol::x11::ClientByteOrder::LittleEndian)
    );
    assert_eq!(
        denied.drain(NOTIFY_RECEIVER_ALLOWED),
        literal_rules_names_stream(0x5678, yserver_protocol::x11::ClientByteOrder::LittleEndian)
    );
    assert_receive_calls_with_requests(
        &calls,
        &[
            NOTIFY_SENDER,
            NOTIFY_RECEIVER_DENIED,
            NOTIFY_RECEIVER_ALLOWED,
            NOTIFY_SENDER,
            NOTIFY_RECEIVER_DENIED,
            NOTIFY_RECEIVER_ALLOWED,
        ],
        &[
            request_opcodes(18, 0),
            request_opcodes(18, 0),
            request_opcodes(18, 0),
            request_opcodes(18, 0),
            request_opcodes(18, 0),
            request_opcodes(18, 0),
        ],
    );
    let denied_overrides = denied.state.keymap_overrides.clone();
    assert!(
        denied_overrides.is_empty(),
        "denial does not skip keymap replacement"
    );
    assert_no_writer_failures(&denied);

    let mut allowed = xkb_event_fixture(&selected, &map_selected, false);
    let backend = std::mem::take(&mut allowed.backend);
    allowed.backend = backend.with_keymap_rmlvo_result((8, 255));
    allowed.state.keymap_overrides.insert(38, vec![0x0071]);
    let allow_calls = allowed.install_policy(|_, _| Decision::Allow);
    assert!(matches!(
        drive_rules_names_change(&mut allowed, REQUEST_SEQUENCE, b"evdev\0pc105\0de\0\0\0"),
        RequestOutcome::Handled
    ));
    for (client, sequence) in [
        (NOTIFY_SENDER, REQUEST_SEQUENCE),
        (NOTIFY_RECEIVER_DENIED, 0x1234),
        (NOTIFY_RECEIVER_ALLOWED, 0x5678),
    ] {
        assert_eq!(
            allowed.drain(client),
            literal_rules_names_stream(
                sequence,
                yserver_protocol::x11::ClientByteOrder::LittleEndian
            )
        );
    }
    assert_receive_calls_with_requests(
        &allow_calls,
        &[
            NOTIFY_SENDER,
            NOTIFY_RECEIVER_DENIED,
            NOTIFY_RECEIVER_ALLOWED,
            NOTIFY_SENDER,
            NOTIFY_RECEIVER_DENIED,
            NOTIFY_RECEIVER_ALLOWED,
        ],
        &[
            request_opcodes(18, 0),
            request_opcodes(18, 0),
            request_opcodes(18, 0),
            request_opcodes(18, 0),
            request_opcodes(18, 0),
            request_opcodes(18, 0),
        ],
    );
    assert_eq!(denied_overrides, allowed.state.keymap_overrides);
    assert_no_writer_failures(&allowed);
}

fn run_xkb_set_new_keyboard(
    install_policy: bool,
    deny_one: bool,
) -> (
    Vec<Vec<u8>>,
    Vec<RecordedExtensionCheck>,
    [u8; 32],
    RequestOutcome,
) {
    let selected = [
        NOTIFY_SENDER,
        NOTIFY_RECEIVER_DENIED,
        NOTIFY_RECEIVER_ALLOWED,
    ];
    let mut fixture = xkb_event_fixture(&selected, &[], true);
    let repeat_bit = 1 << (REPEAT_KEYCODE & 7);
    let repeat_byte = usize::from(REPEAT_KEYCODE >> 3);
    fixture.state.keyboard_control.auto_repeats[repeat_byte] &= !repeat_bit;
    let calls = install_policy.then(|| {
        fixture.install_policy(move |_, check| {
            if deny_one
                && check.hook == ExtensionHook::Receive
                && check.client == NOTIFY_RECEIVER_DENIED
                && check.receive == Some(EventSubject::Device(1))
                && check.events == Some(vec![XKB_EVENT_DESCRIPTION])
            {
                Decision::Deny(DENY)
            } else {
                Decision::Allow
            }
        })
    });
    script_xkb_set_outcome(
        &mut fixture,
        XkbSetOutcome {
            error: None,
            events: vec![
                XkbSetEvent::NewKeyboard(xkb_new_keyboard_info(1)),
                XkbSetEvent::Repeats(vec![(REPEAT_KEYCODE, true)]),
            ],
        },
    );
    let outcome = drive_xkb_set_map(&mut fixture, SET_MAP_SEQUENCE);
    assert_no_writer_failures(&fixture);
    let outputs = [
        NOTIFY_SENDER,
        NOTIFY_RECEIVER_DENIED,
        NOTIFY_RECEIVER_ALLOWED,
        NON_XKB_CLIENT,
    ]
    .into_iter()
    .map(|client| fixture.drain(client))
    .collect();
    let calls = calls.map_or_else(Vec::new, |calls| calls.lock().unwrap().clone());
    let repeats = fixture.state.keyboard_control.auto_repeats;
    (outputs, calls, repeats, outcome)
}

// Mutation: leave K3 unchecked; the denied client's stream and the complete
// Receive call log must fail while repeats and legacy MappingNotify stay.
#[test]
fn receive_xkb_set_new_keyboard_denied() {
    let (denied, calls, denied_repeats, outcome) = run_xkb_set_new_keyboard(true, true);
    assert!(matches!(outcome, RequestOutcome::Handled));
    assert_eq!(
        denied[0],
        literal_xkb_new_keyboard_notify(
            SET_MAP_SEQUENCE,
            yserver_protocol::x11::ClientByteOrder::LittleEndian,
            XKB_MAJOR_OPCODE,
            8,
            1,
        ),
        "requester's complete XkbSetMap notification stream"
    );
    assert!(
        denied[1].is_empty(),
        "denied recipient gets no NewKeyboardNotify"
    );
    assert_eq!(
        denied[2],
        literal_xkb_new_keyboard_notify(
            0x5678,
            yserver_protocol::x11::ClientByteOrder::LittleEndian,
            XKB_MAJOR_OPCODE,
            8,
            1,
        )
    );
    let legacy = literal_mapping_pair(0x6a2f, yserver_protocol::x11::ClientByteOrder::LittleEndian);
    assert_eq!(denied[3], legacy, "legacy core MappingNotify is unchanged");
    assert_xkb_receive_log(
        &calls,
        &[
            NOTIFY_SENDER,
            NOTIFY_RECEIVER_DENIED,
            NOTIFY_RECEIVER_ALLOWED,
        ],
        EventSubject::Device(1),
        request_opcodes(XKB_MAJOR_OPCODE, 8),
    );
    let repeat_bit = 1 << (REPEAT_KEYCODE & 7);
    let repeat_byte = usize::from(REPEAT_KEYCODE >> 3);
    assert_ne!(denied_repeats[repeat_byte] & repeat_bit, 0);

    let (allowed, allow_calls, allow_repeats, allow_outcome) =
        run_xkb_set_new_keyboard(true, false);
    assert!(matches!(allow_outcome, RequestOutcome::Handled));
    assert_eq!(
        allowed[0],
        literal_xkb_new_keyboard_notify(
            SET_MAP_SEQUENCE,
            yserver_protocol::x11::ClientByteOrder::LittleEndian,
            XKB_MAJOR_OPCODE,
            8,
            1,
        )
    );
    assert_eq!(
        allowed[1],
        literal_xkb_new_keyboard_notify(
            0x1234,
            yserver_protocol::x11::ClientByteOrder::LittleEndian,
            XKB_MAJOR_OPCODE,
            8,
            1,
        )
    );
    assert_eq!(
        allowed[2],
        literal_xkb_new_keyboard_notify(
            0x5678,
            yserver_protocol::x11::ClientByteOrder::LittleEndian,
            XKB_MAJOR_OPCODE,
            8,
            1,
        )
    );
    assert_eq!(allowed[3], legacy);
    assert_xkb_receive_log(
        &allow_calls,
        &[
            NOTIFY_SENDER,
            NOTIFY_RECEIVER_DENIED,
            NOTIFY_RECEIVER_ALLOWED,
        ],
        EventSubject::Device(1),
        request_opcodes(XKB_MAJOR_OPCODE, 8),
    );
    assert_eq!(
        denied_repeats, allow_repeats,
        "denial keeps the repeat state effect"
    );
}

fn run_xkb_map_change(
    install_policy: bool,
    deny_selected: bool,
) -> (Vec<Vec<u8>>, Vec<RecordedExtensionCheck>, RequestOutcome) {
    let map_clients = [(NOTIFY_RECEIVER_DENIED, 0x0002)];
    let mut fixture = xkb_event_fixture(&[], &map_clients, true);
    let calls = install_policy.then(|| {
        fixture.install_policy(move |_, check| {
            if deny_selected
                && check.hook == ExtensionHook::Receive
                && check.client == NOTIFY_RECEIVER_DENIED
                && check.receive == Some(EventSubject::Device(1))
                && check.events == Some(vec![XKB_EVENT_DESCRIPTION])
            {
                Decision::Deny(DENY)
            } else {
                Decision::Allow
            }
        })
    });
    script_xkb_set_outcome(
        &mut fixture,
        XkbSetOutcome {
            error: None,
            events: vec![XkbSetEvent::Notification(KeyboardMappingChange {
                map_notify: yserver_protocol::x11::XkbMapNotify {
                    device_id: 1,
                    changed: 0x0002,
                    min_keycode: 8,
                    max_keycode: 255,
                    first_key_sym: 8,
                    n_key_syms: 1,
                    ..Default::default()
                },
                num_groups: 2,
                enabled_controls: 0,
                repeats: Vec::new(),
                indicator_map_changed: 0,
                indicator_state: 0,
                compat_changed_groups: 0,
                compat_total_si: 0,
            })],
        },
    );
    let outcome = drive_xkb_set_map(&mut fixture, SET_MAP_SEQUENCE);
    assert_no_writer_failures(&fixture);
    let outputs = [
        NOTIFY_SENDER,
        NOTIFY_RECEIVER_DENIED,
        NOTIFY_RECEIVER_ALLOWED,
        NON_XKB_CLIENT,
    ]
    .into_iter()
    .map(|client| fixture.drain(client))
    .collect();
    let calls = calls.map_or_else(Vec::new, |calls| calls.lock().unwrap().clone());
    (outputs, calls, outcome)
}

// Mutation: filter core MappingNotify by the XKB MapNotify Receive outcome.
// The selected client must keep its core copy when its XKB copy is denied.
#[test]
fn receive_xkb_map_denied_keeps_legacy_filter() {
    let (baseline, baseline_calls, baseline_outcome) = run_xkb_map_change(false, false);
    assert!(matches!(baseline_outcome, RequestOutcome::Handled));
    assert!(baseline_calls.is_empty());
    assert!(
        baseline[0].is_empty(),
        "requester selected neither map event"
    );
    assert_eq!(
        baseline[1],
        literal_map_change_stream(0x1234, yserver_protocol::x11::ClientByteOrder::LittleEndian)
    );
    assert!(
        baseline[2].is_empty(),
        "unselected XKB client gets no legacy copy"
    );
    assert_eq!(
        baseline[3],
        literal_mapping_notify(
            0x6a2f,
            yserver_protocol::x11::ClientByteOrder::LittleEndian,
            1,
            8,
            1,
        )
    );

    let (denied, calls, outcome) = run_xkb_map_change(true, true);
    assert!(matches!(outcome, RequestOutcome::Handled));
    assert!(denied[0].is_empty());
    assert_eq!(
        denied[1],
        literal_mapping_notify(
            0x1234,
            yserver_protocol::x11::ClientByteOrder::LittleEndian,
            1,
            8,
            1,
        ),
        "denying MapNotify leaves the selected client's core MappingNotify"
    );
    assert!(
        denied[2].is_empty(),
        "unselected XKB client gets no core copy"
    );
    assert_eq!(
        denied[3],
        literal_mapping_notify(
            0x6a2f,
            yserver_protocol::x11::ClientByteOrder::LittleEndian,
            1,
            8,
            1,
        )
    );
    assert_xkb_receive_log(
        &calls,
        &[NOTIFY_RECEIVER_DENIED],
        EventSubject::Device(1),
        request_opcodes(XKB_MAJOR_OPCODE, 8),
    );
}

// Mutation: bypass K1's NewKeyboardNotify Receive call; this explicit Allow
// policy must still record it while preserving literal bytes.
#[test]
fn receive_xkb_new_keyboard_allow_control() {
    let selected = [
        NOTIFY_SENDER,
        NOTIFY_RECEIVER_DENIED,
        NOTIFY_RECEIVER_ALLOWED,
    ];
    let map_selected = selected.map(|client| (client, 0x0007));
    let mut fixture = xkb_event_fixture(&selected, &map_selected, false);
    let calls = fixture.install_policy(|_, _| Decision::Allow);
    let backend = std::mem::take(&mut fixture.backend);
    fixture.backend = backend.with_keymap_rmlvo_result((8, 255));
    assert!(matches!(
        drive_rules_names_change(&mut fixture, REQUEST_SEQUENCE, b"evdev\0pc105\0de\0\0\0"),
        RequestOutcome::Handled
    ));
    for (client, sequence) in [
        (NOTIFY_SENDER, REQUEST_SEQUENCE),
        (NOTIFY_RECEIVER_DENIED, 0x1234),
        (NOTIFY_RECEIVER_ALLOWED, 0x5678),
    ] {
        assert_eq!(
            fixture.drain(client),
            literal_rules_names_stream(
                sequence,
                yserver_protocol::x11::ClientByteOrder::LittleEndian
            )
        );
    }
    assert_no_writer_failures(&fixture);

    kbd_by_name_backend(&mut fixture);
    let kbd_receive_order: Vec<_> = fixture
        .state
        .clients
        .keys()
        .copied()
        .map(ClientId)
        .collect();
    assert!(matches!(
        xkb_request(&mut fixture, NOTIFY_SENDER, KBD_BY_NAME_SEQUENCE, 23, &[]),
        RequestOutcome::Handled
    ));
    assert_eq!(
        fixture.drain(NOTIFY_SENDER),
        literal_kbd_by_name_stream(
            KBD_BY_NAME_SEQUENCE,
            yserver_protocol::x11::ClientByteOrder::LittleEndian,
            true,
        )
    );
    assert_eq!(
        fixture.drain(NOTIFY_RECEIVER_DENIED),
        literal_kbd_by_name_stream(
            0x1234,
            yserver_protocol::x11::ClientByteOrder::LittleEndian,
            false,
        )
    );
    assert_eq!(
        fixture.drain(NOTIFY_RECEIVER_ALLOWED),
        literal_kbd_by_name_stream(
            0x5678,
            yserver_protocol::x11::ClientByteOrder::LittleEndian,
            false,
        )
    );
    assert_no_writer_failures(&fixture);

    script_xkb_set_outcome(
        &mut fixture,
        XkbSetOutcome {
            error: None,
            events: vec![
                XkbSetEvent::NewKeyboard(xkb_new_keyboard_info(1)),
                XkbSetEvent::Repeats(vec![(REPEAT_KEYCODE, true)]),
            ],
        },
    );
    assert!(matches!(
        drive_xkb_set_map(&mut fixture, SET_MAP_SEQUENCE),
        RequestOutcome::Handled
    ));
    for (client, sequence) in [
        (NOTIFY_SENDER, SET_MAP_SEQUENCE),
        (NOTIFY_RECEIVER_DENIED, 0x1234),
        (NOTIFY_RECEIVER_ALLOWED, 0x5678),
    ] {
        assert_eq!(
            fixture.drain(client),
            literal_xkb_new_keyboard_notify(
                sequence,
                yserver_protocol::x11::ClientByteOrder::LittleEndian,
                XKB_MAJOR_OPCODE,
                8,
                1,
            )
        );
    }
    assert_no_writer_failures(&fixture);

    let mut receive_clients = vec![
        NOTIFY_SENDER,
        NOTIFY_RECEIVER_DENIED,
        NOTIFY_RECEIVER_ALLOWED,
        NOTIFY_SENDER,
        NOTIFY_RECEIVER_DENIED,
        NOTIFY_RECEIVER_ALLOWED,
    ];
    receive_clients.extend(kbd_receive_order);
    receive_clients.extend([
        NOTIFY_SENDER,
        NOTIFY_RECEIVER_DENIED,
        NOTIFY_RECEIVER_ALLOWED,
    ]);
    let mut receive_requests = vec![request_opcodes(18, 0); 6];
    receive_requests.extend([request_opcodes(XKB_MAJOR_OPCODE, 23); 3]);
    receive_requests.extend([request_opcodes(XKB_MAJOR_OPCODE, 8); 3]);
    assert_receive_calls_with_requests(&calls, &receive_clients, &receive_requests);
}

// Mutations: use Device(1) for K4 instead of notify.device_id, or return only
// allowed recipients from send_xkb_map_notify; both violate this helper contract.
#[test]
fn receive_xkb_map_payload_device_subject() {
    let mut fixture = NotifyFixture::new();
    fixture.backend.xkb_info_result = Some((XKB_MAJOR_OPCODE, XKB_EVENT_BASE, 0));
    for client in [NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED] {
        crate::core_loop::xkb_select::xkb_select_events(
            &mut fixture.state,
            client.0,
            0x0100,
            0x0002,
        );
    }
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
    let calls = fixture.install_policy(|_, check| {
        if check.hook == ExtensionHook::Receive
            && check.client == NOTIFY_RECEIVER_DENIED
            && check.receive == Some(EventSubject::Device(3))
            && check.events == Some(vec![XKB_EVENT_DESCRIPTION])
        {
            Decision::Deny(DENY)
        } else {
            Decision::Allow
        }
    });
    let notify = yserver_protocol::x11::XkbMapNotify {
        device_id: 3,
        changed: 0x0002,
        min_keycode: 8,
        max_keycode: 255,
        first_key_sym: 8,
        n_key_syms: 1,
        ..Default::default()
    };
    let recipients = crate::core_loop::xkb_layout::send_xkb_map_notify(
        &mut fixture.state,
        XKB_EVENT_BASE,
        notify,
    );
    assert_eq!(recipients.len(), 2);
    assert!(recipients.contains(&NOTIFY_RECEIVER_DENIED));
    assert!(recipients.contains(&NOTIFY_RECEIVER_ALLOWED));
    assert!(fixture.drain(NOTIFY_RECEIVER_DENIED).is_empty());
    assert_eq!(
        fixture.drain(NOTIFY_RECEIVER_ALLOWED),
        literal_xkb_map_notify(
            0x5678,
            yserver_protocol::x11::ClientByteOrder::LittleEndian,
            3,
            0x0002,
            0,
            8,
            1,
            0,
            0
        )
    );
    assert_xkb_receive_log(
        &calls.lock().unwrap(),
        &[NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED],
        EventSubject::Device(3),
        None,
    );
    assert_no_writer_failures(&fixture);
}
