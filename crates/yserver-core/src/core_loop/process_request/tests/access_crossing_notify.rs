use super::{
    access_notify_support::{
        NOTIFY_RECEIVER_ALLOWED, NOTIFY_RECEIVER_DENIED, NOTIFY_SENDER, NotifyFixture,
        append_event_u16, append_event_u32, finish_literal_event, literal_event_header,
        request_opcodes,
    },
    extensions::{ExtensionHook, RecordedExtensionCheck},
    *,
};
use crate::{
    access::{Decision, DenyError, EventDescription, EventSubject},
    backend::WindowHandle,
    resources::{COMPOSITE_OVERLAY_WINDOW, ROOT_WINDOW},
    server::PresentEventSelection,
};
use std::sync::atomic::Ordering;
use yserver_protocol::x11::{ClientByteOrder, ResourceId, SequenceNumber};

const STRUCTURE_NOTIFY: u32 = 0x0002_0000;
const SUBSTRUCTURE_NOTIFY: u32 = 0x0008_0000;
const SCREEN_WIDTH: u16 = 1280;
const SCREEN_HEIGHT: u16 = 960;
const SCREEN_WIDTH_MM: u32 = 340;
const SCREEN_HEIGHT_MM: u32 = 254;
const PRESENT_EVENT_ID: u32 = 0x0003_0042;
const SENDER_SEQUENCE: u16 = 0x1111;
const DUAL_SEQUENCE: u16 = 0x2222;
const ROOT_SEQUENCE: u16 = 0x3333;
const DENY: DenyError = DenyError {
    code: x11::error::BAD_ACCESS,
    value: 0,
};

fn receive_calls(calls: &[RecordedExtensionCheck]) -> Vec<&RecordedExtensionCheck> {
    calls
        .iter()
        .filter(|call| call.hook == ExtensionHook::Receive)
        .collect()
}

fn literal_configure_notify(
    order: ClientByteOrder,
    sequence: u16,
    event_window: ResourceId,
    window: ResourceId,
    width: u16,
    height: u16,
) -> Vec<u8> {
    let mut event = literal_event_header(22, 0, sequence, order);
    append_event_u32(&mut event, order, event_window.0);
    append_event_u32(&mut event, order, window.0);
    append_event_u32(&mut event, order, 0); // no above sibling
    append_event_u16(&mut event, order, 0); // x
    append_event_u16(&mut event, order, 0); // y
    append_event_u16(&mut event, order, width);
    append_event_u16(&mut event, order, height);
    append_event_u16(&mut event, order, 0); // border width
    event.extend_from_slice(&[
        u8::from(window == COMPOSITE_OVERLAY_WINDOW), // COW is override_redirect
        0,
        0,
        0,
        0,
        0,
    ]);
    finish_literal_event(event)
}

fn literal_present_configure_notify(
    order: ClientByteOrder,
    sequence: u16,
    window: ResourceId,
) -> Vec<u8> {
    let mut event = vec![35, 145]; // GenericEvent, PRESENT major opcode
    append_event_u16(&mut event, order, sequence);
    append_event_u32(&mut event, order, 2); // two extra four-byte words
    append_event_u16(&mut event, order, 0); // ConfigureNotify
    append_event_u16(&mut event, order, 0); // pad
    append_event_u32(&mut event, order, PRESENT_EVENT_ID);
    append_event_u32(&mut event, order, window.0);
    append_event_u16(&mut event, order, 0); // x
    append_event_u16(&mut event, order, 0); // y
    append_event_u16(&mut event, order, SCREEN_WIDTH);
    append_event_u16(&mut event, order, SCREEN_HEIGHT);
    append_event_u16(&mut event, order, 0); // offset x
    append_event_u16(&mut event, order, 0); // offset y
    append_event_u16(&mut event, order, SCREEN_WIDTH);
    append_event_u16(&mut event, order, SCREEN_HEIGHT);
    append_event_u32(&mut event, order, 0); // pixmap flags
    assert_eq!(event.len(), 40);
    event
}

