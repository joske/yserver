use super::{
    extensions::{ExtensionHook, RecordedExtensionCheck, RecordingExtensionPolicy},
    *,
};
use crate::{
    access::{Decision, DenyError, EventDescription, EventSubject, RequestOpcodes},
    backend::recording::{RecordedCall, RecordingBackend},
    resources::{MapState, ROOT_WINDOW, ROOTLESS_SELECTION_WINDOW, SERVER_OWNER},
    rootless::{self, CoordinatePolicy, SelectionEvent},
    server::ServerState,
};
use std::{
    collections::HashMap,
    os::unix::net::UnixStream,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

const SENDER: ClientId = ClientId(1);
const RECEIVER_A: ClientId = ClientId(2);
const RECEIVER_B: ClientId = ClientId(3);
const RECEIVER_C: ClientId = ClientId(4);
const PARENT: ResourceId = ResourceId(0x0051_0001);
const MIDDLE: ResourceId = ResourceId(0x0051_0002);
const LEAF: ResourceId = ResourceId(0x0051_0003);
const EVENT_MASK: u32 = 1 << 2;
const SUBSTRUCTURE_NOTIFY_MASK: u32 = 1 << 19;
const CORE_SEND: RequestOpcodes = RequestOpcodes {
    major: 25,
    minor: 0,
};
const XI_SEND: RequestOpcodes = RequestOpcodes {
    major: 137,
    minor: 31,
};
const XI_DEVICE: u8 = 4;
const DENY_ERROR: DenyError = DenyError {
    code: x11::error::BAD_VALUE,
    value: 0x55,
};

struct Fixture {
    state: ServerState,
    backend: RecordingBackend,
    peers: HashMap<u32, UnixStream>,
}

impl Fixture {
    fn new() -> Self {
        let mut state = ServerState::new();
        let mut peers = HashMap::new();
        for id in [SENDER, RECEIVER_A, RECEIVER_B, RECEIVER_C] {
            peers.insert(id.0, install_client(&mut state, id.0));
        }
        create_window(&mut state, PARENT, ROOT_WINDOW);
        create_window(&mut state, MIDDLE, PARENT);
        create_window(&mut state, LEAF, MIDDLE);
        Self {
            state,
            backend: RecordingBackend::new(),
            peers,
        }
    }

    fn install_policy(
        &mut self,
        decide: impl Fn(usize, &RecordedExtensionCheck) -> Decision + Send + Sync + 'static,
    ) -> Arc<Mutex<Vec<RecordedExtensionCheck>>> {
        let calls = Arc::new(Mutex::new(Vec::new()));
        self.state
            .install_access_policy(Arc::new(RecordingExtensionPolicy::custom(
                Arc::clone(&calls),
                "",
                decide,
            )));
        calls
    }

    fn select(&mut self, client: ClientId, window: ResourceId, mask: u32) {
        self.state
            .clients
            .get_mut(&client.0)
            .expect("client")
            .event_masks
            .insert(window, mask);
    }

    fn send_event(
        &mut self,
        sequence: u16,
        destination: u32,
        mask: u32,
        propagate: u8,
        event: [u8; 32],
    ) -> Vec<u8> {
        let mut body = Vec::with_capacity(40);
        body.extend_from_slice(&destination.to_le_bytes());
        body.extend_from_slice(&mask.to_le_bytes());
        body.extend_from_slice(&event);
        process_request(
            &mut self.state,
            &mut self.backend,
            SENDER,
            SequenceNumber(sequence),
            RequestHeader {
                opcode: 25,
                data: propagate,
                length_units: 11,
            },
            &body,
            None,
        )
        .expect("SendEvent through process_request");
        self.drain(SENDER)
    }

    fn send_extension_event(
        &mut self,
        sequence: u16,
        destination: u32,
        propagate: bool,
        events: &[[u8; 32]],
        class: u32,
    ) -> Vec<u8> {
        self.send_extension_event_with_byte_order(
            sequence,
            destination,
            propagate,
            events,
            class,
            yserver_protocol::x11::ClientByteOrder::LittleEndian,
        )
    }

    fn send_extension_event_with_byte_order(
        &mut self,
        sequence: u16,
        destination: u32,
        propagate: bool,
        events: &[[u8; 32]],
        class: u32,
        byte_order: yserver_protocol::x11::ClientByteOrder,
    ) -> Vec<u8> {
        let mut body = Vec::with_capacity(12 + events.len() * 32 + 4);
        body.extend_from_slice(&destination.to_le_bytes());
        body.push(XI_DEVICE);
        body.push(u8::from(propagate));
        body.extend_from_slice(&1u16.to_le_bytes());
        body.push(u8::try_from(events.len()).expect("XI1 event count"));
        body.extend_from_slice(&[0; 3]);
        for event in events {
            body.extend_from_slice(event);
        }
        match byte_order {
            yserver_protocol::x11::ClientByteOrder::LittleEndian => {
                body.extend_from_slice(&class.to_le_bytes());
            }
            yserver_protocol::x11::ClientByteOrder::BigEndian => {
                body.extend_from_slice(&class.to_be_bytes());
            }
        }
        let length_units = 4 + 8 * u32::try_from(events.len()).expect("XI1 event count") + 1;
        process_request(
            &mut self.state,
            &mut self.backend,
            SENDER,
            SequenceNumber(sequence),
            RequestHeader {
                opcode: XI_SEND.major,
                data: XI_SEND.minor,
                length_units,
            },
            &body,
            None,
        )
        .expect("SendExtensionEvent through process_request");
        self.drain(SENDER)
    }

    fn drain(&mut self, client: ClientId) -> Vec<u8> {
        read_all_available(self.peers.get_mut(&client.0).expect("peer"))
    }

    fn drain_all(&mut self) -> HashMap<u32, Vec<u8>> {
        [SENDER, RECEIVER_A, RECEIVER_B, RECEIVER_C]
            .into_iter()
            .map(|client| (client.0, self.drain(client)))
            .collect()
    }
}

fn send_state_snapshot(state: &ServerState) -> String {
    let event_masks: Vec<_> = state
        .clients
        .iter()
        .map(|(id, client)| (*id, &client.event_masks))
        .collect();
    format!(
        "{:?}",
        (
            &state.resources,
            &state.rootless,
            state.core_focus,
            state.pointer_root,
            event_masks,
        )
    )
}

fn xi_send_state_snapshot(state: &ServerState) -> String {
    let xi1_classes: Vec<_> = state
        .clients
        .iter()
        .map(|(id, client)| (*id, &client.xi1_window_event_classes))
        .collect();
    format!(
        "{:?}",
        (
            send_state_snapshot(state),
            xi1_classes,
            &state.xi1_window_dont_propagate,
        )
    )
}

fn create_window(state: &mut ServerState, window: ResourceId, parent: ResourceId) {
    state.resources.create_window(
        SENDER,
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window,
            parent,
            width: 100,
            height: 100,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    state
        .resources
        .window_mut(window)
        .expect("new window")
        .map_state = MapState::Viewable;
}

fn key_event() -> [u8; 32] {
    let mut event = [0; 32];
    event[0] = 2; // KeyPress, without SendEvent's high bit.
    event[1] = 7;
    event[4..8].copy_from_slice(&LEAF.0.to_le_bytes());
    event
}

fn sent_key_event(sequence: u16) -> [u8; 32] {
    let mut event = key_event();
    event[0] |= 0x80;
    event[2..4].copy_from_slice(&sequence.to_le_bytes());
    event
}

fn xi_event(event_type: u8) -> [u8; 32] {
    let mut event = [0; 32];
    event[0] = event_type;
    event[4..8].copy_from_slice(&LEAF.0.to_le_bytes());
    event
}

fn xi_event_pair() -> [[u8; 32]; 2] {
    [
        xi_event(crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_KEY_PRESS_OFFSET),
        xi_event(crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_BUTTON_PRESS_OFFSET),
    ]
}

fn xi_event_class(event_type: u8) -> u32 {
    (u32::from(XI_DEVICE) << 8) | u32::from(event_type)
}

fn xi_event_descriptions(events: &[[u8; 32]]) -> Vec<EventDescription> {
    events
        .iter()
        .map(|event| {
            let event_type = event[0] & 0x7f;
            EventDescription::extension(
                event_type,
                XI_SEND.major,
                u16::from(event_type - crate::server::XI_FIRST_EVENT),
            )
        })
        .collect()
}

fn sent_xi_events(events: &[[u8; 32]], sequence: u16) -> Vec<u8> {
    let mut expected = Vec::with_capacity(events.len() * 32);
    for event in events {
        let mut sent = *event;
        sent[0] |= 0x80;
        sent[2..4].copy_from_slice(&sequence.to_le_bytes());
        expected.extend_from_slice(&sent);
    }
    expected
}

fn assert_core_send(call: &RecordedExtensionCheck, window: ResourceId) {
    assert_eq!(call.hook, ExtensionHook::Send);
    assert_eq!(call.client, SENDER);
    assert_eq!(call.send, Some((window, None)));
    assert_eq!(call.events, Some(vec![EventDescription::core(2)]));
    assert_eq!(call.request, Some(CORE_SEND));
}

fn assert_send_type(call: &RecordedExtensionCheck, window: ResourceId, type_code: u8) {
    assert_eq!(call.hook, ExtensionHook::Send);
    assert_eq!(call.client, SENDER);
    assert_eq!(call.send, Some((window, None)));
    assert_eq!(call.events, Some(vec![EventDescription::core(type_code)]));
    assert_eq!(call.request, Some(CORE_SEND));
}

fn assert_extension_send(
    call: &RecordedExtensionCheck,
    window: ResourceId,
    type_code: u8,
    major_opcode: u8,
    event_type: u16,
) {
    assert_eq!(call.hook, ExtensionHook::Send);
    assert_eq!(call.client, SENDER);
    assert_eq!(call.send, Some((window, None)));
    assert_eq!(
        call.events,
        Some(vec![EventDescription::extension(
            type_code,
            major_opcode,
            event_type
        )])
    );
    assert_eq!(call.request, Some(CORE_SEND));
}

fn assert_extension_receive(
    call: &RecordedExtensionCheck,
    client: ClientId,
    window: ResourceId,
    type_code: u8,
    major_opcode: u8,
    event_type: u16,
) {
    assert_eq!(call.hook, ExtensionHook::Receive);
    assert_eq!(call.client, client);
    assert_eq!(call.receive, Some(EventSubject::Window(window)));
    assert_eq!(
        call.events,
        Some(vec![EventDescription::extension(
            type_code,
            major_opcode,
            event_type
        )])
    );
    assert_eq!(call.request, Some(CORE_SEND));
}

fn assert_core_receive(call: &RecordedExtensionCheck, client: ClientId, window: ResourceId) {
    assert_eq!(call.hook, ExtensionHook::Receive);
    assert_eq!(call.client, client);
    assert_eq!(call.receive, Some(EventSubject::Window(window)));
    assert_eq!(call.events, Some(vec![EventDescription::core(2)]));
    assert_eq!(call.request, Some(CORE_SEND));
}

fn assert_receive_type(
    call: &RecordedExtensionCheck,
    client: ClientId,
    window: ResourceId,
    type_code: u8,
) {
    assert_eq!(call.hook, ExtensionHook::Receive);
    assert_eq!(call.client, client);
    assert_eq!(call.receive, Some(EventSubject::Window(window)));
    assert_eq!(call.events, Some(vec![EventDescription::core(type_code)]));
    assert_eq!(call.request, Some(CORE_SEND));
}

fn assert_xi_send(
    call: &RecordedExtensionCheck,
    window: ResourceId,
    descriptions: &[EventDescription],
) {
    assert_eq!(call.hook, ExtensionHook::Send);
    assert_eq!(call.client, SENDER);
    assert_eq!(call.send, Some((window, Some(u16::from(XI_DEVICE)))));
    assert_eq!(call.events.as_deref(), Some(descriptions));
    assert_eq!(call.request, Some(XI_SEND));
}

fn assert_xi_receive(
    call: &RecordedExtensionCheck,
    client: ClientId,
    window: ResourceId,
    descriptions: &[EventDescription],
) {
    assert_eq!(call.hook, ExtensionHook::Receive);
    assert_eq!(call.client, client);
    assert_eq!(call.receive, Some(EventSubject::Window(window)));
    assert_eq!(call.events.as_deref(), Some(descriptions));
    assert_eq!(call.request, Some(XI_SEND));
}

// Mutation: omit check_send from the SendEvent delivery path.
#[test]
fn send_event_ask_rows_cover_pointer_focus_and_explicit_destinations() {
    for (destination, propagate) in [(0, 0), (0, 1), (1, 0), (1, 1), (LEAF.0, 0), (LEAF.0, 1)] {
        let mut fixture = Fixture::new();
        fixture.state.pointer_root = (5, 5);
        fixture.state.core_focus.raw = if destination == 1 { LEAF.0 } else { 0 };
        fixture.select(RECEIVER_A, LEAF, EVENT_MASK);
        let calls = fixture.install_policy(|_, _| Decision::Allow);

        assert!(
            fixture
                .send_event(1, destination, EVENT_MASK, propagate, key_event())
                .is_empty()
        );
        let calls = calls.lock().unwrap();
        assert_eq!(
            calls.len(),
            2,
            "destination={destination}, propagate={propagate}: {calls:?}"
        );
        assert_core_send(&calls[0], LEAF);
        assert_core_receive(&calls[1], RECEIVER_A, LEAF);
        assert_eq!(fixture.drain(RECEIVER_A), sent_key_event(0));
    }
}

// Mutation: classify an advertised extension event as a core event.
#[test]
fn send_event_ask_describes_advertised_extension_metadata() {
    let mut fixture = Fixture::new();
    fixture.select(RECEIVER_A, LEAF, EVENT_MASK);
    let calls = fixture.install_policy(|_, _| Decision::Allow);
    let mut event = key_event();
    event[0] = 89; // RANDR ScreenChangeNotify, first event code 0.

    assert!(
        fixture
            .send_event(1, LEAF.0, EVENT_MASK, 0, event)
            .is_empty()
    );
    let mut expected_event = event;
    expected_event[0] |= 0x80;
    assert_eq!(fixture.drain(RECEIVER_A), expected_event);

    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_extension_send(&calls[0], LEAF, 89, 128, 0);
    assert_extension_receive(&calls[1], RECEIVER_A, LEAF, 89, 128, 0);
}

// Mutation: filter extension events by the registry's static first_event,
// which omits XKEYBOARD's live event base.
#[test]
fn send_event_xkb_live_event_base_is_described_and_denied() {
    let mut fixture = Fixture::new();
    fixture.backend.xkb_info_result = Some((136, 85, 162));
    fixture.select(RECEIVER_A, LEAF, EVENT_MASK);
    let xkb_description = EventDescription::extension(85, 136, 0);
    let calls = fixture.install_policy(move |_, call| {
        if call.hook == ExtensionHook::Receive
            && call.events.as_deref() == Some(std::slice::from_ref(&xkb_description))
        {
            Decision::Deny(DENY_ERROR)
        } else {
            Decision::Allow
        }
    });
    let backend_before = fixture.backend.calls();
    let state_before = send_state_snapshot(&fixture.state);
    let mut event = key_event();
    event[0] = 85; // XKB's live first event code from xkb_info().

    assert!(
        fixture
            .send_event(1, LEAF.0, EVENT_MASK, 0, event)
            .is_empty()
    );
    let observed = fixture.drain_all();
    assert!(
        observed.values().all(Vec::is_empty),
        "denied XKB SendEvent wrote bytes: {observed:?}"
    );
    assert_eq!(fixture.backend.calls(), backend_before);
    assert_eq!(send_state_snapshot(&fixture.state), state_before);

    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_extension_send(&calls[0], LEAF, 85, 136, 0);
    assert_extension_receive(&calls[1], RECEIVER_A, LEAF, 85, 136, 0);
}

// Mutation: skip the destination send check and allow selected clients to receive.
#[test]
fn send_denied_at_destination_is_success_without_delivery() {
    let mut fixture = Fixture::new();
    fixture.select(RECEIVER_A, LEAF, EVENT_MASK);
    fixture.select(RECEIVER_B, LEAF, EVENT_MASK);
    let calls = fixture.install_policy(|_, call| {
        if call.hook == ExtensionHook::Send {
            Decision::Deny(DENY_ERROR)
        } else {
            Decision::Allow
        }
    });
    let backend_before = fixture.backend.calls();
    let state_before = send_state_snapshot(&fixture.state);

    assert!(
        fixture
            .send_event(1, LEAF.0, EVENT_MASK, 0, key_event())
            .is_empty()
    );
    let observed = fixture.drain_all();
    assert!(
        observed.values().all(Vec::is_empty),
        "denied send wrote bytes: {observed:?}"
    );
    assert_eq!(fixture.backend.calls(), backend_before);
    assert_eq!(send_state_snapshot(&fixture.state), state_before);
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_core_send(&calls[0], LEAF);
}

// Mutation: run the propagation send check only at the original destination.
#[test]
fn send_denied_at_middle_candidate_stops_propagation() {
    let mut fixture = Fixture::new();
    fixture.select(RECEIVER_A, MIDDLE, EVENT_MASK);
    fixture.select(RECEIVER_B, PARENT, EVENT_MASK);
    let calls = fixture.install_policy(|_, call| {
        if call.hook == ExtensionHook::Send && call.send == Some((MIDDLE, None)) {
            Decision::Deny(DENY_ERROR)
        } else {
            Decision::Allow
        }
    });
    let backend_before = fixture.backend.calls();
    let state_before = send_state_snapshot(&fixture.state);

    assert!(
        fixture
            .send_event(1, LEAF.0, EVENT_MASK, 1, key_event())
            .is_empty()
    );
    let observed = fixture.drain_all();
    assert!(
        observed.values().all(Vec::is_empty),
        "denied propagation wrote bytes: {observed:?}"
    );
    assert_eq!(fixture.backend.calls(), backend_before);
    assert_eq!(send_state_snapshot(&fixture.state), state_before);
    let calls = calls.lock().unwrap();
    assert_eq!(
        calls.len(),
        2,
        "send denial must precede any receive at MIDDLE: {calls:?}"
    );
    assert_core_send(&calls[0], LEAF);
    assert_core_send(&calls[1], MIDDLE);
}

// Mutation: stop propagation as soon as a window has selectors, even if all are denied.
#[test]
fn receive_denied_for_all_selectors_continues_to_parent() {
    let mut fixture = Fixture::new();
    fixture.select(RECEIVER_A, LEAF, EVENT_MASK);
    fixture.select(RECEIVER_B, MIDDLE, EVENT_MASK);
    let calls = fixture.install_policy(|_, call| {
        if call.hook == ExtensionHook::Receive && call.client == RECEIVER_A {
            Decision::Deny(DENY_ERROR)
        } else {
            Decision::Allow
        }
    });
    let backend_before = fixture.backend.calls();
    let state_before = send_state_snapshot(&fixture.state);

    assert!(
        fixture
            .send_event(1, LEAF.0, EVENT_MASK, 1, key_event())
            .is_empty()
    );
    assert_eq!(fixture.drain(RECEIVER_A), Vec::<u8>::new());
    assert_eq!(fixture.drain(RECEIVER_B), sent_key_event(0));
    assert_eq!(fixture.backend.calls(), backend_before);
    assert_eq!(send_state_snapshot(&fixture.state), state_before);
    let calls = calls.lock().unwrap();
    assert_eq!(
        calls.iter().map(|call| call.hook).collect::<Vec<_>>(),
        [
            ExtensionHook::Send,
            ExtensionHook::Receive,
            ExtensionHook::Send,
            ExtensionHook::Receive,
        ]
    );
    assert_core_send(&calls[0], LEAF);
    assert_core_receive(&calls[1], RECEIVER_A, LEAF);
    assert_core_send(&calls[2], MIDDLE);
    assert_core_receive(&calls[3], RECEIVER_B, MIDDLE);
}

// Mutation: continue to a parent after the zero-mask creator refuses the event.
#[test]
fn receive_denied_creator_under_zero_mask_stops() {
    let mut fixture = Fixture::new();
    let calls = fixture.install_policy(|_, call| {
        if call.hook == ExtensionHook::Receive && call.client == SENDER {
            Decision::Deny(DENY_ERROR)
        } else {
            Decision::Allow
        }
    });
    let backend_before = fixture.backend.calls();
    let state_before = send_state_snapshot(&fixture.state);

    assert!(fixture.send_event(1, LEAF.0, 0, 1, key_event()).is_empty());
    assert!(fixture.drain_all().values().all(Vec::is_empty));
    assert_eq!(fixture.backend.calls(), backend_before);
    assert_eq!(send_state_snapshot(&fixture.state), state_before);
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_core_send(&calls[0], LEAF);
    assert_core_receive(&calls[1], SENDER, LEAF);
}

// Mutation: stop at the first selected recipient even when that recipient is denied.
#[test]
fn receive_denied_one_of_two_selectors() {
    let mut fixture = Fixture::new();
    fixture.select(RECEIVER_A, LEAF, EVENT_MASK);
    fixture.select(RECEIVER_B, LEAF, EVENT_MASK);
    fixture.select(RECEIVER_C, MIDDLE, EVENT_MASK);
    let calls = fixture.install_policy(|_, call| {
        if call.hook == ExtensionHook::Receive && call.client == RECEIVER_A {
            Decision::Deny(DENY_ERROR)
        } else {
            Decision::Allow
        }
    });
    let backend_before = fixture.backend.calls();
    let state_before = send_state_snapshot(&fixture.state);

    assert!(
        fixture
            .send_event(1, LEAF.0, EVENT_MASK, 1, key_event())
            .is_empty()
    );
    assert!(fixture.drain(RECEIVER_A).is_empty());
    assert_eq!(fixture.drain(RECEIVER_B), sent_key_event(0));
    assert!(fixture.drain(RECEIVER_C).is_empty());
    assert_eq!(fixture.backend.calls(), backend_before);
    assert_eq!(send_state_snapshot(&fixture.state), state_before);
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 3);
    assert_core_send(&calls[0], LEAF);
    for call in &calls[1..] {
        assert!(matches!(call.client, RECEIVER_A | RECEIVER_B));
        assert_core_receive(call, call.client, LEAF);
    }
    let mut checked_clients: Vec<_> = calls[1..].iter().map(|call| call.client).collect();
    checked_clients.sort_unstable_by_key(|client| client.0);
    assert_eq!(checked_clients, [RECEIVER_A, RECEIVER_B]);
}

fn enable_rootless(fixture: &mut Fixture) {
    fixture.backend = RecordingBackend::new().with_redirect_activation();
    rootless::enable(
        &mut fixture.state,
        &mut fixture.backend,
        CoordinatePolicy::Opaque,
    )
    .expect("enable rootless mode");
    let _ = rootless::selection::selection_window(&mut fixture.state);
}

fn map_rootless_top_level(fixture: &mut Fixture, window: ResourceId) {
    fixture
        .state
        .resources
        .window_mut(window)
        .expect("top-level")
        .map_state = MapState::Unmapped;
    process_request(
        &mut fixture.state,
        &mut fixture.backend,
        SENDER,
        SequenceNumber(1),
        RequestHeader {
            opcode: 8,
            data: 0,
            length_units: 2,
        },
        &window.0.to_le_bytes(),
        None,
    )
    .expect("MapWindow through process_request");
    let _ = fixture.drain_all();
    rootless::reconcile_and_dispatch(&mut fixture.state, &mut fixture.backend);
}

fn selection_notify_event() -> [u8; 32] {
    let mut event = [0; 32];
    event[0] = 31;
    event[8..12].copy_from_slice(&ROOTLESS_SELECTION_WINDOW.0.to_le_bytes());
    event[12..16].copy_from_slice(&0x100u32.to_le_bytes());
    event[16..20].copy_from_slice(&0x101u32.to_le_bytes());
    event[20..24].copy_from_slice(&0x102u32.to_le_bytes());
    event
}

// Mutation: bypass the denied send check and run SelectionNotify interception.
#[test]
fn rootless_selection_notify_send_denied_skips_interception() {
    let mut fixture = Fixture::new();
    enable_rootless(&mut fixture);
    let calls = fixture.install_policy(|_, call| {
        if call.hook == ExtensionHook::Send {
            Decision::Deny(DENY_ERROR)
        } else {
            Decision::Allow
        }
    });
    let backend_before = fixture.backend.calls();
    let state_before = send_state_snapshot(&fixture.state);

    assert!(
        fixture
            .send_event(
                1,
                ROOTLESS_SELECTION_WINDOW.0,
                0,
                0,
                selection_notify_event()
            )
            .is_empty()
    );
    assert!(rootless::selection::take_selection_events(&mut fixture.state).is_empty());
    assert_eq!(fixture.backend.calls(), backend_before);
    assert_eq!(send_state_snapshot(&fixture.state), state_before);
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].hook, ExtensionHook::Send);
    assert_eq!(calls[0].send, Some((ROOTLESS_SELECTION_WINDOW, None)));
    assert_eq!(calls[0].events, Some(vec![EventDescription::core(31)]));
}

