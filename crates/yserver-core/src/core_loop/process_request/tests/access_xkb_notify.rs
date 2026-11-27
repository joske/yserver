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
use yserver_protocol::x11::{ClientByteOrder, ClientId, RequestHeader, SequenceNumber};

const XKB_EVENT_BASE: u8 = 85;
const XKB_MAJOR_OPCODE: u8 = 136;
const XKB_NEW_KEYBOARD_NOTIFY_TYPE: u8 = 0;
const XKB_MAP_NOTIFY_TYPE: u8 = 1;
const XKB_STATE_NOTIFY_TYPE: u8 = 2;
const XKB_CONTROLS_NOTIFY_TYPE: u8 = 3;
const XKB_INDICATOR_STATE_NOTIFY_TYPE: u8 = 4;
const XKB_INDICATOR_MAP_NOTIFY_TYPE: u8 = 5;
const XKB_NAMES_NOTIFY_TYPE: u8 = 6;
const XKB_COMPAT_MAP_NOTIFY_TYPE: u8 = 7;
const XKB_EXTENSION_DEVICE_NOTIFY_TYPE: u8 = 11;
const STATE_DETAILS: u16 = 0x3fff;
const REPEAT_KEYS_MASK: u32 = 1;
const PER_KEY_REPEAT_MASK: u32 = 1 << 30;
const REPEAT_KEYCODE: u8 = 200;
const REQUEST_SEQUENCE: u16 = 0x2a6b;
const NON_XKB_CLIENT: ClientId = ClientId(4);
const KBD_BY_NAME_SEQUENCE: u16 = 0x3b7c;
const SET_MAP_SEQUENCE: u16 = 0x4c8d;
const SET_COMPAT_SEQUENCE: u16 = 0x5d9e;
const SET_NAMES_SEQUENCE: u16 = 0x6eaf;
const SET_INDICATOR_MAP_SEQUENCE: u16 = 0x7fb0;
const XKB_SET_COMPAT_MAP: u8 = 11;
const XKB_SET_INDICATOR_MAP: u8 = 14;
const XKB_SET_NAMES: u8 = 18;
const XKB_SET_COMPAT_BODY: [u8; 12] = [0x00, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
const XKB_SET_INDICATOR_MAP_BODY: [u8; 8] = [0x00, 0x01, 0, 0, 0, 0, 0, 0];
const XKB_SET_NAMES_BODY: [u8; 24] = [
    0x00, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

const DENY: DenyError = DenyError {
    code: yserver_protocol::x11::error::BAD_ACCESS,
    value: 0,
};

fn xkb_event_description_for_test(xkb_type: u8) -> EventDescription {
    EventDescription::extension(XKB_EVENT_BASE, XKB_MAJOR_OPCODE, u16::from(xkb_type))
}

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

fn range_change_set_map_body() -> [u8; 32] {
    let mut body = [0; 32];
    body[..2].copy_from_slice(&0x0100u16.to_le_bytes()); // XkbUseCoreKbd
    body[6] = 9; // minKeyCode differs from the current range
    body[7] = u8::MAX;
    body
}

fn keysym_set_map_body() -> [u8; 44] {
    let mut body = [0; 44];
    body[..2].copy_from_slice(&0x0100u16.to_le_bytes()); // XkbUseCoreKbd
    body[2..4].copy_from_slice(&0x0002u16.to_le_bytes()); // XkbKeySymsMask
    body[6] = 8; // current minKeyCode
    body[7] = u8::MAX;
    body[10] = 8; // firstKeySym
    body[11] = 1; // nKeySyms
    body[12..14].copy_from_slice(&1u16.to_le_bytes()); // totalSyms
    body[36] = 1; // one group
    body[37] = 1; // width
    body[38..40].copy_from_slice(&1u16.to_le_bytes()); // one symbol
    body[40..44].copy_from_slice(&0x0000_0041u32.to_le_bytes()); // XK_A
    body
}

fn drive_xkb_set_map(fixture: &mut NotifyFixture, sequence: u16, body: &[u8]) -> RequestOutcome {
    xkb_request(fixture, NOTIFY_SENDER, sequence, 9, body)
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

fn assert_no_writer_failures(fixture: &NotifyFixture) {
    assert!(
        crate::core_loop::client_io::failed_writers(&fixture.state.clients).is_empty(),
        "notification fanout has no failed writers"
    );
}

fn receive_log(calls: &Arc<Mutex<Vec<RecordedExtensionCheck>>>) -> Vec<RecordedExtensionCheck> {
    calls.lock().expect("policy call log").clone()
}

fn assert_receive_calls_with_requests(
    calls: &[RecordedExtensionCheck],
    clients: &[ClientId],
    requests: &[Option<RequestOpcodes>],
    subject: EventSubject,
    xkb_types: &[u8],
) {
    let receives: Vec<_> = calls
        .iter()
        .filter(|call| call.hook == ExtensionHook::Receive)
        .collect();
    assert_eq!(receives.len(), clients.len(), "whole-log Receive count");
    assert_eq!(requests.len(), clients.len(), "expected request contexts");
    assert_eq!(xkb_types.len(), clients.len(), "expected per-event xkbType");
    assert!(
        calls.iter().all(|call| call.hook != ExtensionHook::Send),
        "server-generated notifications run Receive without Send"
    );
    for (((call, client), request), xkb_type) in
        receives.iter().zip(clients).zip(requests).zip(xkb_types)
    {
        assert_eq!(call.client, *client);
        assert_eq!(call.receive, Some(subject));
        // Mutation: make xkb_event_description ignore xkbType and return 0;
        // every nonzero per-event expectation here must fail.
        assert_eq!(
            call.events,
            Some(vec![xkb_event_description_for_test(*xkb_type)])
        );
        assert_eq!(call.request, *request);
    }
}

const XKB_INDICATOR_STATE_SELECT: u16 = 1 << 4;
const XKB_INDICATOR_MAP_SELECT: u16 = 1 << 5;
const XKB_NAMES_SELECT: u16 = 1 << 6;
const XKB_COMPAT_MAP_SELECT: u16 = 1 << 7;
const XKB_EXTENSION_DEVICE_SELECT: u16 = 1 << 11;
const XKB_INDICATOR_EVENTS_SELECT: u16 = XKB_INDICATOR_STATE_SELECT
    | XKB_INDICATOR_MAP_SELECT
    | XKB_NAMES_SELECT
    | XKB_COMPAT_MAP_SELECT
    | XKB_EXTENSION_DEVICE_SELECT;
const XKB_INDICATOR_FOLLOWUPS_SELECT: u16 =
    (1 << 2) | XKB_INDICATOR_STATE_SELECT | XKB_EXTENSION_DEVICE_SELECT;

fn indicator_fixture(selections: &[(ClientId, u16)]) -> NotifyFixture {
    let mut fixture = NotifyFixture::new();
    fixture.backend.xkb_info_result = Some((XKB_MAJOR_OPCODE, XKB_EVENT_BASE, 0));
    for client in [
        NOTIFY_SENDER,
        NOTIFY_RECEIVER_DENIED,
        NOTIFY_RECEIVER_ALLOWED,
    ] {
        initialize_xkb_client(&mut fixture, client, 0, 0);
        let selected = selections
            .iter()
            .find_map(|(selected, which)| (*selected == client).then_some(*which))
            .unwrap_or(0);
        if selected != 0 {
            let mut body = Vec::with_capacity(12);
            body.extend_from_slice(&0x0100u16.to_le_bytes()); // XkbUseCoreKbd
            body.extend_from_slice(&selected.to_le_bytes()); // affectWhich
            body.extend_from_slice(&0u16.to_le_bytes()); // clear
            body.extend_from_slice(&selected.to_le_bytes()); // selectAll
            body.extend_from_slice(&0u16.to_le_bytes()); // affectMap
            body.extend_from_slice(&0u16.to_le_bytes()); // map
            assert!(matches!(
                xkb_request(&mut fixture, client, 0x2020 + client.0 as u16, 1, &body),
                RequestOutcome::Handled
            ));
            assert!(fixture.drain(client).is_empty(), "XkbSelectEvents is void");
        }
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

fn compat_map_notify() -> yserver_protocol::x11::XkbCompatMapNotify {
    yserver_protocol::x11::XkbCompatMapNotify {
        device_id: 1,
        changed_groups: 5,
        first_si: 0x0102,
        n_si: 0x0304,
        n_total_si: 0x0506,
    }
}

fn names_notify() -> yserver_protocol::x11::XkbNamesNotify {
    yserver_protocol::x11::XkbNamesNotify {
        device_id: 1,
        changed: 0x0123,
        first_type: 0x12,
        n_types: 0x03,
        first_level_name: 0x04,
        n_level_names: 0x05,
        n_radio_groups: 0x06,
        n_aliases: 0x07,
        changed_group_names: 0x08,
        changed_virtual_mods: 0x0910,
        first_key: 0x0b,
        n_keys: 0x0c,
        changed_indicators: 0x0d0e_0f10,
    }
}

fn indicator_maps_change() -> crate::backend::XkbIndicatorMapsChange {
    crate::backend::XkbIndicatorMapsChange {
        maps_changed: 0x0000_0002,
        state_changed: 0x0000_0004,
        state: 0x0000_0020,
        leds_defined: 0x0000_0023,
    }
}

fn indicator_names_change() -> XkbSetEvent {
    XkbSetEvent::IndicatorNames {
        leds_defined: 0x0000_0030,
        state: 0x0000_0020,
    }
}

const REQUEST_XKB_MODS: (u8, u8, u8, u8) = (0x40, 0x40, 0, 0);
const REQUEST_XKB_INDICATORS: u32 = 0x20;

fn seed_request_xkb_snapshots(fixture: &mut NotifyFixture) {
    fixture.backend.xkb_mods = REQUEST_XKB_MODS;
    fixture.backend.xkb_indicators_result = (REQUEST_XKB_INDICATORS, 0x3f);
    fixture.state.xkb_announced = (0, (1, 2, 3, 4));
    fixture.state.xkb_announced_indicators = 0x08;
}

fn assert_request_xkb_snapshots(fixture: &NotifyFixture) {
    assert_eq!(fixture.backend.xkb_mods, REQUEST_XKB_MODS);
    assert_eq!(
        fixture.backend.xkb_indicators_result,
        (REQUEST_XKB_INDICATORS, 0x3f)
    );
    assert_eq!(fixture.state.xkb_announced, (0, REQUEST_XKB_MODS));
    assert_eq!(
        fixture.state.xkb_announced_indicators,
        REQUEST_XKB_INDICATORS
    );
}

fn drive_scripted_xkb_set(
    fixture: &mut NotifyFixture,
    sequence: u16,
    minor: u8,
    body: &[u8],
    events: Vec<XkbSetEvent>,
) {
    script_xkb_set_outcome(
        fixture,
        XkbSetOutcome {
            error: None,
            events,
        },
    );
    assert!(matches!(
        xkb_request(fixture, NOTIFY_SENDER, sequence, minor, body),
        RequestOutcome::Handled
    ));
    assert!(fixture.drain(NOTIFY_SENDER).is_empty(), "XkbSet* is void");
    assert_request_xkb_snapshots(fixture);
    assert_no_writer_failures(fixture);
}

fn assert_xkb_notification_effects_match(denied: &NotifyFixture, allowed: &NotifyFixture) {
    assert_eq!(denied.backend.xkb_mods, allowed.backend.xkb_mods);
    assert_eq!(denied.state.xkb_announced, allowed.state.xkb_announced);
    assert_eq!(
        denied.state.xkb_announced_indicators,
        allowed.state.xkb_announced_indicators
    );
    assert_eq!(
        denied.state.keyboard_control.auto_repeats,
        allowed.state.keyboard_control.auto_repeats
    );
}

fn literal_xkb_indicator_notify(
    sequence: u16,
    kind: yserver_protocol::x11::XkbIndicatorNotifyKind,
    state: u32,
    changed: u32,
) -> Vec<u8> {
    let mut event = vec![
        XKB_EVENT_BASE,
        match kind {
            yserver_protocol::x11::XkbIndicatorNotifyKind::State => 4,
            yserver_protocol::x11::XkbIndicatorNotifyKind::Map => 5,
        },
    ];
    append_event_u16(
        &mut event,
        yserver_protocol::x11::ClientByteOrder::LittleEndian,
        sequence,
    );
    event.extend_from_slice(&[0; 4]); // timestamp
    event.extend_from_slice(&[1, 0, 0, 0]); // device and pad
    append_event_u32(
        &mut event,
        yserver_protocol::x11::ClientByteOrder::LittleEndian,
        state,
    );
    append_event_u32(
        &mut event,
        yserver_protocol::x11::ClientByteOrder::LittleEndian,
        changed,
    );
    finish_literal_event(event)
}

fn literal_xkb_compat_map_notify(
    sequence: u16,
    notify: yserver_protocol::x11::XkbCompatMapNotify,
) -> Vec<u8> {
    let mut event = vec![XKB_EVENT_BASE, 7]; // XkbCompatMapNotify
    append_event_u16(
        &mut event,
        yserver_protocol::x11::ClientByteOrder::LittleEndian,
        sequence,
    );
    event.extend_from_slice(&[0; 4]); // timestamp
    event.extend_from_slice(&[notify.device_id, notify.changed_groups]);
    for value in [notify.first_si, notify.n_si, notify.n_total_si] {
        append_event_u16(
            &mut event,
            yserver_protocol::x11::ClientByteOrder::LittleEndian,
            value,
        );
    }
    finish_literal_event(event)
}

fn literal_xkb_names_notify(
    sequence: u16,
    notify: yserver_protocol::x11::XkbNamesNotify,
) -> Vec<u8> {
    let mut event = vec![XKB_EVENT_BASE, 6]; // XkbNamesNotify
    append_event_u16(
        &mut event,
        yserver_protocol::x11::ClientByteOrder::LittleEndian,
        sequence,
    );
    event.extend_from_slice(&[0; 4]); // timestamp
    event.extend_from_slice(&[notify.device_id, 0]); // device and pad
    append_event_u16(
        &mut event,
        yserver_protocol::x11::ClientByteOrder::LittleEndian,
        notify.changed,
    );
    event.extend_from_slice(&[
        notify.first_type,
        notify.n_types,
        notify.first_level_name,
        notify.n_level_names,
        0,
        notify.n_radio_groups,
        notify.n_aliases,
        notify.changed_group_names,
    ]);
    append_event_u16(
        &mut event,
        yserver_protocol::x11::ClientByteOrder::LittleEndian,
        notify.changed_virtual_mods,
    );
    event.extend_from_slice(&[notify.first_key, notify.n_keys]);
    append_event_u32(
        &mut event,
        yserver_protocol::x11::ClientByteOrder::LittleEndian,
        notify.changed_indicators,
    );
    finish_literal_event(event)
}

fn literal_xkb_extension_device_notify(
    sequence: u16,
    notify: yserver_protocol::x11::XkbExtensionDeviceNotify,
) -> Vec<u8> {
    let mut event = vec![XKB_EVENT_BASE, 11]; // XkbExtensionDeviceNotify
    append_event_u16(
        &mut event,
        yserver_protocol::x11::ClientByteOrder::LittleEndian,
        sequence,
    );
    event.extend_from_slice(&[0; 4]); // timestamp
    event.extend_from_slice(&[notify.device_id, 0]); // device and pad
    for value in [notify.reason, notify.led_class, notify.led_id] {
        append_event_u16(
            &mut event,
            yserver_protocol::x11::ClientByteOrder::LittleEndian,
            value,
        );
    }
    for value in [notify.leds_defined, notify.led_state] {
        append_event_u32(
            &mut event,
            yserver_protocol::x11::ClientByteOrder::LittleEndian,
            value,
        );
    }
    event.extend_from_slice(&[notify.first_btn, notify.n_btns]);
    for value in [notify.supported, notify.unsupported] {
        append_event_u16(
            &mut event,
            yserver_protocol::x11::ClientByteOrder::LittleEndian,
            value,
        );
    }
    finish_literal_event(event)
}

fn maps_extension_device_notify() -> yserver_protocol::x11::XkbExtensionDeviceNotify {
    yserver_protocol::x11::XkbExtensionDeviceNotify {
        device_id: 1,
        reason: 0x18,
        leds_defined: 0x23,
        led_state: 0x20,
        supported: 0x001f,
        ..Default::default()
    }
}

fn names_extension_device_notify() -> yserver_protocol::x11::XkbExtensionDeviceNotify {
    yserver_protocol::x11::XkbExtensionDeviceNotify {
        device_id: 1,
        reason: 0x04,
        leds_defined: 0x30,
        led_state: 0x20,
        supported: 0x001f,
        ..Default::default()
    }
}

fn literal_xkb_names_set_stream(sequence: u16) -> Vec<u8> {
    let mut stream = literal_xkb_names_notify(sequence, names_notify());
    append_frame(
        &mut stream,
        literal_xkb_extension_device_notify(sequence, names_extension_device_notify()),
    );
    stream
}

fn literal_xkb_indicator_maps_stream(sequence: u16) -> Vec<u8> {
    let mut stream = literal_xkb_indicator_notify(
        sequence,
        yserver_protocol::x11::XkbIndicatorNotifyKind::Map,
        0x20,
        0x02,
    );
    append_frame(
        &mut stream,
        literal_xkb_indicator_notify(
            sequence,
            yserver_protocol::x11::XkbIndicatorNotifyKind::State,
            0x20,
            0x04,
        ),
    );
    append_frame(
        &mut stream,
        literal_xkb_extension_device_notify(sequence, maps_extension_device_notify()),
    );
    stream
}

fn literal_xkb_indicator_set_stream(sequence: u16) -> Vec<u8> {
    let mut stream = literal_xkb_compat_map_notify(sequence, compat_map_notify());
    append_frame(&mut stream, literal_xkb_names_set_stream(sequence));
    append_frame(&mut stream, literal_xkb_indicator_maps_stream(sequence));
    stream
}

fn literal_xkb_key_indicator_stream(sequence: u16, include_state: bool) -> Vec<u8> {
    let mut stream = Vec::new();
    if include_state {
        append_frame(
            &mut stream,
            literal_state_notify(sequence, 0x40, 0x40, 0, 0x1f03, 38, 2, 0, 0),
        );
    }
    append_frame(
        &mut stream,
        literal_xkb_indicator_notify(
            sequence,
            yserver_protocol::x11::XkbIndicatorNotifyKind::State,
            0x20,
            0x20,
        ),
    );
    append_frame(
        &mut stream,
        literal_xkb_extension_device_notify(
            sequence,
            yserver_protocol::x11::XkbExtensionDeviceNotify {
                device_id: 1,
                reason: 0x10,
                leds_defined: 0x3f,
                led_state: 0x20,
                supported: 0x001f,
                ..Default::default()
            },
        ),
    );
    stream
}

#[test]
// Focused because the original no-policy pipeline compares each request checkpoint.
// Mutation B1: change the literal StateNotify requestMinor from 5.
fn receive_xkb_state_no_policy_checkpoints() {
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
    assert_no_writer_failures(&fixture);

    drive_set_controls(&mut fixture);
    append_frame(&mut denied_stream, fixture.drain(NOTIFY_RECEIVER_DENIED));
    append_frame(&mut allowed_stream, fixture.drain(NOTIFY_RECEIVER_ALLOWED));
    assert_eq!(
        denied_stream,
        [
            literal_state_notify(0x1234, 0x10, 0, 0x10, 0x1f09, 0, 0, 136, 5),
            literal_controls_notify(0x1234, 1, REPEAT_KEYS_MASK, 1, 7),
        ]
        .concat()
    );
    assert_eq!(
        allowed_stream,
        [
            literal_state_notify(0x5678, 0x10, 0, 0x10, 0x1f09, 0, 0, 136, 5),
            literal_controls_notify(0x5678, 1, REPEAT_KEYS_MASK, 1, 7),
        ]
        .concat()
    );
    assert_no_writer_failures(&fixture);

    drive_set_per_key_repeat(&mut fixture);
    append_frame(&mut denied_stream, fixture.drain(NOTIFY_RECEIVER_DENIED));
    append_frame(&mut allowed_stream, fixture.drain(NOTIFY_RECEIVER_ALLOWED));
    assert_eq!(denied_stream, literal_request_stream(0x1234));
    assert_eq!(allowed_stream, literal_request_stream(0x5678));
    assert_eq!(fixture.backend.xkb_mods, (0x10, 0, 0, 0x10));
    assert_eq!(fixture.state.keyboard_control.repeat_delay, 200);
    assert_eq!(fixture.state.keyboard_control.repeat_interval, 20);
    assert_ne!(
        fixture.state.keyboard_control.auto_repeats[usize::from(REPEAT_KEYCODE >> 3)]
            & (1 << (REPEAT_KEYCODE & 7)),
        0
    );
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

#[test]
// Mutation B2: skip K11 Receive; the targeted deny must drop this copy.
// Mutation B3: use a Global subject for K11; the full Receive trace must fail.
// Mutation B4: force native XKB descriptions to xkbType 0; type 2 must fail.
fn receive_xkb_state_pipeline_denies_latch_copy() {
    use super::access_notify_support::{
        CallOrder, CaseExpect, ExpectedReceive, ReceiveCase, Stream, check_receive_case,
    };

    let build = || (notify_fixture(STATE_DETAILS, 0), ());
    let act = |fixture: &mut NotifyFixture, _: &()| {
        drive_latch_lock(fixture);
    };
    let expect = |_: &()| {
        let receive = |client, request, xkb_type| ExpectedReceive {
            client,
            request,
            subject: EventSubject::Device(1),
            events: vec![xkb_event_description_for_test(xkb_type)],
        };
        let state_denied = receive(NOTIFY_RECEIVER_DENIED, request_opcodes(136, 5), 2);
        let receives = vec![
            state_denied.clone(),
            receive(NOTIFY_RECEIVER_ALLOWED, request_opcodes(136, 5), 2),
        ];
        let denied_stream = vec![
            Stream::unchanged(NOTIFY_SENDER, Vec::new()),
            Stream {
                client: NOTIFY_RECEIVER_DENIED,
                bytes: literal_state_notify(0x1234, 0x10, 0, 0x10, 0x1f09, 0, 0, 136, 5),
                denied: Some(Vec::new()),
            },
            Stream::unchanged(
                NOTIFY_RECEIVER_ALLOWED,
                literal_state_notify(0x5678, 0x10, 0, 0x10, 0x1f09, 0, 0, 136, 5),
            ),
        ];
        let mut expectations = CaseExpect::new(
            denied_stream,
            receives,
            CallOrder::Total,
            vec![state_denied],
        );
        expectations.deny_before_allow = true;
        expectations.after_deny = Some(Box::new(|allow, denied| {
            assert_eq!(denied.backend.xkb_mods, allow.backend.xkb_mods);
            assert_eq!(denied.state.xkb_announced, allow.state.xkb_announced);
            assert_eq!(denied.state.xkb_announced, (0, (0x10, 0, 0, 0x10)));
        }));
        expectations
    };
    check_receive_case(&ReceiveCase {
        name: "XKB state pipeline denies one latch copy",
        build: &build,
        act: &act,
        expect: &expect,
    });
}

#[test]
// Mutation B5: advance xkb_announced only after an all-recipient denial return.
fn receive_xkb_state_denied_snapshot_advances() {
    use super::access_notify_support::{
        CallOrder, CaseExpect, ExpectedReceive, ReceiveCase, Stream, check_receive_case,
    };

    let build = || (notify_fixture(STATE_DETAILS, 0), ());
    let act = |fixture: &mut NotifyFixture, _: &()| {
        drive_latch_lock(fixture);
    };
    let expect = |_: &()| {
        let receive = |client| ExpectedReceive {
            client,
            request: request_opcodes(136, 5),
            subject: EventSubject::Device(1),
            events: vec![xkb_event_description_for_test(XKB_STATE_NOTIFY_TYPE)],
        };
        let denied = receive(NOTIFY_RECEIVER_DENIED);
        let allowed = receive(NOTIFY_RECEIVER_ALLOWED);
        let mut expectations = CaseExpect::new(
            vec![
                Stream::unchanged(NOTIFY_SENDER, Vec::new()),
                Stream {
                    client: NOTIFY_RECEIVER_DENIED,
                    bytes: literal_state_notify(0x1234, 0x10, 0, 0x10, 0x1f09, 0, 0, 136, 5),
                    denied: Some(Vec::new()),
                },
                Stream {
                    client: NOTIFY_RECEIVER_ALLOWED,
                    bytes: literal_state_notify(0x5678, 0x10, 0, 0x10, 0x1f09, 0, 0, 136, 5),
                    denied: Some(Vec::new()),
                },
            ],
            vec![denied.clone(), allowed.clone()],
            CallOrder::Total,
            vec![denied, allowed],
        );
        expectations.deny_before_allow = true;
        expectations.after_deny = Some(Box::new(|allow, denied| {
            assert_eq!(denied.backend.xkb_mods, allow.backend.xkb_mods);
            assert_eq!(denied.state.xkb_announced, allow.state.xkb_announced);
            assert_eq!(denied.state.xkb_announced, (0, (0x10, 0, 0, 0x10)));
        }));
        expectations
    };
    check_receive_case(&ReceiveCase {
        name: "XKB state snapshot advances when both copies are denied",
        build: &build,
        act: &act,
        expect: &expect,
    });
}

#[test]
// Mutation B7: skip K12 Receive; the targeted deny must drop ControlsNotify.
fn receive_xkb_controls_denied_one_recipient() {
    use super::access_notify_support::{
        CallOrder, CaseExpect, ExpectedReceive, ReceiveCase, Stream, check_receive_case,
    };

    let build = || (notify_fixture(0, REPEAT_KEYS_MASK), ());
    let act = |fixture: &mut NotifyFixture, _: &()| {
        drive_set_controls(fixture);
        assert_eq!(
            (
                fixture.state.keyboard_control.repeat_delay,
                fixture.state.keyboard_control.repeat_interval
            ),
            (200, 20)
        );
    };
    let expect = |_: &()| {
        let receive = |client| ExpectedReceive {
            client,
            request: request_opcodes(136, 7),
            subject: EventSubject::Device(1),
            events: vec![xkb_event_description_for_test(XKB_CONTROLS_NOTIFY_TYPE)],
        };
        let denied = receive(NOTIFY_RECEIVER_DENIED);
        let allowed = receive(NOTIFY_RECEIVER_ALLOWED);
        let mut expectations = CaseExpect::new(
            vec![
                Stream::unchanged(NOTIFY_SENDER, Vec::new()),
                Stream {
                    client: NOTIFY_RECEIVER_DENIED,
                    bytes: literal_controls_notify(0x1234, 1, REPEAT_KEYS_MASK, 1, 7),
                    denied: Some(Vec::new()),
                },
                Stream::unchanged(
                    NOTIFY_RECEIVER_ALLOWED,
                    literal_controls_notify(0x5678, 1, REPEAT_KEYS_MASK, 1, 7),
                ),
            ],
            vec![denied.clone(), allowed],
            CallOrder::Total,
            vec![denied],
        );
        expectations.deny_before_allow = true;
        expectations.after_deny = Some(Box::new(|allow, denied| {
            assert_eq!(
                (
                    denied.state.keyboard_control.global_auto_repeat,
                    denied.state.keyboard_control.repeat_delay,
                    denied.state.keyboard_control.repeat_interval,
                    denied.state.keyboard_control.auto_repeats
                ),
                (
                    allow.state.keyboard_control.global_auto_repeat,
                    allow.state.keyboard_control.repeat_delay,
                    allow.state.keyboard_control.repeat_interval,
                    allow.state.keyboard_control.auto_repeats
                ),
            );
        }));
        expectations
    };
    check_receive_case(&ReceiveCase {
        name: "XKB controls denial keeps the other recipient",
        build: &build,
        act: &act,
        expect: &expect,
    });
}

#[test]
// Mutation B8: skip K9 Receive; the targeted deny must drop the per-key copy.
// Mutation B9: alter another repeat bit only in the denied run; the full bitmap comparison must fail.
fn receive_xkb_per_key_repeat_denied() {
    use super::access_notify_support::{
        CallOrder, CaseExpect, ExpectedReceive, ReceiveCase, Stream, check_receive_case,
    };

    let build = || (notify_fixture(0, PER_KEY_REPEAT_MASK), ());
    let act = |fixture: &mut NotifyFixture, _: &()| {
        drive_set_per_key_repeat(fixture);
        let repeat_bit = 1u8 << (REPEAT_KEYCODE & 7);
        let repeat_byte = usize::from(REPEAT_KEYCODE >> 3);
        assert_ne!(
            fixture.state.keyboard_control.auto_repeats[repeat_byte] & repeat_bit,
            0
        );
    };
    let expect = |_: &()| {
        let receive = |client| ExpectedReceive {
            client,
            request: request_opcodes(136, 9),
            subject: EventSubject::Device(1),
            events: vec![xkb_event_description_for_test(XKB_CONTROLS_NOTIFY_TYPE)],
        };
        let denied = receive(NOTIFY_RECEIVER_DENIED);
        let allowed = receive(NOTIFY_RECEIVER_ALLOWED);
        let mut expectations = CaseExpect::new(
            vec![
                Stream::unchanged(NOTIFY_SENDER, Vec::new()),
                Stream {
                    client: NOTIFY_RECEIVER_DENIED,
                    bytes: literal_controls_notify(0x1234, 2, PER_KEY_REPEAT_MASK, 1, 9),
                    denied: Some(Vec::new()),
                },
                Stream::unchanged(
                    NOTIFY_RECEIVER_ALLOWED,
                    literal_controls_notify(0x5678, 2, PER_KEY_REPEAT_MASK, 1, 9),
                ),
            ],
            vec![denied.clone(), allowed],
            CallOrder::Total,
            vec![denied],
        );
        expectations.deny_before_allow = true;
        expectations.after_deny = Some(Box::new(|allow, denied| {
            assert_eq!(
                (
                    denied.state.keyboard_control.auto_repeats,
                    denied.state.keyboard_control.global_auto_repeat
                ),
                (
                    allow.state.keyboard_control.auto_repeats,
                    allow.state.keyboard_control.global_auto_repeat
                ),
                "denial keeps the full repeat bitmap and enabled-controls value"
            );
        }));
        expectations
    };
    check_receive_case(&ReceiveCase {
        name: "XKB per-key repeat denial keeps the full bitmap",
        build: &build,
        act: &act,
        expect: &expect,
    });
}

#[test]
// Mutation B6: add request context to the physical-key K11 check; Allow must record None.
fn receive_xkb_physical_key_state_has_no_request() {
    use super::access_notify_support::{
        CallOrder, CaseExpect, ExpectedReceive, ReceiveCase, Stream, check_receive_case,
    };

    let build = || (notify_fixture(STATE_DETAILS, 0), ());
    let act = |fixture: &mut NotifyFixture, _: &()| {
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
        assert_eq!(fixture.state.xkb_announced, (0, (0x40, 0x40, 0, 0)));
    };
    let expect = |_: &()| {
        let receive = |client| ExpectedReceive {
            client,
            request: None,
            subject: EventSubject::Device(1),
            events: vec![xkb_event_description_for_test(XKB_STATE_NOTIFY_TYPE)],
        };
        let denied = receive(NOTIFY_RECEIVER_DENIED);
        let allowed = receive(NOTIFY_RECEIVER_ALLOWED);
        let mut expectations = CaseExpect::new(
            vec![
                Stream::unchanged(NOTIFY_SENDER, Vec::new()),
                Stream {
                    client: NOTIFY_RECEIVER_DENIED,
                    bytes: literal_state_notify(0x1234, 0x40, 0x40, 0, 0x1f03, 38, 2, 0, 0),
                    denied: Some(Vec::new()),
                },
                Stream::unchanged(
                    NOTIFY_RECEIVER_ALLOWED,
                    literal_state_notify(0x5678, 0x40, 0x40, 0, 0x1f03, 38, 2, 0, 0),
                ),
            ],
            vec![denied.clone(), allowed],
            CallOrder::Total,
            vec![denied],
        );
        expectations.after_deny = Some(Box::new(|allow, denied| {
            assert_eq!(denied.backend.xkb_mods, allow.backend.xkb_mods);
            assert_eq!(denied.state.xkb_announced, allow.state.xkb_announced);
        }));
        expectations
    };
    check_receive_case(&ReceiveCase {
        name: "XKB physical-key StateNotify has no request context",
        build: &build,
        act: &act,
        expect: &expect,
    });
}

#[test]
// Mutation B10: bypass K11 Receive under allow-all; the full ordered log must fail.
fn receive_xkb_state_allow_control() {
    use super::access_notify_support::{
        CallOrder, CaseExpect, ExpectedReceive, ReceiveCase, Stream, check_receive_case,
    };

    let build = || {
        (
            notify_fixture(STATE_DETAILS, REPEAT_KEYS_MASK | PER_KEY_REPEAT_MASK),
            (),
        )
    };
    let act = |fixture: &mut NotifyFixture, _: &()| {
        drive_latch_lock(fixture);
        drive_set_controls(fixture);
        drive_set_per_key_repeat(fixture);
        assert_eq!(fixture.state.keyboard_control.repeat_delay, 200);
        assert_eq!(fixture.state.keyboard_control.repeat_interval, 20);
        assert_ne!(fixture.state.keyboard_control.auto_repeats[25] & 1, 0);
    };
    let expect = |_: &()| {
        let receive = |client, minor, xkb_type| ExpectedReceive {
            client,
            request: request_opcodes(XKB_MAJOR_OPCODE, minor),
            subject: EventSubject::Device(1),
            events: vec![xkb_event_description_for_test(xkb_type)],
        };
        let state_denied = receive(NOTIFY_RECEIVER_DENIED, 5, XKB_STATE_NOTIFY_TYPE);
        let mut receives = Vec::new();
        for (minor, xkb_type) in [
            (5, XKB_STATE_NOTIFY_TYPE),
            (7, XKB_CONTROLS_NOTIFY_TYPE),
            (9, XKB_CONTROLS_NOTIFY_TYPE),
        ] {
            receives.push(receive(NOTIFY_RECEIVER_DENIED, minor, xkb_type));
            receives.push(receive(NOTIFY_RECEIVER_ALLOWED, minor, xkb_type));
        }
        let denied_tail = [
            literal_controls_notify(0x1234, 1, REPEAT_KEYS_MASK, 1, 7),
            literal_controls_notify(0x1234, 2, PER_KEY_REPEAT_MASK, 1, 9),
        ]
        .concat();
        let mut expectations = CaseExpect::new(
            vec![
                Stream::unchanged(NOTIFY_SENDER, Vec::new()),
                Stream {
                    client: NOTIFY_RECEIVER_DENIED,
                    bytes: literal_request_stream(0x1234),
                    denied: Some(denied_tail),
                },
                Stream::unchanged(NOTIFY_RECEIVER_ALLOWED, literal_request_stream(0x5678)),
            ],
            receives,
            CallOrder::Total,
            vec![state_denied],
        );
        expectations.after_deny = Some(Box::new(|allow, denied| {
            assert_eq!(denied.backend.xkb_mods, allow.backend.xkb_mods);
            assert_eq!(denied.state.xkb_announced, allow.state.xkb_announced);
            assert_eq!(
                denied.state.keyboard_control.auto_repeats,
                allow.state.keyboard_control.auto_repeats
            );
            assert_eq!(
                (
                    denied.state.keyboard_control.global_auto_repeat,
                    denied.state.keyboard_control.repeat_delay,
                    denied.state.keyboard_control.repeat_interval
                ),
                (
                    allow.state.keyboard_control.global_auto_repeat,
                    allow.state.keyboard_control.repeat_delay,
                    allow.state.keyboard_control.repeat_interval
                ),
            );
        }));
        expectations
    };
    check_receive_case(&ReceiveCase {
        name: "XKB state allow control preserves the full trace and state",
        build: &build,
        act: &act,
        expect: &expect,
    });
}

#[test]
// Mutation B15: pass no request context to K1.
// Mutation B16: use Global instead of Device(1) for K1.
fn receive_xkb_rules_change_denies_new_keyboard_and_map() {
    use super::access_notify_support::{
        CallOrder, CaseExpect, ExpectedReceive, ReceiveCase, Stream, check_receive_case,
    };

    let selected = [
        NOTIFY_SENDER,
        NOTIFY_RECEIVER_DENIED,
        NOTIFY_RECEIVER_ALLOWED,
    ];
    let map_selected = selected.map(|client| (client, 0x0007));
    let build = || {
        let mut fixture = xkb_event_fixture(&selected, &map_selected, false);
        let backend = std::mem::take(&mut fixture.backend);
        fixture.backend = backend.with_keymap_rmlvo_result((8, 255));
        fixture.state.keymap_overrides.insert(38, vec![0x0071]);
        (fixture, ())
    };
    let act = |fixture: &mut NotifyFixture, _: &()| {
        assert!(matches!(
            drive_rules_names_change(fixture, REQUEST_SEQUENCE, b"evdev\0pc105\0de\0\0\0"),
            RequestOutcome::Handled
        ));
        assert!(fixture.state.keymap_overrides.is_empty());
    };
    let expect = |_: &()| {
        let receive = |client, xkb_type| ExpectedReceive {
            client,
            request: request_opcodes(18, 0),
            subject: EventSubject::Device(1),
            events: vec![xkb_event_description_for_test(xkb_type)],
        };
        let clients = [
            NOTIFY_SENDER,
            NOTIFY_RECEIVER_DENIED,
            NOTIFY_RECEIVER_ALLOWED,
        ];
        let new_keyboard = clients.map(|client| receive(client, XKB_NEW_KEYBOARD_NOTIFY_TYPE));
        let map = clients.map(|client| receive(client, XKB_MAP_NOTIFY_TYPE));
        let denied_nkn = receive(NOTIFY_RECEIVER_DENIED, XKB_NEW_KEYBOARD_NOTIFY_TYPE);
        let denied_map = receive(NOTIFY_RECEIVER_DENIED, XKB_MAP_NOTIFY_TYPE);
        let full = |sequence| literal_rules_names_stream(sequence, ClientByteOrder::LittleEndian);
        let mut expectations = CaseExpect::new(
            vec![
                Stream::unchanged(NOTIFY_SENDER, full(REQUEST_SEQUENCE)),
                Stream {
                    client: NOTIFY_RECEIVER_DENIED,
                    bytes: full(0x1234),
                    denied: Some(literal_mapping_pair(0x1234, ClientByteOrder::LittleEndian)),
                },
                Stream::unchanged(NOTIFY_RECEIVER_ALLOWED, full(0x5678)),
            ],
            new_keyboard.into_iter().chain(map).collect(),
            CallOrder::Total,
            vec![denied_nkn, denied_map],
        );
        expectations.deny_before_allow = true;
        // Preserve the original policy target while asserting request context in the trace.
        expectations.deny_match_ignores_request = true;
        expectations.after_deny = Some(Box::new(|allow, denied| {
            assert_eq!(denied.state.keymap_overrides, allow.state.keymap_overrides);
        }));
        expectations
    };
    check_receive_case(&ReceiveCase {
        name: "XKB rules change denies only the selected client's copies",
        build: &build,
        act: &act,
        expect: &expect,
    });
}

#[test]
// Mutation B13: skip K2 Receive for GetKbdByName.
// Mutation B14: encode NewKeyboardNotify in little-endian for the big-endian client;
// the original no-policy literal bytes must fail first.
fn receive_xkb_kbd_by_name_all_clients() {
    use super::access_notify_support::{
        CallOrder, CaseExpect, ExpectedReceive, ReceiveCase, Stream, check_receive_case,
    };

    let build = || {
        let mut fixture = NotifyFixture::new();
        fixture.backend.xkb_info_result = Some((XKB_MAJOR_OPCODE, XKB_EVENT_BASE, 0));
        initialize_xkb_notifications(&mut fixture, NOTIFY_SENDER, false, 0);
        fixture
            .state
            .clients
            .get_mut(&NOTIFY_RECEIVER_ALLOWED.0)
            .unwrap()
            .byte_order = ClientByteOrder::BigEndian;
        for (client, sequence) in [
            (NOTIFY_RECEIVER_DENIED, 0x1234),
            (NOTIFY_RECEIVER_ALLOWED, 0x5678),
        ] {
            fixture
                .state
                .clients
                .get(&client.0)
                .unwrap()
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
        let client_map_order = fixture
            .state
            .clients
            .keys()
            .copied()
            .map(ClientId)
            .collect::<Vec<_>>();
        (fixture, client_map_order)
    };
    let act = |fixture: &mut NotifyFixture, _: &Vec<ClientId>| {
        assert!(matches!(
            xkb_request(fixture, NOTIFY_SENDER, KBD_BY_NAME_SEQUENCE, 23, &[]),
            RequestOutcome::Handled
        ));
        assert!(fixture.state.keymap_overrides.is_empty());
    };
    let expect = |client_map_order: &Vec<ClientId>| {
        let receives = client_map_order
            .iter()
            .copied()
            .map(|client| ExpectedReceive {
                client,
                request: request_opcodes(XKB_MAJOR_OPCODE, 23),
                subject: EventSubject::Device(1),
                events: vec![xkb_event_description_for_test(XKB_NEW_KEYBOARD_NOTIFY_TYPE)],
            })
            .collect();
        let denied_receive = ExpectedReceive {
            client: NOTIFY_RECEIVER_DENIED,
            request: request_opcodes(XKB_MAJOR_OPCODE, 23),
            subject: EventSubject::Device(1),
            events: vec![xkb_event_description_for_test(XKB_NEW_KEYBOARD_NOTIFY_TYPE)],
        };
        let mut expectations = CaseExpect::new(
            vec![
                Stream::unchanged(
                    NOTIFY_SENDER,
                    literal_kbd_by_name_stream(
                        KBD_BY_NAME_SEQUENCE,
                        ClientByteOrder::LittleEndian,
                        true,
                    ),
                ),
                Stream {
                    client: NOTIFY_RECEIVER_DENIED,
                    bytes: literal_kbd_by_name_stream(0x1234, ClientByteOrder::LittleEndian, false),
                    denied: Some(literal_mapping_pair(0x1234, ClientByteOrder::LittleEndian)),
                },
                Stream::unchanged(
                    NOTIFY_RECEIVER_ALLOWED,
                    literal_kbd_by_name_stream(0x5678, ClientByteOrder::BigEndian, false),
                ),
            ],
            receives,
            CallOrder::Total,
            vec![denied_receive],
        );
        expectations.deny_before_allow = true;
        expectations.after_deny = Some(Box::new(|allow, denied| {
            assert_eq!(denied.state.keymap_overrides, allow.state.keymap_overrides);
        }));
        expectations
    };
    check_receive_case(&ReceiveCase {
        name: "XKB GetKbdByName checks every client in client-map order",
        build: &build,
        act: &act,
        expect: &expect,
    });
}

#[test]
// Mutation B17: skip K3 Receive for SetMap's NewKeyboardNotify.
// Mutation B18: drive K3 with GetMap minor 8 instead of SetMap minor 9.
fn receive_xkb_set_new_keyboard_denied() {
    use super::access_notify_support::{
        CallOrder, CaseExpect, ExpectedReceive, ReceiveCase, Stream, check_receive_case,
    };

    let selected = [
        NOTIFY_SENDER,
        NOTIFY_RECEIVER_DENIED,
        NOTIFY_RECEIVER_ALLOWED,
    ];
    let build = || {
        let mut fixture = xkb_event_fixture(&selected, &[], true);
        let repeat_bit = 1 << (REPEAT_KEYCODE & 7);
        let repeat_byte = usize::from(REPEAT_KEYCODE >> 3);
        fixture.state.keyboard_control.auto_repeats[repeat_byte] &= !repeat_bit;
        (fixture, ())
    };
    let act = |fixture: &mut NotifyFixture, _: &()| {
        script_xkb_set_outcome(
            fixture,
            XkbSetOutcome {
                error: None,
                events: vec![
                    XkbSetEvent::NewKeyboard(xkb_new_keyboard_info(1)),
                    XkbSetEvent::Repeats(vec![(REPEAT_KEYCODE, true)]),
                ],
            },
        );
        assert!(matches!(
            drive_xkb_set_map(fixture, SET_MAP_SEQUENCE, &range_change_set_map_body()),
            RequestOutcome::Handled
        ));
        let repeat_bit = 1 << (REPEAT_KEYCODE & 7);
        let repeat_byte = usize::from(REPEAT_KEYCODE >> 3);
        assert_ne!(
            fixture.state.keyboard_control.auto_repeats[repeat_byte] & repeat_bit,
            0
        );
    };
    let expect = |_: &()| {
        let receive = |client| ExpectedReceive {
            client,
            request: request_opcodes(XKB_MAJOR_OPCODE, 9),
            subject: EventSubject::Device(1),
            events: vec![xkb_event_description_for_test(XKB_NEW_KEYBOARD_NOTIFY_TYPE)],
        };
        let denied_receive = receive(NOTIFY_RECEIVER_DENIED);
        let new_keyboard = |sequence| {
            literal_xkb_new_keyboard_notify(
                sequence,
                ClientByteOrder::LittleEndian,
                XKB_MAJOR_OPCODE,
                9,
                1,
            )
        };
        let legacy = literal_mapping_pair(0x6a2f, ClientByteOrder::LittleEndian);
        let mut expectations = CaseExpect::new(
            vec![
                Stream::unchanged(NOTIFY_SENDER, new_keyboard(SET_MAP_SEQUENCE)),
                Stream {
                    client: NOTIFY_RECEIVER_DENIED,
                    bytes: new_keyboard(0x1234),
                    denied: Some(Vec::new()),
                },
                Stream::unchanged(NOTIFY_RECEIVER_ALLOWED, new_keyboard(0x5678)),
                Stream::unchanged(NON_XKB_CLIENT, legacy),
            ],
            [
                NOTIFY_SENDER,
                NOTIFY_RECEIVER_DENIED,
                NOTIFY_RECEIVER_ALLOWED,
            ]
            .map(receive)
            .to_vec(),
            CallOrder::Total,
            vec![denied_receive],
        );
        expectations.deny_before_allow = true;
        expectations.after_deny = Some(Box::new(|allow, denied| {
            assert_eq!(
                denied.state.keyboard_control.auto_repeats,
                allow.state.keyboard_control.auto_repeats
            );
        }));
        expectations
    };
    check_receive_case(&ReceiveCase {
        name: "XKB SetMap NewKeyboardNotify denial keeps repeat state and legacy bytes",
        build: &build,
        act: &act,
        expect: &expect,
    });
}

#[test]
// Mutation B19: filter the legacy core MappingNotify by the XKB MapNotify deny.
// Mutation B20: drive the keysym-only case with GetMap minor 8.
fn receive_xkb_map_denied_keeps_legacy_filter() {
    use super::access_notify_support::{
        CallOrder, CaseExpect, ExpectedReceive, ReceiveCase, Stream, check_receive_case,
    };

    let map_clients = [(NOTIFY_RECEIVER_DENIED, 0x0002)];
    let build = || (xkb_event_fixture(&[], &map_clients, true), ());
    let act = |fixture: &mut NotifyFixture, _: &()| {
        script_xkb_set_outcome(
            fixture,
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
        assert!(matches!(
            drive_xkb_set_map(fixture, SET_MAP_SEQUENCE, &keysym_set_map_body()),
            RequestOutcome::Handled
        ));
    };
    let expect = |_: &()| {
        let receive = ExpectedReceive {
            client: NOTIFY_RECEIVER_DENIED,
            request: request_opcodes(XKB_MAJOR_OPCODE, 9),
            subject: EventSubject::Device(1),
            events: vec![xkb_event_description_for_test(XKB_MAP_NOTIFY_TYPE)],
        };
        let legacy =
            |sequence| literal_mapping_notify(sequence, ClientByteOrder::LittleEndian, 1, 8, 1);
        let mut expectations = CaseExpect::new(
            vec![
                Stream::unchanged(NOTIFY_SENDER, Vec::new()),
                Stream {
                    client: NOTIFY_RECEIVER_DENIED,
                    bytes: literal_map_change_stream(0x1234, ClientByteOrder::LittleEndian),
                    denied: Some(legacy(0x1234)),
                },
                Stream::unchanged(NOTIFY_RECEIVER_ALLOWED, Vec::new()),
                Stream::unchanged(NON_XKB_CLIENT, legacy(0x6a2f)),
            ],
            vec![receive.clone()],
            CallOrder::Total,
            vec![receive],
        );
        expectations.deny_before_allow = true;
        expectations.trace_before_deny_streams = true;
        expectations
    };
    check_receive_case(&ReceiveCase {
        name: "XKB MapNotify denial keeps selected legacy MappingNotify",
        build: &build,
        act: &act,
        expect: &expect,
    });
}

#[test]
// Focused because the original four-client K1–K4 sequence uses one fixture and
// checks each recipient's literal stream and writer health after every request.
// Mutation B11: change the literal K4 MapNotify changed mask from KeySyms 0x0002.
// Mutation B12: drive K3 through GetMap minor 8 instead of SetMap minor 9.
fn receive_xkb_new_keyboard_no_policy_checkpoints() {
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

    let check_clients = |fixture: &mut NotifyFixture, streams: &[(ClientId, Vec<u8>)]| {
        for (client, expected) in streams {
            assert_eq!(fixture.drain(*client), *expected, "client {client:?}");
        }
        assert_no_writer_failures(fixture);
    };
    let rules_streams = [
        (
            NOTIFY_SENDER,
            literal_rules_names_stream(REQUEST_SEQUENCE, ClientByteOrder::LittleEndian),
        ),
        (
            NOTIFY_RECEIVER_DENIED,
            literal_rules_names_stream(0x1234, ClientByteOrder::LittleEndian),
        ),
        (
            NOTIFY_RECEIVER_ALLOWED,
            literal_rules_names_stream(0x5678, ClientByteOrder::LittleEndian),
        ),
        (
            NON_XKB_CLIENT,
            literal_mapping_pair(0x6a2f, ClientByteOrder::LittleEndian),
        ),
    ];
    assert!(matches!(
        drive_rules_names_change(&mut fixture, REQUEST_SEQUENCE, b"evdev\0pc105\0de\0\0\0"),
        RequestOutcome::Handled
    ));
    check_clients(&mut fixture, &rules_streams);

    kbd_by_name_backend(&mut fixture);
    assert!(matches!(
        xkb_request(&mut fixture, NOTIFY_SENDER, KBD_BY_NAME_SEQUENCE, 23, &[]),
        RequestOutcome::Handled
    ));
    let kbd_streams = [
        (
            NOTIFY_SENDER,
            literal_kbd_by_name_stream(KBD_BY_NAME_SEQUENCE, ClientByteOrder::LittleEndian, true),
        ),
        (
            NOTIFY_RECEIVER_DENIED,
            literal_kbd_by_name_stream(0x1234, ClientByteOrder::LittleEndian, false),
        ),
        (
            NOTIFY_RECEIVER_ALLOWED,
            literal_kbd_by_name_stream(0x5678, ClientByteOrder::LittleEndian, false),
        ),
        (
            NON_XKB_CLIENT,
            literal_kbd_by_name_stream(0x6a2f, ClientByteOrder::LittleEndian, false),
        ),
    ];
    check_clients(&mut fixture, &kbd_streams);

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
        drive_xkb_set_map(&mut fixture, SET_MAP_SEQUENCE, &range_change_set_map_body()),
        RequestOutcome::Handled
    ));
    let range_change_streams = [
        (NOTIFY_SENDER, SET_MAP_SEQUENCE),
        (NOTIFY_RECEIVER_DENIED, 0x1234),
        (NOTIFY_RECEIVER_ALLOWED, 0x5678),
    ]
    .map(|(client, sequence)| {
        (
            client,
            literal_xkb_new_keyboard_notify(
                sequence,
                ClientByteOrder::LittleEndian,
                XKB_MAJOR_OPCODE,
                9,
                1,
            ),
        )
    })
    .into_iter()
    .chain([(
        NON_XKB_CLIENT,
        literal_mapping_pair(0x6a2f, ClientByteOrder::LittleEndian),
    )])
    .collect::<Vec<_>>();
    check_clients(&mut fixture, &range_change_streams);
    assert_ne!(
        fixture.state.keyboard_control.auto_repeats[usize::from(REPEAT_KEYCODE >> 3)]
            & (1 << (REPEAT_KEYCODE & 7)),
        0,
        "scripted Repeats still updates core key repeat state"
    );

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
    assert!(matches!(
        drive_xkb_set_map(&mut fixture, 0x5d9e, &keysym_set_map_body()),
        RequestOutcome::Handled
    ));
    let keysym_streams = [
        (NOTIFY_SENDER, 0x5d9e),
        (NOTIFY_RECEIVER_DENIED, 0x1234),
        (NOTIFY_RECEIVER_ALLOWED, 0x5678),
    ]
    .map(|(client, sequence)| {
        (
            client,
            literal_map_change_stream(sequence, ClientByteOrder::LittleEndian),
        )
    })
    .into_iter()
    .chain([(
        NON_XKB_CLIENT,
        literal_mapping_notify(0x6a2f, ClientByteOrder::LittleEndian, 1, 8, 1),
    )])
    .collect::<Vec<_>>();
    check_clients(&mut fixture, &keysym_streams);
}

// Mutation B21: bypass K1's NewKeyboardNotify Receive; this explicit Allow
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
        drive_xkb_set_map(&mut fixture, SET_MAP_SEQUENCE, &range_change_set_map_body(),),
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
                9,
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
    receive_requests.extend([request_opcodes(XKB_MAJOR_OPCODE, 9); 3]);
    let log = receive_log(&calls);
    assert_receive_calls_with_requests(
        &log,
        &receive_clients,
        &receive_requests,
        EventSubject::Device(1),
        &[
            XKB_NEW_KEYBOARD_NOTIFY_TYPE,
            XKB_NEW_KEYBOARD_NOTIFY_TYPE,
            XKB_NEW_KEYBOARD_NOTIFY_TYPE,
            XKB_MAP_NOTIFY_TYPE,
            XKB_MAP_NOTIFY_TYPE,
            XKB_MAP_NOTIFY_TYPE,
            XKB_NEW_KEYBOARD_NOTIFY_TYPE,
            XKB_NEW_KEYBOARD_NOTIFY_TYPE,
            XKB_NEW_KEYBOARD_NOTIFY_TYPE,
            XKB_NEW_KEYBOARD_NOTIFY_TYPE,
            XKB_NEW_KEYBOARD_NOTIFY_TYPE,
            XKB_NEW_KEYBOARD_NOTIFY_TYPE,
        ],
    );
}

#[test]
// Focused because the direct producer's returned recipient list is part of its contract, including denied clients.
// Mutation B22: use Device(1) rather than the payload device in the Receive subject.
// Mutation B23: return only policy-allowed recipients from send_xkb_map_notify.
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
            .unwrap()
            .last_sequence
            .store(sequence, Ordering::Relaxed);
    }
    let calls = fixture.install_policy(|_, check| {
        if check.hook == ExtensionHook::Receive
            && check.client == NOTIFY_RECEIVER_DENIED
            && check.receive == Some(EventSubject::Device(3))
            && check.events == Some(vec![xkb_event_description_for_test(XKB_MAP_NOTIFY_TYPE)])
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
            ClientByteOrder::LittleEndian,
            3,
            0x0002,
            0,
            8,
            1,
            0,
            0
        )
    );
    let log = receive_log(&calls);
    assert_receive_calls_with_requests(
        &log,
        &[NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED],
        &[None; 2],
        EventSubject::Device(3),
        &[XKB_MAP_NOTIFY_TYPE; 2],
    );
    assert_no_writer_failures(&fixture);
}

fn check_xkb_single_notify_case<Event, EventBytes>(
    name: &'static str,
    selection: u16,
    sequence: u16,
    request_minor: u8,
    expected_request_minor: u8,
    request_body: &'static [u8],
    xkb_type: u8,
    make_event: Event,
    event_bytes: EventBytes,
) where
    Event: Fn() -> XkbSetEvent + Copy,
    EventBytes: Fn(u16) -> Vec<u8> + Copy,
{
    use super::access_notify_support::{
        CallOrder, CaseExpect, ExpectedReceive, ReceiveCase, Stream, check_receive_case,
    };

    let selections = [
        (NOTIFY_RECEIVER_DENIED, selection),
        (NOTIFY_RECEIVER_ALLOWED, selection),
    ];
    let build = || {
        let mut fixture = indicator_fixture(&selections);
        seed_request_xkb_snapshots(&mut fixture);
        (fixture, ())
    };
    let act = |fixture: &mut NotifyFixture, _: &()| {
        drive_scripted_xkb_set(
            fixture,
            sequence,
            request_minor,
            request_body,
            vec![make_event()],
        );
    };
    let expect = |_: &()| {
        let receive = |client| ExpectedReceive {
            client,
            request: request_opcodes(XKB_MAJOR_OPCODE, expected_request_minor),
            subject: EventSubject::Device(1),
            events: vec![xkb_event_description_for_test(xkb_type)],
        };
        let denied = receive(NOTIFY_RECEIVER_DENIED);
        let allowed = receive(NOTIFY_RECEIVER_ALLOWED);
        let mut expectations = CaseExpect::new(
            vec![
                Stream::unchanged(NOTIFY_SENDER, Vec::new()),
                Stream {
                    client: NOTIFY_RECEIVER_DENIED,
                    bytes: event_bytes(0x1234),
                    denied: Some(Vec::new()),
                },
                Stream::unchanged(NOTIFY_RECEIVER_ALLOWED, event_bytes(0x5678)),
            ],
            vec![denied.clone(), allowed],
            CallOrder::Total,
            vec![denied],
        );
        expectations.deny_before_allow = true;
        expectations.after_deny = Some(Box::new(|allow, denied| {
            assert_xkb_notification_effects_match(denied, allow);
        }));
        expectations
    };
    check_receive_case(&ReceiveCase {
        name,
        build: &build,
        act: &act,
        expect: &expect,
    });
}

#[test]
// Mutation B31: skip K5 Receive; the targeted CompatMap denial must drop its copy.
fn receive_xkb_compat_map_denied() {
    check_xkb_single_notify_case(
        "XKB CompatMapNotify denial keeps the other recipient",
        XKB_COMPAT_MAP_SELECT,
        SET_COMPAT_SEQUENCE,
        XKB_SET_COMPAT_MAP,
        11,
        &XKB_SET_COMPAT_BODY,
        XKB_COMPAT_MAP_NOTIFY_TYPE,
        || XkbSetEvent::CompatMap(compat_map_notify()),
        |sequence| literal_xkb_compat_map_notify(sequence, compat_map_notify()),
    );
}

#[test]
// Focused because the original literal-field mutation belongs to a no-policy-only assertion.
// Mutation B24: change the literal CompatMapNotify changed_groups field from 5.
fn receive_xkb_indicator_set_no_policy_bytes() {
    let selections = [
        (NOTIFY_RECEIVER_DENIED, XKB_INDICATOR_EVENTS_SELECT),
        (NOTIFY_RECEIVER_ALLOWED, XKB_INDICATOR_EVENTS_SELECT),
    ];
    let mut fixture = indicator_fixture(&selections);
    assert!(!fixture.state.policies_installed());
    seed_request_xkb_snapshots(&mut fixture);
    drive_scripted_xkb_set(
        &mut fixture,
        SET_COMPAT_SEQUENCE,
        XKB_SET_COMPAT_MAP,
        &XKB_SET_COMPAT_BODY,
        vec![XkbSetEvent::CompatMap(compat_map_notify())],
    );
    drive_scripted_xkb_set(
        &mut fixture,
        SET_NAMES_SEQUENCE,
        XKB_SET_NAMES,
        &XKB_SET_NAMES_BODY,
        vec![XkbSetEvent::Names(names_notify()), indicator_names_change()],
    );
    drive_scripted_xkb_set(
        &mut fixture,
        SET_INDICATOR_MAP_SEQUENCE,
        XKB_SET_INDICATOR_MAP,
        &XKB_SET_INDICATOR_MAP_BODY,
        vec![XkbSetEvent::IndicatorMaps(indicator_maps_change())],
    );
    assert_eq!(
        fixture.drain(NOTIFY_RECEIVER_DENIED),
        literal_xkb_indicator_set_stream(0x1234)
    );
    assert_eq!(
        fixture.drain(NOTIFY_RECEIVER_ALLOWED),
        literal_xkb_indicator_set_stream(0x5678)
    );
    assert_no_writer_failures(&fixture);
}

#[test]
// Mutation B34: bypass K5 under allow-all; the full Receive log must fail.
// Mutation B35: drive all scripted Set* events through SetMap minor 9.
fn receive_xkb_indicator_set_allow_control() {
    use super::access_notify_support::{
        CallOrder, CaseExpect, ExpectedReceive, ReceiveCase, Stream, check_receive_case,
    };

    let selections = [
        (NOTIFY_RECEIVER_DENIED, XKB_INDICATOR_EVENTS_SELECT),
        (NOTIFY_RECEIVER_ALLOWED, XKB_INDICATOR_EVENTS_SELECT),
    ];
    let build = || {
        let mut fixture = indicator_fixture(&selections);
        seed_request_xkb_snapshots(&mut fixture);
        (fixture, ())
    };
    let act = |fixture: &mut NotifyFixture, _: &()| {
        drive_scripted_xkb_set(
            fixture,
            SET_COMPAT_SEQUENCE,
            XKB_SET_COMPAT_MAP,
            &XKB_SET_COMPAT_BODY,
            vec![XkbSetEvent::CompatMap(compat_map_notify())],
        );
        drive_scripted_xkb_set(
            fixture,
            SET_NAMES_SEQUENCE,
            XKB_SET_NAMES,
            &XKB_SET_NAMES_BODY,
            vec![XkbSetEvent::Names(names_notify()), indicator_names_change()],
        );
        drive_scripted_xkb_set(
            fixture,
            SET_INDICATOR_MAP_SEQUENCE,
            XKB_SET_INDICATOR_MAP,
            &XKB_SET_INDICATOR_MAP_BODY,
            vec![XkbSetEvent::IndicatorMaps(indicator_maps_change())],
        );
    };
    let expect = |_: &()| {
        let receive = |client, request, xkb_type| ExpectedReceive {
            client,
            request: request_opcodes(XKB_MAJOR_OPCODE, request),
            subject: EventSubject::Device(1),
            events: vec![xkb_event_description_for_test(xkb_type)],
        };
        let denied_state = receive(
            NOTIFY_RECEIVER_DENIED,
            XKB_SET_INDICATOR_MAP,
            XKB_INDICATOR_STATE_NOTIFY_TYPE,
        );
        let mut receives = Vec::new();
        for client in [NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED] {
            receives.push(receive(
                client,
                XKB_SET_COMPAT_MAP,
                XKB_COMPAT_MAP_NOTIFY_TYPE,
            ));
        }
        for xkb_type in [XKB_NAMES_NOTIFY_TYPE, XKB_EXTENSION_DEVICE_NOTIFY_TYPE] {
            for client in [NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED] {
                receives.push(receive(client, XKB_SET_NAMES, xkb_type));
            }
        }
        for xkb_type in [
            XKB_INDICATOR_MAP_NOTIFY_TYPE,
            XKB_INDICATOR_STATE_NOTIFY_TYPE,
            XKB_EXTENSION_DEVICE_NOTIFY_TYPE,
        ] {
            for client in [NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED] {
                receives.push(receive(client, XKB_SET_INDICATOR_MAP, xkb_type));
            }
        }
        let denied_bytes = [
            literal_xkb_compat_map_notify(0x1234, compat_map_notify()),
            literal_xkb_names_set_stream(0x1234),
            literal_xkb_indicator_notify(
                0x1234,
                yserver_protocol::x11::XkbIndicatorNotifyKind::Map,
                0x20,
                0x02,
            ),
            literal_xkb_extension_device_notify(0x1234, maps_extension_device_notify()),
        ]
        .concat();
        let mut expectations = CaseExpect::new(
            vec![
                Stream::unchanged(NOTIFY_SENDER, Vec::new()),
                Stream {
                    client: NOTIFY_RECEIVER_DENIED,
                    bytes: literal_xkb_indicator_set_stream(0x1234),
                    denied: Some(denied_bytes),
                },
                Stream::unchanged(
                    NOTIFY_RECEIVER_ALLOWED,
                    literal_xkb_indicator_set_stream(0x5678),
                ),
            ],
            receives,
            CallOrder::Total,
            vec![denied_state],
        );
        expectations.no_policy_last = true;
        expectations.after_deny = Some(Box::new(|allow, denied| {
            assert_xkb_notification_effects_match(denied, allow);
        }));
        expectations
    };
    check_receive_case(&ReceiveCase {
        name: "XKB indicator Set* allow control checks every event and request",
        build: &build,
        act: &act,
        expect: &expect,
    });
}

#[test]
// Mutation B29: skip K6 Receive for NamesNotify.
// Mutation B30: drive SetNames using SetMap minor 9.
fn receive_xkb_names_denied() {
    check_xkb_single_notify_case(
        "XKB NamesNotify denial keeps the other recipient",
        XKB_NAMES_SELECT,
        SET_NAMES_SEQUENCE,
        XKB_SET_NAMES,
        18,
        &XKB_SET_NAMES_BODY,
        XKB_NAMES_NOTIFY_TYPE,
        || XkbSetEvent::Names(names_notify()),
        |sequence| literal_xkb_names_notify(sequence, names_notify()),
    );
}

#[test]
// Mutation B32: skip K7 Receive; the denied ExtensionDeviceNotify must disappear.
// Mutation B33: require NamesNotify interest for the K7 extension-only recipient.
fn receive_xkb_indicator_names_denied() {
    use super::access_notify_support::{
        CallOrder, CaseExpect, ExpectedReceive, ReceiveCase, Stream, check_receive_case,
    };

    let selections = [
        (
            NOTIFY_RECEIVER_DENIED,
            XKB_NAMES_SELECT | XKB_EXTENSION_DEVICE_SELECT,
        ),
        (NOTIFY_RECEIVER_ALLOWED, XKB_EXTENSION_DEVICE_SELECT),
    ];
    let build = || {
        let mut fixture = indicator_fixture(&selections);
        seed_request_xkb_snapshots(&mut fixture);
        (fixture, ())
    };
    let act = |fixture: &mut NotifyFixture, _: &()| {
        drive_scripted_xkb_set(
            fixture,
            SET_NAMES_SEQUENCE,
            XKB_SET_NAMES,
            &XKB_SET_NAMES_BODY,
            vec![XkbSetEvent::Names(names_notify()), indicator_names_change()],
        );
    };
    let expect = |_: &()| {
        let receive = |client, xkb_type| ExpectedReceive {
            client,
            request: request_opcodes(XKB_MAJOR_OPCODE, 18),
            subject: EventSubject::Device(1),
            events: vec![xkb_event_description_for_test(xkb_type)],
        };
        let denied_names = receive(NOTIFY_RECEIVER_DENIED, XKB_NAMES_NOTIFY_TYPE);
        let denied_extension = receive(NOTIFY_RECEIVER_DENIED, XKB_EXTENSION_DEVICE_NOTIFY_TYPE);
        let allowed_extension = receive(NOTIFY_RECEIVER_ALLOWED, XKB_EXTENSION_DEVICE_NOTIFY_TYPE);
        let mut expectations = CaseExpect::new(
            vec![
                Stream::unchanged(NOTIFY_SENDER, Vec::new()),
                Stream {
                    client: NOTIFY_RECEIVER_DENIED,
                    bytes: literal_xkb_names_set_stream(0x1234),
                    denied: Some(literal_xkb_names_notify(0x1234, names_notify())),
                },
                Stream::unchanged(
                    NOTIFY_RECEIVER_ALLOWED,
                    literal_xkb_extension_device_notify(0x5678, names_extension_device_notify()),
                ),
            ],
            vec![denied_names, denied_extension.clone(), allowed_extension],
            CallOrder::Total,
            vec![denied_extension],
        );
        expectations.deny_before_allow = true;
        expectations.trace_before_deny_streams = true;
        expectations.no_policy_last = true;
        expectations.after_deny = Some(Box::new(|allow, denied| {
            assert_xkb_notification_effects_match(denied, allow)
        }));
        expectations
    };
    check_receive_case(&ReceiveCase {
        name: "XKB indicator names denial preserves per-client recipients",
        build: &build,
        act: &act,
        expect: &expect,
    });
}

#[test]
// Mutation B25: make the Map/State Receive subtype differ from the writer's xkbType.
// Mutation B26: reuse one recipient's K8 decision for every recipient.
fn receive_xkb_indicator_maps_denies_one_kind() {
    use super::access_notify_support::{
        CallOrder, CaseExpect, ExpectedReceive, ReceiveCase, Stream, check_receive_case,
    };

    let selections = [
        (
            NOTIFY_RECEIVER_DENIED,
            XKB_INDICATOR_MAP_SELECT | XKB_INDICATOR_STATE_SELECT | XKB_EXTENSION_DEVICE_SELECT,
        ),
        (NOTIFY_RECEIVER_ALLOWED, XKB_EXTENSION_DEVICE_SELECT),
    ];
    let build = || {
        let mut fixture = indicator_fixture(&selections);
        seed_request_xkb_snapshots(&mut fixture);
        (fixture, ())
    };
    let act = |fixture: &mut NotifyFixture, _: &()| {
        drive_scripted_xkb_set(
            fixture,
            SET_INDICATOR_MAP_SEQUENCE,
            XKB_SET_INDICATOR_MAP,
            &XKB_SET_INDICATOR_MAP_BODY,
            vec![XkbSetEvent::IndicatorMaps(indicator_maps_change())],
        );
    };
    let expect = |_: &()| {
        let receive = |client, xkb_type| ExpectedReceive {
            client,
            request: request_opcodes(XKB_MAJOR_OPCODE, 14),
            subject: EventSubject::Device(1),
            events: vec![xkb_event_description_for_test(xkb_type)],
        };
        let denied_map = receive(NOTIFY_RECEIVER_DENIED, XKB_INDICATOR_MAP_NOTIFY_TYPE);
        let denied_state = receive(NOTIFY_RECEIVER_DENIED, XKB_INDICATOR_STATE_NOTIFY_TYPE);
        let denied_extension = receive(NOTIFY_RECEIVER_DENIED, XKB_EXTENSION_DEVICE_NOTIFY_TYPE);
        let allowed_extension = receive(NOTIFY_RECEIVER_ALLOWED, XKB_EXTENSION_DEVICE_NOTIFY_TYPE);
        let denied_bytes = [
            literal_xkb_indicator_notify(
                0x1234,
                yserver_protocol::x11::XkbIndicatorNotifyKind::Map,
                0x20,
                0x02,
            ),
            literal_xkb_extension_device_notify(0x1234, maps_extension_device_notify()),
        ]
        .concat();
        let mut expectations = CaseExpect::new(
            vec![
                Stream::unchanged(NOTIFY_SENDER, Vec::new()),
                Stream {
                    client: NOTIFY_RECEIVER_DENIED,
                    bytes: literal_xkb_indicator_maps_stream(0x1234),
                    denied: Some(denied_bytes),
                },
                Stream::unchanged(
                    NOTIFY_RECEIVER_ALLOWED,
                    literal_xkb_extension_device_notify(0x5678, maps_extension_device_notify()),
                ),
            ],
            vec![
                denied_map,
                denied_state.clone(),
                denied_extension,
                allowed_extension,
            ],
            CallOrder::Total,
            vec![denied_state],
        );
        expectations.deny_before_allow = true;
        expectations.trace_before_deny_streams = true;
        expectations.after_deny = Some(Box::new(|allow, denied| {
            assert_xkb_notification_effects_match(denied, allow)
        }));
        expectations
    };
    check_receive_case(&ReceiveCase {
        name: "XKB IndicatorMap/State denial checks each xkbType and recipient",
        build: &build,
        act: &act,
        expect: &expect,
    });
}

#[test]
// Mutation B27: skip K10 Receive on IndicatorStateNotify.
// Mutation B28: return early from announce_xkb_state whenever a policy is installed.
fn receive_xkb_state_indicator_followups() {
    use super::access_notify_support::{
        CallOrder, CaseExpect, ExpectedReceive, ReceiveCase, Stream, check_receive_case,
    };

    let selections = [
        (NOTIFY_RECEIVER_DENIED, XKB_INDICATOR_FOLLOWUPS_SELECT),
        (NOTIFY_RECEIVER_ALLOWED, XKB_INDICATOR_FOLLOWUPS_SELECT),
    ];
    let build = || {
        let mut fixture = indicator_fixture(&selections);
        fixture.backend.xkb_mods = (0x40, 0x40, 0, 0);
        fixture.backend.xkb_indicators_result = (0x20, 0x3f);
        (fixture, ())
    };
    let act = |fixture: &mut NotifyFixture, _: &()| {
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
        assert_eq!(fixture.state.xkb_announced, (0, (0x40, 0x40, 0, 0)));
    };
    let expect = |_: &()| {
        let receive = |client, xkb_type| ExpectedReceive {
            client,
            request: None,
            subject: EventSubject::Device(1),
            events: vec![xkb_event_description_for_test(xkb_type)],
        };
        let denied_state = receive(NOTIFY_RECEIVER_DENIED, XKB_STATE_NOTIFY_TYPE);
        let allowed_state = receive(NOTIFY_RECEIVER_ALLOWED, XKB_STATE_NOTIFY_TYPE);
        let denied_followup = literal_xkb_key_indicator_stream(0x1234, false);
        let allowed_followup = literal_xkb_key_indicator_stream(0x5678, false);
        let mut expectations = CaseExpect::new(
            vec![
                Stream::unchanged(NOTIFY_SENDER, Vec::new()),
                Stream {
                    client: NOTIFY_RECEIVER_DENIED,
                    bytes: literal_xkb_key_indicator_stream(0x1234, true),
                    denied: Some(denied_followup),
                },
                Stream {
                    client: NOTIFY_RECEIVER_ALLOWED,
                    bytes: literal_xkb_key_indicator_stream(0x5678, true),
                    denied: Some(allowed_followup),
                },
            ],
            vec![
                denied_state.clone(),
                allowed_state.clone(),
                receive(NOTIFY_RECEIVER_DENIED, XKB_INDICATOR_STATE_NOTIFY_TYPE),
                receive(NOTIFY_RECEIVER_ALLOWED, XKB_INDICATOR_STATE_NOTIFY_TYPE),
                receive(NOTIFY_RECEIVER_DENIED, XKB_EXTENSION_DEVICE_NOTIFY_TYPE),
                receive(NOTIFY_RECEIVER_ALLOWED, XKB_EXTENSION_DEVICE_NOTIFY_TYPE),
            ],
            CallOrder::Total,
            vec![denied_state, allowed_state],
        );
        expectations.deny_before_allow = true;
        expectations.after_deny = Some(Box::new(|allow, denied| {
            assert_xkb_notification_effects_match(denied, allow);
            assert_eq!(denied.state.xkb_announced_indicators, 0x20);
        }));
        expectations
    };
    check_receive_case(&ReceiveCase {
        name: "XKB denied StateNotify keeps indicator follow-ups",
        build: &build,
        act: &act,
        expect: &expect,
    });
}