fn literal_screen_change_notify(
    order: ClientByteOrder,
    sequence: u16,
    request_window: ResourceId,
) -> Vec<u8> {
    let mut event = vec![89, 1]; // RandR ScreenChangeNotify, Rotate_0
    append_event_u16(&mut event, order, sequence);
    append_event_u32(&mut event, order, 1); // timestamp
    append_event_u32(&mut event, order, 1); // config timestamp
    append_event_u32(&mut event, order, ROOT_WINDOW.0);
    append_event_u32(&mut event, order, request_window.0);
    append_event_u16(&mut event, order, u16::MAX); // no matching RandR 1.0 size
    append_event_u16(&mut event, order, 0); // unknown subpixel order
    append_event_u16(&mut event, order, SCREEN_WIDTH);
    append_event_u16(&mut event, order, SCREEN_HEIGHT);
    append_event_u16(&mut event, order, SCREEN_WIDTH_MM as u16);
    append_event_u16(&mut event, order, SCREEN_HEIGHT_MM as u16);
    assert_eq!(event.len(), 32);
    event
}

fn resize_fixture() -> NotifyFixture {
    let mut fixture = NotifyFixture::new();
    fixture.state.start_instant = std::time::Instant::now() + std::time::Duration::from_secs(30);
    for (client, sequence, order) in [
        (
            NOTIFY_SENDER,
            SENDER_SEQUENCE,
            ClientByteOrder::LittleEndian,
        ),
        (
            NOTIFY_RECEIVER_DENIED,
            DUAL_SEQUENCE,
            ClientByteOrder::BigEndian,
        ),
        (
            NOTIFY_RECEIVER_ALLOWED,
            ROOT_SEQUENCE,
            ClientByteOrder::LittleEndian,
        ),
    ] {
        let state = fixture
            .state
            .clients
            .get_mut(&client.0)
            .expect("fixture client");
        state.byte_order = order;
        state.last_sequence.store(sequence, Ordering::Relaxed);
    }
    fixture
        .state
        .resources
        .materialize_cow_resource(WindowHandle::from_raw_for_test(COMPOSITE_OVERLAY_WINDOW.0));
    fixture
        .state
        .clients
        .get_mut(&NOTIFY_RECEIVER_ALLOWED.0)
        .unwrap()
        .event_masks
        .insert(ROOT_WINDOW, STRUCTURE_NOTIFY);
    let dual = fixture
        .state
        .clients
        .get_mut(&NOTIFY_RECEIVER_DENIED.0)
        .unwrap();
    dual.event_masks
        .insert(COMPOSITE_OVERLAY_WINDOW, STRUCTURE_NOTIFY);
    dual.event_masks.insert(ROOT_WINDOW, SUBSTRUCTURE_NOTIFY);
    fixture.state.randr_select_masks.insert(
        (NOTIFY_RECEIVER_DENIED.0, COMPOSITE_OVERLAY_WINDOW),
        yserver_protocol::x11::randr::NOTIFY_MASK_SCREEN_CHANGE,
    );
    fixture.state.present_event_selections.insert(
        PRESENT_EVENT_ID,
        PresentEventSelection {
            owner: NOTIFY_RECEIVER_DENIED,
            window: COMPOSITE_OVERLAY_WINDOW,
            event_mask: yserver_protocol::x11::present::EVENT_MASK_CONFIGURE_NOTIFY,
        },
    );
    fixture
}