// Mutation: queue SelectionEvent::Notify before asking SERVER_OWNER to receive it.
#[test]
fn rootless_selection_notify_server_receive_denied_queues_nothing() {
    let mut fixture = Fixture::new();
    enable_rootless(&mut fixture);
    let calls = fixture.install_policy(|_, call| {
        if call.hook == ExtensionHook::Receive && call.client == SERVER_OWNER {
            Decision::Deny(DENY_ERROR)
        } else {
            Decision::Allow
        }
    });
    let backend_before = fixture.backend.calls();
    let state_before = send_state_snapshot(&fixture.state);

    assert!(
        fixture
            .send_event(
                1,
                ROOTLESS_SELECTION_WINDOW.0,
                0,
                0,
                selection_notify_event()
            )
            .is_empty()
    );
    assert!(
        rootless::selection::take_selection_events(&mut fixture.state)
            .iter()
            .all(|event| !matches!(event, SelectionEvent::Notify { .. }))
    );
    assert_eq!(fixture.backend.calls(), backend_before);
    assert_eq!(send_state_snapshot(&fixture.state), state_before);
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].hook, ExtensionHook::Send);
    assert_receive_type(&calls[1], SERVER_OWNER, ROOTLESS_SELECTION_WINDOW, 31);
}

fn root_client_message(message_type: u32, window: ResourceId, data: [u32; 5]) -> [u8; 32] {
    let mut event = [0; 32];
    event[0] = 33;
    event[1] = 32;
    event[4..8].copy_from_slice(&window.0.to_le_bytes());
    event[8..12].copy_from_slice(&message_type.to_le_bytes());
    for (index, value) in data.into_iter().enumerate() {
        event[12 + index * 4..16 + index * 4].copy_from_slice(&value.to_le_bytes());
    }
    event
}

// Mutation: turn a SERVER_OWNER receive denial into a whole-request denial before listener delivery.
#[test]
fn rootless_root_client_message_server_receive_denied_keeps_listeners() {
    let mut fixture = Fixture::new();
    enable_rootless(&mut fixture);
    map_rootless_top_level(&mut fixture, PARENT);
    fixture.select(RECEIVER_A, ROOT_WINDOW, SUBSTRUCTURE_NOTIFY_MASK);
    let request_extents = fixture
        .state
        .atoms
        .intern("_NET_REQUEST_FRAME_EXTENTS", false)
        .0;
    let wm_state = fixture.state.atoms.intern("_NET_WM_STATE", false).0;
    let state_above = fixture.state.atoms.intern("_NET_WM_STATE_ABOVE", false).0;
    let calls = fixture.install_policy(|_, call| {
        if call.hook == ExtensionHook::Receive && call.client == SERVER_OWNER {
            Decision::Deny(DENY_ERROR)
        } else {
            Decision::Allow
        }
    });
    let backend_before = fixture.backend.calls();
    let state_before = send_state_snapshot(&fixture.state);

    assert!(
        fixture
            .send_event(
                1,
                ROOT_WINDOW.0,
                SUBSTRUCTURE_NOTIFY_MASK,
                0,
                root_client_message(wm_state, PARENT, [1, state_above, 0, 0, 0]),
            )
            .is_empty()
    );
    assert!(
        fixture
            .send_event(
                2,
                ROOT_WINDOW.0,
                SUBSTRUCTURE_NOTIFY_MASK,
                0,
                root_client_message(request_extents, LEAF, [0; 5]),
            )
            .is_empty()
    );
    let listener_events = fixture.drain(RECEIVER_A);
    assert_eq!(listener_events.len(), 64);
    assert_eq!(listener_events[0], 0x80 | 33);
    assert_eq!(listener_events[32], 0x80 | 33);

    assert_eq!(send_state_snapshot(&fixture.state), state_before);
    let frame_extents = fixture.state.atoms.intern("_NET_FRAME_EXTENTS", false);
    assert!(
        !fixture
            .state
            .resources
            .window(LEAF)
            .unwrap()
            .properties
            .contains_key(&frame_extents)
    );
    assert_eq!(fixture.backend.calls(), backend_before);
    let calls_before_reconcile = fixture.backend.calls();
    rootless::reconcile_and_dispatch(&mut fixture.state, &mut fixture.backend);
    let calls_after_reconcile = fixture.backend.calls();
    assert!(
        !calls_after_reconcile[calls_before_reconcile.len()..]
            .iter()
            .any(|call| matches!(call, RecordedCall::RootlessEvents(events)
                if events.iter().any(|event| matches!(event, rootless::RootlessEvent::WmRequest { .. })))),
        "denied root message queued a rootless WM request"
    );
    let calls = calls.lock().unwrap();
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.hook == ExtensionHook::Send)
            .count(),
        2
    );
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.hook == ExtensionHook::Receive && call.client == RECEIVER_A)
            .count(),
        2
    );
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.hook == ExtensionHook::Receive && call.client == SERVER_OWNER)
            .count(),
        2
    );
    assert!(
        calls
            .iter()
            .filter(|call| call.hook == ExtensionHook::Receive && call.client == SERVER_OWNER)
            .all(
                |call| call.receive == Some(EventSubject::Window(ROOT_WINDOW))
                    && call.request == Some(CORE_SEND)
            )
    );
    assert!(
        calls
            .iter()
            .filter(|call| call.hook == ExtensionHook::Receive && call.client == SERVER_OWNER)
            .all(|call| call.events == Some(vec![EventDescription::core(33)]))
    );
}