fn resize(fixture: &mut NotifyFixture) -> RequestOutcome {
    let mut body = Vec::with_capacity(16);
    body.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    body.extend_from_slice(&SCREEN_WIDTH.to_le_bytes());
    body.extend_from_slice(&SCREEN_HEIGHT.to_le_bytes());
    body.extend_from_slice(&SCREEN_WIDTH_MM.to_le_bytes());
    body.extend_from_slice(&SCREEN_HEIGHT_MM.to_le_bytes());
    fixture
        .request(
            NOTIFY_SENDER,
            SequenceNumber(41),
            RequestHeader {
                opcode: 128,
                data: yserver_protocol::x11::randr::RR_SET_SCREEN_SIZE,
                length_units: 5,
            },
            &body,
        )
        .expect("RRSetScreenSize through process_request")
}

fn assert_resized(fixture: &NotifyFixture) {
    assert_eq!(
        (
            fixture.state.randr.screen_width,
            fixture.state.randr.screen_height
        ),
        (SCREEN_WIDTH, SCREEN_HEIGHT),
        "RandR screen size changed"
    );
    let overlay = fixture
        .state
        .resources
        .window(COMPOSITE_OVERLAY_WINDOW)
        .expect("materialized overlay remains present");
    assert_eq!(
        (overlay.width, overlay.height),
        (SCREEN_WIDTH, SCREEN_HEIGHT)
    );
    assert!(crate::core_loop::client_io::failed_writers(&fixture.state.clients).is_empty());
}

fn assert_resize_receive_calls(calls: &[RecordedExtensionCheck]) {
    let rows = receive_calls(calls);
    assert_eq!(rows.len(), 3, "root, overlay and parent copies are checked");
    let dual: Vec<_> = rows
        .iter()
        .filter(|call| call.client == NOTIFY_RECEIVER_DENIED)
        .copied()
        .collect();
    assert_eq!(
        dual.len(),
        2,
        "dual selector gets overlay then parent checks"
    );
    assert_eq!(
        dual.iter()
            .map(|call| (call.receive, call.events.clone(), call.request))
            .collect::<Vec<_>>(),
        [
            (
                Some(EventSubject::Window(COMPOSITE_OVERLAY_WINDOW)),
                Some(vec![EventDescription::core(22)]),
                request_opcodes(128, yserver_protocol::x11::randr::RR_SET_SCREEN_SIZE),
            ),
            (
                Some(EventSubject::Window(ROOT_WINDOW)),
                Some(vec![EventDescription::core(22)]),
                request_opcodes(128, yserver_protocol::x11::randr::RR_SET_SCREEN_SIZE),
            ),
        ],
        "the dual subscriber's call order and arguments"
    );
    let root: Vec<_> = rows
        .iter()
        .filter(|call| call.client == NOTIFY_RECEIVER_ALLOWED)
        .copied()
        .collect();
    assert_eq!(root.len(), 1, "root selector gets one root check");
    assert_eq!(root[0].receive, Some(EventSubject::Window(ROOT_WINDOW)));
    assert_eq!(root[0].events, Some(vec![EventDescription::core(22)]));
    assert_eq!(
        root[0].request,
        request_opcodes(128, yserver_protocol::x11::randr::RR_SET_SCREEN_SIZE)
    );
    assert!(calls.iter().all(|call| call.hook != ExtensionHook::Send));
}

fn monitor_request(fixture: &mut NotifyFixture) -> RequestOutcome {
    let name = fixture.state.atoms.intern("PR4C_CONFIGURE_NOTIFY", false).0;
    let mut body = Vec::with_capacity(28);
    body.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    body.extend_from_slice(&name.to_le_bytes());
    body.extend_from_slice(&[0, 0]); // primary=false, automatic=false
    body.extend_from_slice(&0_u16.to_le_bytes()); // no output ids
    body.extend_from_slice(&0_i16.to_le_bytes());
    body.extend_from_slice(&0_i16.to_le_bytes());
    body.extend_from_slice(&100_u16.to_le_bytes());
    body.extend_from_slice(&100_u16.to_le_bytes());
    body.extend_from_slice(&100_u32.to_le_bytes());
    body.extend_from_slice(&50_u32.to_le_bytes());
    fixture
        .request(
            NOTIFY_SENDER,
            SequenceNumber(52),
            RequestHeader {
                opcode: 128,
                data: yserver_protocol::x11::randr::RR_SET_MONITOR,
                length_units: 8,
            },
            &body,
        )
        .expect("RRSetMonitor through process_request")
}