// Mutation: cache the first send or receive decision across identical requests.
#[test]
fn send_and_receive_revocation_is_checked_on_each_request() {
    let mut fixture = Fixture::new();
    fixture.select(RECEIVER_A, LEAF, EVENT_MASK);
    let revoke_send = Arc::new(AtomicBool::new(false));
    let revoke_receive = Arc::new(AtomicBool::new(false));
    let send_flag = Arc::clone(&revoke_send);
    let receive_flag = Arc::clone(&revoke_receive);
    let calls = fixture.install_policy(move |_, call| {
        if call.hook == ExtensionHook::Send && send_flag.load(Ordering::SeqCst)
            || call.hook == ExtensionHook::Receive
                && call.client == RECEIVER_A
                && receive_flag.load(Ordering::SeqCst)
        {
            Decision::Deny(DENY_ERROR)
        } else {
            Decision::Allow
        }
    });

    assert!(
        fixture
            .send_event(1, LEAF.0, EVENT_MASK, 0, key_event())
            .is_empty()
    );
    assert_eq!(fixture.drain(RECEIVER_A), sent_key_event(0));
    revoke_send.store(true, Ordering::SeqCst);
    let state_before_send_deny = send_state_snapshot(&fixture.state);
    assert!(
        fixture
            .send_event(2, LEAF.0, EVENT_MASK, 0, key_event())
            .is_empty()
    );
    assert!(fixture.drain(RECEIVER_A).is_empty());
    assert_eq!(
        send_state_snapshot(&fixture.state),
        state_before_send_deny,
        "send denial changed delivery state"
    );
    revoke_send.store(false, Ordering::SeqCst);
    revoke_receive.store(true, Ordering::SeqCst);
    let state_before_receive_deny = send_state_snapshot(&fixture.state);
    assert!(
        fixture
            .send_event(3, LEAF.0, EVENT_MASK, 0, key_event())
            .is_empty()
    );
    assert!(fixture.drain(RECEIVER_A).is_empty());
    assert_eq!(
        send_state_snapshot(&fixture.state),
        state_before_receive_deny,
        "receive denial changed delivery state"
    );

    let calls = calls.lock().unwrap();
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.hook == ExtensionHook::Send)
            .count(),
        3
    );
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.hook == ExtensionHook::Receive)
            .count(),
        2
    );
}

// Mutation: skip the bridge SERVER_OWNER receive check on SelectionNotify.
#[test]
fn rootless_server_receptions_are_asked_with_subject_and_opcodes() {
    let mut fixture = Fixture::new();
    enable_rootless(&mut fixture);
    let calls = fixture.install_policy(|_, _| Decision::Allow);

    assert!(
        fixture
            .send_event(
                1,
                ROOTLESS_SELECTION_WINDOW.0,
                0,
                0,
                selection_notify_event()
            )
            .is_empty()
    );
    assert_eq!(
        rootless::selection::take_selection_events(&mut fixture.state).len(),
        1
    );

    let request_extents = fixture
        .state
        .atoms
        .intern("_NET_REQUEST_FRAME_EXTENTS", false)
        .0;
    assert!(
        fixture
            .send_event(
                2,
                ROOT_WINDOW.0,
                0,
                0,
                root_client_message(request_extents, PARENT, [0; 5]),
            )
            .is_empty()
    );

    let calls = calls.lock().unwrap();
    let server_receives: Vec<_> = calls
        .iter()
        .filter(|call| call.hook == ExtensionHook::Receive && call.client == SERVER_OWNER)
        .collect();
    assert_eq!(server_receives.len(), 2);
    assert_receive_type(
        server_receives[0],
        SERVER_OWNER,
        ROOTLESS_SELECTION_WINDOW,
        31,
    );
    assert_receive_type(server_receives[1], SERVER_OWNER, ROOT_WINDOW, 33);
    let sends: Vec<_> = calls
        .iter()
        .filter(|call| call.hook == ExtensionHook::Send)
        .collect();
    assert_eq!(sends.len(), 2);
    assert_send_type(sends[0], ROOTLESS_SELECTION_WINDOW, 31);
    assert_send_type(sends[1], ROOT_WINDOW, 33);
}

fn no_policy_delivery(
    destination: u32,
    propagate: u8,
    mask: u32,
    selected: ResourceId,
    expected_recipient: Option<ClientId>,
    do_not_propagate_mask: u16,
) -> (Vec<u8>, Vec<u8>) {
    let mut fixture = Fixture::new();
    if mask != 0 {
        fixture.select(RECEIVER_A, selected, mask);
    }
    fixture
        .state
        .resources
        .window_mut(LEAF)
        .unwrap()
        .do_not_propagate_mask = do_not_propagate_mask;
    fixture.state.pointer_root = (5, 5);
    let sender_bytes = fixture.send_event(1, destination, mask, propagate, key_event());
    if expected_recipient != Some(SENDER) {
        assert!(
            sender_bytes.is_empty(),
            "unexpected requester bytes when delivery expected for {expected_recipient:?}: {sender_bytes:?}"
        );
    }
    let event = match expected_recipient {
        Some(SENDER) => sender_bytes,
        Some(client) => fixture.drain(client),
        None => fixture.drain_all().into_values().flatten().collect(),
    };
    let expected = expected_recipient.map_or_else(Vec::new, |client| {
        let sequence = u16::from(client == SENDER);
        sent_key_event(sequence).to_vec()
    });
    (event, expected)
}