fn monitor_fixture() -> NotifyFixture {
    let mut fixture = NotifyFixture::new();
    fixture.state.start_instant = std::time::Instant::now() + std::time::Duration::from_secs(30);
    for (client, sequence, order) in [
        (
            NOTIFY_SENDER,
            SENDER_SEQUENCE,
            ClientByteOrder::LittleEndian,
        ),
        (
            NOTIFY_RECEIVER_DENIED,
            DUAL_SEQUENCE,
            ClientByteOrder::BigEndian,
        ),
        (
            NOTIFY_RECEIVER_ALLOWED,
            ROOT_SEQUENCE,
            ClientByteOrder::LittleEndian,
        ),
    ] {
        let state = fixture
            .state
            .clients
            .get_mut(&client.0)
            .expect("fixture client");
        state.byte_order = order;
        state.last_sequence.store(sequence, Ordering::Relaxed);
        if client != NOTIFY_SENDER {
            state.event_masks.insert(ROOT_WINDOW, STRUCTURE_NOTIFY);
        }
    }
    fixture
}

fn assert_root_notify_receive_calls(calls: &[RecordedExtensionCheck], request_minor: u8) {
    let rows = receive_calls(calls);
    assert_eq!(rows.len(), 2, "both selected root copies are checked");
    assert!(rows.iter().all(|call| {
        call.receive == Some(EventSubject::Window(ROOT_WINDOW))
            && call.events == Some(vec![EventDescription::core(22)])
            && call.request == request_opcodes(128, request_minor)
    }));
    let mut clients: Vec<_> = rows.iter().map(|call| call.client).collect();
    clients.sort_by_key(|client| client.0);
    assert_eq!(clients, [NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED]);
    assert!(calls.iter().all(|call| call.hook != ExtensionHook::Send));
}

// mutation: swap R3 and R4
#[test]
fn receive_screen_config_baseline_frames() {
    let mut fixture = resize_fixture();
    assert!(!fixture.state.policies_installed());
    assert!(matches!(resize(&mut fixture), RequestOutcome::Handled));
    assert_resized(&fixture);
    assert!(
        fixture.drain(NOTIFY_SENDER).is_empty(),
        "void request has no reply or error"
    );
    assert_eq!(
        fixture.drain(NOTIFY_RECEIVER_ALLOWED),
        literal_configure_notify(
            ClientByteOrder::LittleEndian,
            ROOT_SEQUENCE,
            ROOT_WINDOW,
            ROOT_WINDOW,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
        ),
        "root StructureNotify stream"
    );
    let expected_dual = [
        literal_configure_notify(
            ClientByteOrder::BigEndian,
            DUAL_SEQUENCE,
            COMPOSITE_OVERLAY_WINDOW,
            COMPOSITE_OVERLAY_WINDOW,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
        )
        .as_slice(),
        literal_configure_notify(
            ClientByteOrder::BigEndian,
            DUAL_SEQUENCE,
            ROOT_WINDOW,
            COMPOSITE_OVERLAY_WINDOW,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
        )
        .as_slice(),
        literal_present_configure_notify(
            ClientByteOrder::BigEndian,
            DUAL_SEQUENCE,
            COMPOSITE_OVERLAY_WINDOW,
        )
        .as_slice(),
        literal_screen_change_notify(
            ClientByteOrder::BigEndian,
            DUAL_SEQUENCE,
            COMPOSITE_OVERLAY_WINDOW,
        )
        .as_slice(),
    ]
    .concat();
    assert_eq!(
        fixture.drain(NOTIFY_RECEIVER_DENIED),
        expected_dual,
        "dual core and extension notification stream"
    );

    let mut monitor = monitor_fixture();
    assert!(!monitor.state.policies_installed());
    assert!(matches!(
        monitor_request(&mut monitor),
        RequestOutcome::Handled
    ));
    let requested_name = monitor.state.atoms.intern("PR4C_CONFIGURE_NOTIFY", false).0;
    assert_eq!(
        monitor.state.randr_client_monitors.len(),
        1,
        "SetMonitor took effect"
    );
    assert_eq!(
        monitor.state.randr_client_monitors[0].name, requested_name,
        "SetMonitor stores the requested monitor name"
    );
    assert!(crate::core_loop::client_io::failed_writers(&monitor.state.clients).is_empty());
    assert!(
        monitor.drain(NOTIFY_SENDER).is_empty(),
        "void request has no reply"
    );
    assert_eq!(
        monitor.drain(NOTIFY_RECEIVER_DENIED),
        literal_configure_notify(
            ClientByteOrder::BigEndian,
            DUAL_SEQUENCE,
            ROOT_WINDOW,
            ROOT_WINDOW,
            800,
            600,
        ),
        "big-endian root ConfigureNotify from RRSetMonitor"
    );
    assert_eq!(
        monitor.drain(NOTIFY_RECEIVER_ALLOWED),
        literal_configure_notify(
            ClientByteOrder::LittleEndian,
            ROOT_SEQUENCE,
            ROOT_WINDOW,
            ROOT_WINDOW,
            800,
            600,
        ),
        "little-endian root ConfigureNotify from RRSetMonitor"
    );
}