// Mutations: clear SendEvent's sent-event bit before fanout; write an unexpected
// error reply to the requester when another client receives the event.
#[test]
fn no_policy_send_event_preserves_resolver_branch_bytes() {
    for (destination, propagate, mask, selected, recipient, dnp) in [
        (0, 0, EVENT_MASK, LEAF, Some(RECEIVER_A), 0),
        (0, 1, EVENT_MASK, LEAF, Some(RECEIVER_A), 0),
        (1, 0, EVENT_MASK, LEAF, Some(RECEIVER_A), 0),
        (1, 1, EVENT_MASK, LEAF, Some(RECEIVER_A), 0),
        (LEAF.0, 0, EVENT_MASK, LEAF, Some(RECEIVER_A), 0),
        (LEAF.0, 1, EVENT_MASK, MIDDLE, Some(RECEIVER_A), 0),
        (LEAF.0, 1, 0, LEAF, Some(SENDER), 0),
        (LEAF.0, 1, EVENT_MASK, PARENT, None, EVENT_MASK as u16),
    ] {
        let (actual, expected) =
            no_policy_delivery(destination, propagate, mask, selected, recipient, dnp);
        assert_eq!(
            actual, expected,
            "destination={destination}, propagate={propagate}, mask={mask}"
        );
    }

    let mut no_focus = Fixture::new();
    no_focus.state.core_focus.raw = 0;
    no_focus.select(RECEIVER_A, LEAF, EVENT_MASK);
    assert!(
        no_focus
            .send_event(1, 1, EVENT_MASK, 1, key_event())
            .is_empty()
    );
    assert!(no_focus.drain(RECEIVER_A).is_empty());

    let mut effective_focus = Fixture::new();
    effective_focus.state.pointer_root = (5, 5);
    effective_focus.state.core_focus.raw = MIDDLE.0;
    effective_focus.select(RECEIVER_A, MIDDLE, EVENT_MASK);
    effective_focus.send_event(1, 1, EVENT_MASK, 1, key_event());
    assert_eq!(effective_focus.drain(RECEIVER_A), sent_key_event(0));

    // Mutation: remove the effective-focus propagation stop.
    let mut above_effective_focus = Fixture::new();
    above_effective_focus.state.pointer_root = (5, 5);
    above_effective_focus.state.core_focus.raw = MIDDLE.0;
    above_effective_focus.select(RECEIVER_A, PARENT, EVENT_MASK);
    assert!(
        above_effective_focus
            .send_event(1, 1, EVENT_MASK, 1, key_event())
            .is_empty()
    );
    assert!(above_effective_focus.drain(RECEIVER_A).is_empty());
}

// Mutation: clear SendEvent's sent-event bit before fanout.
#[test]
fn no_policy_rootless_interceptions_keep_their_queue_and_event_bytes() {
    let mut fixture = Fixture::new();
    enable_rootless(&mut fixture);
    assert!(
        fixture
            .send_event(
                1,
                ROOTLESS_SELECTION_WINDOW.0,
                0,
                0,
                selection_notify_event()
            )
            .is_empty()
    );
    assert_eq!(
        rootless::selection::take_selection_events(&mut fixture.state),
        [SelectionEvent::Notify {
            selection: yserver_protocol::x11::AtomId(0x100),
            target: yserver_protocol::x11::AtomId(0x101),
            property: yserver_protocol::x11::AtomId(0x102),
        }]
    );

    fixture.select(RECEIVER_A, ROOT_WINDOW, SUBSTRUCTURE_NOTIFY_MASK);
    let request_extents = fixture
        .state
        .atoms
        .intern("_NET_REQUEST_FRAME_EXTENTS", false)
        .0;
    let event = root_client_message(request_extents, PARENT, [0; 5]);
    assert!(
        fixture
            .send_event(2, ROOT_WINDOW.0, SUBSTRUCTURE_NOTIFY_MASK, 0, event)
            .is_empty()
    );
    let mut expected = event;
    expected[0] |= 0x80;
    expected[2..4].copy_from_slice(&0u16.to_le_bytes());
    assert_eq!(fixture.drain(RECEIVER_A), expected);
    let frame_extents = fixture.state.atoms.intern("_NET_FRAME_EXTENTS", false);
    assert!(
        fixture
            .state
            .resources
            .window(PARENT)
            .unwrap()
            .properties
            .contains_key(&frame_extents)
    );
}

// Mutation: describe only the first event in the XI SendExtensionEvent batch.
#[test]
fn xi_send_ask_checks_every_candidate_with_device_and_full_batch() {
    let mut fixture = Fixture::new();
    let events = xi_event_pair();
    let class = xi_event_class(events[0][0]);
    fixture
        .state
        .clients
        .get_mut(&RECEIVER_A.0)
        .unwrap()
        .xi1_window_event_classes
        .entry(PARENT)
        .or_default()
        .insert(class);
    let calls = fixture.install_policy(|_, _| Decision::Allow);

    assert!(
        fixture
            .send_extension_event(1, LEAF.0, true, &events, class)
            .is_empty()
    );
    assert_eq!(fixture.drain(RECEIVER_A), sent_xi_events(&events, 0));

    let calls = calls.lock().unwrap();
    let event_calls: Vec<_> = calls
        .iter()
        .filter(|call| matches!(call.hook, ExtensionHook::Send | ExtensionHook::Receive))
        .collect();
    let descriptions = xi_event_descriptions(&events);
    assert_eq!(event_calls.len(), 4);
    assert_xi_send(event_calls[0], LEAF, &descriptions);
    assert_xi_send(event_calls[1], MIDDLE, &descriptions);
    assert_xi_send(event_calls[2], PARENT, &descriptions);
    assert_xi_receive(event_calls[3], RECEIVER_A, PARENT, &descriptions);
}

// Mutation: filter extension events by the registry's static first_event,
// which omits XKEYBOARD's live event base.
#[test]
fn xi_send_xkb_live_event_base_is_described_and_denied() {
    let mut fixture = Fixture::new();
    fixture.backend.xkb_info_result = Some((136, 85, 162));
    let event = xi_event(85); // XKB's live first event code from xkb_info().
    let class = xi_event_class(event[0]);
    fixture
        .state
        .clients
        .get_mut(&RECEIVER_A.0)
        .unwrap()
        .xi1_window_event_classes
        .entry(LEAF)
        .or_default()
        .insert(class);
    let xkb_description = EventDescription::extension(85, 136, 0);
    let calls = fixture.install_policy(move |_, call| {
        if call.hook == ExtensionHook::Receive
            && call.events.as_deref() == Some(std::slice::from_ref(&xkb_description))
        {
            Decision::Deny(DENY_ERROR)
        } else {
            Decision::Allow
        }
    });
    let backend_before = fixture.backend.calls();
    let state_before = xi_send_state_snapshot(&fixture.state);

    assert!(
        fixture
            .send_extension_event(1, LEAF.0, false, &[event], class)
            .is_empty()
    );
    let observed = fixture.drain_all();
    assert!(
        observed.values().all(Vec::is_empty),
        "denied XKB SendExtensionEvent wrote bytes: {observed:?}"
    );
    assert_eq!(fixture.backend.calls(), backend_before);
    assert_eq!(xi_send_state_snapshot(&fixture.state), state_before);

    let calls = calls.lock().unwrap();
    let event_calls: Vec<_> = calls
        .iter()
        .filter(|call| matches!(call.hook, ExtensionHook::Send | ExtensionHook::Receive))
        .collect();
    let descriptions = [EventDescription::extension(85, 136, 0)];
    assert_eq!(event_calls.len(), 2);
    assert_xi_send(event_calls[0], LEAF, &descriptions);
    assert_xi_receive(event_calls[1], RECEIVER_A, LEAF, &descriptions);
}