// mutation: pass the overlay as R4's window
#[test]
fn receive_cow_resize_subjects_independent() {
    let mut allowed = resize_fixture();
    let allowed_calls = allowed.install_policy(|_, _| Decision::Allow);
    assert!(matches!(resize(&mut allowed), RequestOutcome::Handled));
    assert_resized(&allowed);
    assert!(
        allowed.drain(NOTIFY_SENDER).is_empty(),
        "void request has no reply or error"
    );
    assert_eq!(
        allowed.drain(NOTIFY_RECEIVER_DENIED),
        [
            literal_configure_notify(
                ClientByteOrder::BigEndian,
                DUAL_SEQUENCE,
                COMPOSITE_OVERLAY_WINDOW,
                COMPOSITE_OVERLAY_WINDOW,
                SCREEN_WIDTH,
                SCREEN_HEIGHT,
            )
            .as_slice(),
            literal_configure_notify(
                ClientByteOrder::BigEndian,
                DUAL_SEQUENCE,
                ROOT_WINDOW,
                COMPOSITE_OVERLAY_WINDOW,
                SCREEN_WIDTH,
                SCREEN_HEIGHT,
            )
            .as_slice(),
            literal_present_configure_notify(
                ClientByteOrder::BigEndian,
                DUAL_SEQUENCE,
                COMPOSITE_OVERLAY_WINDOW,
            )
            .as_slice(),
            literal_screen_change_notify(
                ClientByteOrder::BigEndian,
                DUAL_SEQUENCE,
                COMPOSITE_OVERLAY_WINDOW,
            )
            .as_slice(),
        ]
        .concat(),
        "allow control has both COW copies"
    );
    assert_eq!(
        allowed.drain(NOTIFY_RECEIVER_ALLOWED),
        literal_configure_notify(
            ClientByteOrder::LittleEndian,
            ROOT_SEQUENCE,
            ROOT_WINDOW,
            ROOT_WINDOW,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
        ),
        "unaffected root selector keeps its literal stream"
    );

    let mut deny_parent = resize_fixture();
    let parent_calls = deny_parent.install_policy(|_, call| {
        if call.hook == ExtensionHook::Receive
            && call.client == NOTIFY_RECEIVER_DENIED
            && call.receive == Some(EventSubject::Window(ROOT_WINDOW))
            && call.events == Some(vec![EventDescription::core(22)])
        {
            Decision::Deny(DENY)
        } else {
            Decision::Allow
        }
    });
    assert!(matches!(resize(&mut deny_parent), RequestOutcome::Handled));
    assert_resized(&deny_parent);
    assert!(
        deny_parent.drain(NOTIFY_SENDER).is_empty(),
        "void request has no reply or error"
    );
    assert_eq!(
        deny_parent.drain(NOTIFY_RECEIVER_DENIED),
        [
            literal_configure_notify(
                ClientByteOrder::BigEndian,
                DUAL_SEQUENCE,
                COMPOSITE_OVERLAY_WINDOW,
                COMPOSITE_OVERLAY_WINDOW,
                SCREEN_WIDTH,
                SCREEN_HEIGHT,
            )
            .as_slice(),
            literal_present_configure_notify(
                ClientByteOrder::BigEndian,
                DUAL_SEQUENCE,
                COMPOSITE_OVERLAY_WINDOW,
            )
            .as_slice(),
            literal_screen_change_notify(
                ClientByteOrder::BigEndian,
                DUAL_SEQUENCE,
                COMPOSITE_OVERLAY_WINDOW,
            )
            .as_slice(),
        ]
        .concat(),
        "denied parent copy leaves overlay and extension copies"
    );
    assert_eq!(
        deny_parent.drain(NOTIFY_RECEIVER_ALLOWED),
        literal_configure_notify(
            ClientByteOrder::LittleEndian,
            ROOT_SEQUENCE,
            ROOT_WINDOW,
            ROOT_WINDOW,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
        ),
        "unaffected root selector keeps its literal stream"
    );

    let mut deny_overlay = resize_fixture();
    let overlay_calls = deny_overlay.install_policy(|_, call| {
        if call.hook == ExtensionHook::Receive
            && call.client == NOTIFY_RECEIVER_DENIED
            && call.receive == Some(EventSubject::Window(COMPOSITE_OVERLAY_WINDOW))
            && call.events == Some(vec![EventDescription::core(22)])
        {
            Decision::Deny(DENY)
        } else {
            Decision::Allow
        }
    });
    assert!(matches!(resize(&mut deny_overlay), RequestOutcome::Handled));
    assert_resized(&deny_overlay);
    assert!(
        deny_overlay.drain(NOTIFY_SENDER).is_empty(),
        "void request has no reply or error"
    );
    assert_eq!(
        deny_overlay.drain(NOTIFY_RECEIVER_DENIED),
        [
            literal_configure_notify(
                ClientByteOrder::BigEndian,
                DUAL_SEQUENCE,
                ROOT_WINDOW,
                COMPOSITE_OVERLAY_WINDOW,
                SCREEN_WIDTH,
                SCREEN_HEIGHT,
            )
            .as_slice(),
            literal_present_configure_notify(
                ClientByteOrder::BigEndian,
                DUAL_SEQUENCE,
                COMPOSITE_OVERLAY_WINDOW,
            )
            .as_slice(),
            literal_screen_change_notify(
                ClientByteOrder::BigEndian,
                DUAL_SEQUENCE,
                COMPOSITE_OVERLAY_WINDOW,
            )
            .as_slice(),
        ]
        .concat(),
        "denied overlay copy leaves parent and extension copies"
    );
    assert_eq!(
        deny_overlay.drain(NOTIFY_RECEIVER_ALLOWED),
        literal_configure_notify(
            ClientByteOrder::LittleEndian,
            ROOT_SEQUENCE,
            ROOT_WINDOW,
            ROOT_WINDOW,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
        ),
        "unaffected root selector keeps its literal stream"
    );
    assert_resize_receive_calls(&allowed_calls.lock().unwrap());
    assert_resize_receive_calls(&parent_calls.lock().unwrap());
    assert_resize_receive_calls(&overlay_calls.lock().unwrap());
}

// mutation: leave R2 unchecked
#[test]
fn receive_screen_resize_root_denial() {
    let mut allowed = resize_fixture();
    let allowed_calls = allowed.install_policy(|_, _| Decision::Allow);
    assert!(matches!(resize(&mut allowed), RequestOutcome::Handled));
    assert_resized(&allowed);
    assert!(
        allowed.drain(NOTIFY_SENDER).is_empty(),
        "void request has no reply or error"
    );
    assert_eq!(
        allowed.drain(NOTIFY_RECEIVER_ALLOWED),
        literal_configure_notify(
            ClientByteOrder::LittleEndian,
            ROOT_SEQUENCE,
            ROOT_WINDOW,
            ROOT_WINDOW,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
        ),
        "allow control root stream"
    );
    assert_eq!(
        allowed.drain(NOTIFY_RECEIVER_DENIED),
        [
            literal_configure_notify(
                ClientByteOrder::BigEndian,
                DUAL_SEQUENCE,
                COMPOSITE_OVERLAY_WINDOW,
                COMPOSITE_OVERLAY_WINDOW,
                SCREEN_WIDTH,
                SCREEN_HEIGHT,
            )
            .as_slice(),
            literal_configure_notify(
                ClientByteOrder::BigEndian,
                DUAL_SEQUENCE,
                ROOT_WINDOW,
                COMPOSITE_OVERLAY_WINDOW,
                SCREEN_WIDTH,
                SCREEN_HEIGHT,
            )
            .as_slice(),
            literal_present_configure_notify(
                ClientByteOrder::BigEndian,
                DUAL_SEQUENCE,
                COMPOSITE_OVERLAY_WINDOW,
            )
            .as_slice(),
            literal_screen_change_notify(
                ClientByteOrder::BigEndian,
                DUAL_SEQUENCE,
                COMPOSITE_OVERLAY_WINDOW,
            )
            .as_slice(),
        ]
        .concat(),
        "allow control dual core and extension streams"
    );

    let mut denied = resize_fixture();
    let calls = denied.install_policy(|_, call| {
        if call.hook == ExtensionHook::Receive
            && call.client == NOTIFY_RECEIVER_ALLOWED
            && call.receive == Some(EventSubject::Window(ROOT_WINDOW))
            && call.events == Some(vec![EventDescription::core(22)])
        {
            Decision::Deny(DENY)
        } else {
            Decision::Allow
        }
    });
    assert!(matches!(resize(&mut denied), RequestOutcome::Handled));
    assert_resized(&denied);
    assert!(
        denied.drain(NOTIFY_SENDER).is_empty(),
        "void request has no reply or error"
    );
    assert!(
        denied.drain(NOTIFY_RECEIVER_ALLOWED).is_empty(),
        "denied root StructureNotify copy is absent"
    );
    assert_eq!(
        denied.drain(NOTIFY_RECEIVER_DENIED),
        [
            literal_configure_notify(
                ClientByteOrder::BigEndian,
                DUAL_SEQUENCE,
                COMPOSITE_OVERLAY_WINDOW,
                COMPOSITE_OVERLAY_WINDOW,
                SCREEN_WIDTH,
                SCREEN_HEIGHT,
            )
            .as_slice(),
            literal_configure_notify(
                ClientByteOrder::BigEndian,
                DUAL_SEQUENCE,
                ROOT_WINDOW,
                COMPOSITE_OVERLAY_WINDOW,
                SCREEN_WIDTH,
                SCREEN_HEIGHT,
            )
            .as_slice(),
            literal_present_configure_notify(
                ClientByteOrder::BigEndian,
                DUAL_SEQUENCE,
                COMPOSITE_OVERLAY_WINDOW,
            )
            .as_slice(),
            literal_screen_change_notify(
                ClientByteOrder::BigEndian,
                DUAL_SEQUENCE,
                COMPOSITE_OVERLAY_WINDOW,
            )
            .as_slice(),
        ]
        .concat(),
        "COW and extension recipients remain unaffected"
    );
    assert_resize_receive_calls(&allowed_calls.lock().unwrap());
    assert_resize_receive_calls(&calls.lock().unwrap());
}