// Mutation: describe GenericEvent templates as core events without extension metadata.
#[test]
fn xi_generic_event_metadata_uses_sender_byte_order_and_denial() {
    let mut fixture = Fixture::new();
    fixture
        .state
        .clients
        .get_mut(&SENDER.0)
        .expect("sender")
        .byte_order = yserver_protocol::x11::ClientByteOrder::BigEndian;

    let mut generic_event = [0; 32];
    generic_event[0] = 35;
    generic_event[1] = 0xf2; // Unknown extension opcodes are described, not rejected.
    generic_event[8..10].copy_from_slice(&0x1234u16.to_be_bytes());
    let xi_event =
        xi_event(crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_KEY_PRESS_OFFSET);
    let events = [generic_event, xi_event];
    let class = xi_event_class(xi_event[0]);
    fixture
        .state
        .clients
        .get_mut(&RECEIVER_A.0)
        .expect("receiver")
        .xi1_window_event_classes
        .entry(LEAF)
        .or_default()
        .insert(class);

    let generic_description = EventDescription::extension(35, 0xf2, 0x1234);
    let expected_batch = [
        generic_description,
        EventDescription::extension(
            xi_event[0],
            XI_SEND.major,
            u16::from(xi_event[0] - crate::server::XI_FIRST_EVENT),
        ),
    ];
    let calls = fixture.install_policy(move |_, call| {
        if call.hook == ExtensionHook::Send
            && call.events.as_deref() == Some(expected_batch.as_slice())
        {
            Decision::Deny(DENY_ERROR)
        } else {
            Decision::Allow
        }
    });
    let backend_before = fixture.backend.calls();
    let state_before = xi_send_state_snapshot(&fixture.state);

    assert!(
        fixture
            .send_extension_event_with_byte_order(
                1,
                LEAF.0,
                false,
                &events,
                class,
                yserver_protocol::x11::ClientByteOrder::BigEndian,
            )
            .is_empty(),
        "unknown GenericEvent extension opcode must not add a validity error"
    );
    let observed = fixture.drain_all();
    assert!(
        observed.values().all(Vec::is_empty),
        "GenericEvent denial wrote bytes: {observed:?}"
    );
    assert_eq!(fixture.backend.calls(), backend_before);
    assert_eq!(xi_send_state_snapshot(&fixture.state), state_before);

    let calls = calls.lock().unwrap();
    let send_calls: Vec<_> = calls
        .iter()
        .filter(|call| call.hook == ExtensionHook::Send)
        .collect();
    assert_eq!(send_calls.len(), 1);
    assert_xi_send(send_calls[0], LEAF, &expected_batch);
}

// Mutation: skip SendExtensionEvent's send check at the destination.
#[test]
fn xi_send_denied_at_destination_leaves_all_clients_and_state_unchanged() {
    let mut fixture = Fixture::new();
    let events = xi_event_pair();
    let class = xi_event_class(events[0][0]);
    fixture
        .state
        .clients
        .get_mut(&RECEIVER_A.0)
        .unwrap()
        .xi1_window_event_classes
        .entry(LEAF)
        .or_default()
        .insert(class);
    fixture
        .state
        .clients
        .get_mut(&RECEIVER_B.0)
        .unwrap()
        .xi1_window_event_classes
        .entry(LEAF)
        .or_default()
        .insert(class);
    let calls = fixture.install_policy(|_, call| {
        if call.hook == ExtensionHook::Send {
            Decision::Deny(DENY_ERROR)
        } else {
            Decision::Allow
        }
    });
    let backend_before = fixture.backend.calls();
    let state_before = xi_send_state_snapshot(&fixture.state);

    assert!(
        fixture
            .send_extension_event(1, LEAF.0, false, &events, class)
            .is_empty()
    );
    let observed = fixture.drain_all();
    assert!(
        observed.values().all(Vec::is_empty),
        "denied XI send wrote bytes: {observed:?}"
    );
    assert_eq!(fixture.backend.calls(), backend_before);
    assert_eq!(xi_send_state_snapshot(&fixture.state), state_before);
    let calls = calls.lock().unwrap();
    let event_calls: Vec<_> = calls
        .iter()
        .filter(|call| matches!(call.hook, ExtensionHook::Send | ExtensionHook::Receive))
        .collect();
    assert_eq!(event_calls.len(), 1);
    assert_xi_send(event_calls[0], LEAF, &xi_event_descriptions(&events));
}

// Mutation: check XI send only at the original destination, not each ancestor.
#[test]
fn xi_send_denied_at_middle_candidate_stops_before_parent_delivery() {
    let mut fixture = Fixture::new();
    let events = xi_event_pair();
    let class = xi_event_class(events[0][0]);
    for (client, window) in [(RECEIVER_A, MIDDLE), (RECEIVER_B, PARENT)] {
        fixture
            .state
            .clients
            .get_mut(&client.0)
            .unwrap()
            .xi1_window_event_classes
            .entry(window)
            .or_default()
            .insert(class);
    }
    let calls = fixture.install_policy(|_, call| {
        if call.hook == ExtensionHook::Send
            && call.send == Some((MIDDLE, Some(u16::from(XI_DEVICE))))
        {
            Decision::Deny(DENY_ERROR)
        } else {
            Decision::Allow
        }
    });
    let backend_before = fixture.backend.calls();
    let state_before = xi_send_state_snapshot(&fixture.state);

    assert!(
        fixture
            .send_extension_event(1, LEAF.0, true, &events, class)
            .is_empty()
    );
    let observed = fixture.drain_all();
    assert!(
        observed.values().all(Vec::is_empty),
        "middle-candidate denial wrote bytes: {observed:?}"
    );
    assert_eq!(fixture.backend.calls(), backend_before);
    assert_eq!(xi_send_state_snapshot(&fixture.state), state_before);
    let calls = calls.lock().unwrap();
    let event_calls: Vec<_> = calls
        .iter()
        .filter(|call| matches!(call.hook, ExtensionHook::Send | ExtensionHook::Receive))
        .collect();
    let descriptions = xi_event_descriptions(&events);
    assert_eq!(event_calls.len(), 2);
    assert_xi_send(event_calls[0], LEAF, &descriptions);
    assert_xi_send(event_calls[1], MIDDLE, &descriptions);
}

// Mutation: stop the XI propagation walk when a window has selectors, even if all are denied.
#[test]
fn xi_receive_denied_for_all_selectors_continues_to_parent() {
    let mut fixture = Fixture::new();
    let events = xi_event_pair();
    let class = xi_event_class(events[0][0]);
    for (client, window) in [
        (RECEIVER_A, LEAF),
        (RECEIVER_B, MIDDLE),
        (RECEIVER_C, PARENT),
    ] {
        fixture
            .state
            .clients
            .get_mut(&client.0)
            .unwrap()
            .xi1_window_event_classes
            .entry(window)
            .or_default()
            .insert(class);
    }
    let calls = fixture.install_policy(|_, call| {
        if call.hook == ExtensionHook::Receive && call.client != RECEIVER_C {
            Decision::Deny(DENY_ERROR)
        } else {
            Decision::Allow
        }
    });
    let backend_before = fixture.backend.calls();
    let state_before = xi_send_state_snapshot(&fixture.state);

    assert!(
        fixture
            .send_extension_event(1, LEAF.0, true, &events, class)
            .is_empty()
    );
    assert!(fixture.drain(RECEIVER_A).is_empty());
    assert!(fixture.drain(RECEIVER_B).is_empty());
    assert_eq!(fixture.drain(RECEIVER_C), sent_xi_events(&events, 0));
    assert_eq!(fixture.backend.calls(), backend_before);
    assert_eq!(xi_send_state_snapshot(&fixture.state), state_before);
    let calls = calls.lock().unwrap();
    let event_calls: Vec<_> = calls
        .iter()
        .filter(|call| matches!(call.hook, ExtensionHook::Send | ExtensionHook::Receive))
        .collect();
    let descriptions = xi_event_descriptions(&events);
    assert_eq!(event_calls.len(), 6);
    assert_xi_send(event_calls[0], LEAF, &descriptions);
    assert_xi_receive(event_calls[1], RECEIVER_A, LEAF, &descriptions);
    assert_xi_send(event_calls[2], MIDDLE, &descriptions);
    assert_xi_receive(event_calls[3], RECEIVER_B, MIDDLE, &descriptions);
    assert_xi_send(event_calls[4], PARENT, &descriptions);
    assert_xi_receive(event_calls[5], RECEIVER_C, PARENT, &descriptions);
}