// mutation: leave R1 unchecked
#[test]
fn receive_root_config_notify_denial() {
    let mut allowed = monitor_fixture();
    let allow_calls = allowed.install_policy(|_, _| Decision::Allow);
    let allowed_name = allowed.state.atoms.intern("PR4C_CONFIGURE_NOTIFY", false).0;
    assert!(matches!(
        monitor_request(&mut allowed),
        RequestOutcome::Handled
    ));
    assert_eq!(
        allowed.state.randr_client_monitors.len(),
        1,
        "SetMonitor took effect"
    );
    assert_eq!(
        allowed.state.randr_client_monitors[0].name, allowed_name,
        "SetMonitor stores the requested monitor name"
    );
    assert!(crate::core_loop::client_io::failed_writers(&allowed.state.clients).is_empty());
    assert!(
        allowed.drain(NOTIFY_SENDER).is_empty(),
        "void request has no reply"
    );
    let allowed_frame = literal_configure_notify(
        ClientByteOrder::LittleEndian,
        ROOT_SEQUENCE,
        ROOT_WINDOW,
        ROOT_WINDOW,
        800,
        600,
    );
    assert_eq!(
        allowed.drain(NOTIFY_RECEIVER_ALLOWED),
        allowed_frame,
        "allow control selected root stream"
    );
    assert_eq!(
        allowed.drain(NOTIFY_RECEIVER_DENIED),
        literal_configure_notify(
            ClientByteOrder::BigEndian,
            DUAL_SEQUENCE,
            ROOT_WINDOW,
            ROOT_WINDOW,
            800,
            600,
        ),
        "allow control big-endian root stream"
    );
    let mut denied = monitor_fixture();
    let denied_name = denied.state.atoms.intern("PR4C_CONFIGURE_NOTIFY", false).0;
    let calls = denied.install_policy(|_, call| {
        if call.hook == ExtensionHook::Receive
            && call.client == NOTIFY_RECEIVER_DENIED
            && call.receive == Some(EventSubject::Window(ROOT_WINDOW))
            && call.events == Some(vec![EventDescription::core(22)])
            && call.request == request_opcodes(128, yserver_protocol::x11::randr::RR_SET_MONITOR)
        {
            Decision::Deny(DENY)
        } else {
            Decision::Allow
        }
    });
    assert!(matches!(
        monitor_request(&mut denied),
        RequestOutcome::Handled
    ));
    assert_eq!(
        denied.state.randr_client_monitors.len(),
        1,
        "SetMonitor took effect"
    );
    assert_eq!(
        denied.state.randr_client_monitors[0].name, denied_name,
        "SetMonitor stores the requested monitor name"
    );
    assert!(crate::core_loop::client_io::failed_writers(&denied.state.clients).is_empty());
    assert!(
        denied.drain(NOTIFY_SENDER).is_empty(),
        "void request has no reply or error"
    );
    assert!(
        denied.drain(NOTIFY_RECEIVER_DENIED).is_empty(),
        "denied root selector receives no ConfigureNotify"
    );
    assert_eq!(
        denied.drain(NOTIFY_RECEIVER_ALLOWED),
        allowed_frame,
        "other root selector equals its literal baseline"
    );
    assert_root_notify_receive_calls(
        &allow_calls.lock().unwrap(),
        yserver_protocol::x11::randr::RR_SET_MONITOR,
    );
    assert_root_notify_receive_calls(
        &calls.lock().unwrap(),
        yserver_protocol::x11::randr::RR_SET_MONITOR,
    );
}