// Mutation: continue XI propagation after the zero-mask window creator refuses delivery.
#[test]
fn xi_receive_denied_creator_under_zero_mask_stops() {
    let mut fixture = Fixture::new();
    let events = xi_event_pair();
    let no_event_class = (u32::from(XI_DEVICE) << 8) | 9;
    let calls = fixture.install_policy(|_, call| {
        if call.hook == ExtensionHook::Receive && call.client == SENDER {
            Decision::Deny(DENY_ERROR)
        } else {
            Decision::Allow
        }
    });
    let backend_before = fixture.backend.calls();
    let state_before = xi_send_state_snapshot(&fixture.state);

    assert!(
        fixture
            .send_extension_event(1, LEAF.0, true, &events, no_event_class)
            .is_empty()
    );
    assert!(fixture.drain_all().values().all(Vec::is_empty));
    assert_eq!(fixture.backend.calls(), backend_before);
    assert_eq!(xi_send_state_snapshot(&fixture.state), state_before);
    let calls = calls.lock().unwrap();
    let event_calls: Vec<_> = calls
        .iter()
        .filter(|call| matches!(call.hook, ExtensionHook::Send | ExtensionHook::Receive))
        .collect();
    let descriptions = xi_event_descriptions(&events);
    assert_eq!(event_calls.len(), 2);
    assert_xi_send(event_calls[0], LEAF, &descriptions);
    assert_xi_receive(event_calls[1], SENDER, LEAF, &descriptions);
}

// Mutation: stop checking XI recipients after the first selected recipient is denied.
#[test]
fn xi_receive_denied_one_of_two_selectors_keeps_the_other_delivery() {
    let mut fixture = Fixture::new();
    let events = xi_event_pair();
    let class = xi_event_class(events[0][0]);
    for client in [RECEIVER_A, RECEIVER_B] {
        fixture
            .state
            .clients
            .get_mut(&client.0)
            .unwrap()
            .xi1_window_event_classes
            .entry(LEAF)
            .or_default()
            .insert(class);
    }
    let calls = fixture.install_policy(|_, call| {
        if call.hook == ExtensionHook::Receive && call.client == RECEIVER_A {
            Decision::Deny(DENY_ERROR)
        } else {
            Decision::Allow
        }
    });
    let backend_before = fixture.backend.calls();
    let state_before = xi_send_state_snapshot(&fixture.state);

    assert!(
        fixture
            .send_extension_event(1, LEAF.0, true, &events, class)
            .is_empty()
    );
    assert!(fixture.drain(RECEIVER_A).is_empty());
    assert_eq!(fixture.drain(RECEIVER_B), sent_xi_events(&events, 0));
    assert_eq!(fixture.backend.calls(), backend_before);
    assert_eq!(xi_send_state_snapshot(&fixture.state), state_before);
    let calls = calls.lock().unwrap();
    let event_calls: Vec<_> = calls
        .iter()
        .filter(|call| matches!(call.hook, ExtensionHook::Send | ExtensionHook::Receive))
        .collect();
    let descriptions = xi_event_descriptions(&events);
    assert_eq!(event_calls.len(), 3);
    assert_xi_send(event_calls[0], LEAF, &descriptions);
    assert_xi_receive(event_calls[1], RECEIVER_A, LEAF, &descriptions);
    assert_xi_receive(event_calls[2], RECEIVER_B, LEAF, &descriptions);
}

// Mutation: cache XI SendExtensionEvent's first send or receive decision across requests.
#[test]
fn xi_send_and_receive_revocation_is_checked_on_each_request() {
    let mut fixture = Fixture::new();
    let events = xi_event_pair();
    let class = xi_event_class(events[0][0]);
    fixture
        .state
        .clients
        .get_mut(&RECEIVER_A.0)
        .unwrap()
        .xi1_window_event_classes
        .entry(LEAF)
        .or_default()
        .insert(class);
    let revoke_send = Arc::new(AtomicBool::new(false));
    let revoke_receive = Arc::new(AtomicBool::new(false));
    let send_flag = Arc::clone(&revoke_send);
    let receive_flag = Arc::clone(&revoke_receive);
    let calls = fixture.install_policy(move |_, call| {
        if call.hook == ExtensionHook::Send && send_flag.load(Ordering::SeqCst)
            || call.hook == ExtensionHook::Receive
                && call.client == RECEIVER_A
                && receive_flag.load(Ordering::SeqCst)
        {
            Decision::Deny(DENY_ERROR)
        } else {
            Decision::Allow
        }
    });

    assert!(
        fixture
            .send_extension_event(1, LEAF.0, false, &events, class)
            .is_empty()
    );
    assert_eq!(fixture.drain(RECEIVER_A), sent_xi_events(&events, 0));
    revoke_send.store(true, Ordering::SeqCst);
    let state_before_send_deny = xi_send_state_snapshot(&fixture.state);
    assert!(
        fixture
            .send_extension_event(2, LEAF.0, false, &events, class)
            .is_empty()
    );
    assert!(fixture.drain(RECEIVER_A).is_empty());
    assert_eq!(
        xi_send_state_snapshot(&fixture.state),
        state_before_send_deny
    );
    revoke_send.store(false, Ordering::SeqCst);
    revoke_receive.store(true, Ordering::SeqCst);
    let state_before_receive_deny = xi_send_state_snapshot(&fixture.state);
    assert!(
        fixture
            .send_extension_event(3, LEAF.0, false, &events, class)
            .is_empty()
    );
    assert!(fixture.drain(RECEIVER_A).is_empty());
    assert_eq!(
        xi_send_state_snapshot(&fixture.state),
        state_before_receive_deny
    );

    let calls = calls.lock().unwrap();
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.hook == ExtensionHook::Send)
            .count(),
        3
    );
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.hook == ExtensionHook::Receive)
            .count(),
        2
    );
}

// Mutation: ignore XI class do-not-propagate masks when building the no-policy walk.
#[test]
fn no_policy_xi_class_do_not_propagate_walk_is_unchanged() {
    let mut fixture = Fixture::new();
    let events = xi_event_pair();
    let class = xi_event_class(events[0][0]);
    fixture
        .state
        .clients
        .get_mut(&RECEIVER_A.0)
        .unwrap()
        .xi1_window_event_classes
        .entry(PARENT)
        .or_default()
        .insert(class);
    fixture
        .state
        .xi1_window_dont_propagate
        .entry(LEAF)
        .or_default()
        .insert(class);

    assert!(
        fixture
            .send_extension_event(1, LEAF.0, true, &events, class)
            .is_empty()
    );
    assert!(fixture.drain(RECEIVER_A).is_empty());

    fixture.state.xi1_window_dont_propagate.remove(&LEAF);
    assert!(
        fixture
            .send_extension_event(2, LEAF.0, true, &events, class)
            .is_empty()
    );
    assert_eq!(fixture.drain(RECEIVER_A), sent_xi_events(&events, 0));
}
