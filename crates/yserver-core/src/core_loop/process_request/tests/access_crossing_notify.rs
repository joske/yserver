use super::{
    access_notify_support::{
        CallOrder, CaseExpect, ExpectedReceive, NOTIFY_RECEIVER_ALLOWED, NOTIFY_RECEIVER_DENIED,
        NOTIFY_SENDER, NotifyFixture, ReceiveCase, Stream, append_event_u16, append_event_u32,
        assert_forbidden_hooks, assert_receive_trace, check_receive_case, finish_literal_event,
        literal_event_header, receive_trace, request_opcodes,
    },
    extensions::ExtensionHook,
    *,
};
use crate::{
    access::{Decision, DenyError, EventDescription, EventSubject},
    backend::WindowHandle,
    core_loop::message::InputOrigin,
    host_x11::{HostKeyEvent, HostPointerEvent, HostXidMap, PointerEventKind},
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
const SENDER_SEQUENCE: u16 = 0x1a2b;
const DUAL_SEQUENCE: u16 = 0x2345;
const ROOT_SEQUENCE: u16 = 0x4567;
const DENY: DenyError = DenyError {
    code: x11::error::BAD_ACCESS,
    value: 0,
};

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
    assert!(!fixture.state.policies_installed());
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

fn run_screen_resize_receive_case(
    name: &'static str,
    denied_client: ClientId,
    denied_subject: EventSubject,
    no_policy_last: bool,
) {
    let build = || (resize_fixture(), ());
    let act = |fixture: &mut NotifyFixture, _: &(), _: &std::sync::Mutex<Vec<ExpectedReceive>>| {
        assert!(matches!(resize(fixture), RequestOutcome::Handled));
        assert_resized(fixture);
        assert!(fixture.drain(NOTIFY_SENDER).is_empty());
    };
    let expect = |_: &()| {
        let request = request_opcodes(128, yserver_protocol::x11::randr::RR_SET_SCREEN_SIZE);
        let description = EventDescription::core(22);
        let receives = vec![
            ExpectedReceive {
                client: NOTIFY_RECEIVER_DENIED,
                request,
                subject: EventSubject::Window(COMPOSITE_OVERLAY_WINDOW),
                events: vec![description],
            },
            ExpectedReceive {
                client: NOTIFY_RECEIVER_DENIED,
                request,
                subject: EventSubject::Window(ROOT_WINDOW),
                events: vec![description],
            },
            ExpectedReceive {
                client: NOTIFY_RECEIVER_ALLOWED,
                request,
                subject: EventSubject::Window(ROOT_WINDOW),
                events: vec![description],
            },
        ];
        let overlay = literal_configure_notify(
            ClientByteOrder::BigEndian,
            DUAL_SEQUENCE,
            COMPOSITE_OVERLAY_WINDOW,
            COMPOSITE_OVERLAY_WINDOW,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
        );
        let parent = literal_configure_notify(
            ClientByteOrder::BigEndian,
            DUAL_SEQUENCE,
            ROOT_WINDOW,
            COMPOSITE_OVERLAY_WINDOW,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
        );
        let extension_suffix = [
            literal_present_configure_notify(
                ClientByteOrder::BigEndian,
                DUAL_SEQUENCE,
                COMPOSITE_OVERLAY_WINDOW,
            ),
            literal_screen_change_notify(
                ClientByteOrder::BigEndian,
                DUAL_SEQUENCE,
                COMPOSITE_OVERLAY_WINDOW,
            ),
        ]
        .concat();
        let dual = [
            overlay.as_slice(),
            parent.as_slice(),
            extension_suffix.as_slice(),
        ]
        .concat();
        let dual_denied = match denied_subject {
            EventSubject::Window(COMPOSITE_OVERLAY_WINDOW) => {
                [parent.as_slice(), extension_suffix.as_slice()].concat()
            }
            EventSubject::Window(ROOT_WINDOW) if denied_client == NOTIFY_RECEIVER_DENIED => {
                [overlay.as_slice(), extension_suffix.as_slice()].concat()
            }
            _ => dual.clone(),
        };
        let root = literal_configure_notify(
            ClientByteOrder::LittleEndian,
            ROOT_SEQUENCE,
            ROOT_WINDOW,
            ROOT_WINDOW,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
        );
        let mut root_denied = None;
        let mut dual_stream_denied = None;
        if denied_client == NOTIFY_RECEIVER_ALLOWED {
            root_denied = Some(Vec::new());
        } else if denied_client == NOTIFY_RECEIVER_DENIED {
            dual_stream_denied = Some(dual_denied);
        }
        let streams = vec![
            Stream::unchanged(NOTIFY_SENDER, Vec::new()),
            Stream {
                client: NOTIFY_RECEIVER_DENIED,
                bytes: dual,
                denied: dual_stream_denied,
            },
            Stream {
                client: NOTIFY_RECEIVER_ALLOWED,
                bytes: root,
                denied: root_denied,
            },
        ];
        let denied_call = receives
            .iter()
            .find(|receive| receive.client == denied_client && receive.subject == denied_subject)
            .expect("denied resize copy is in the full trace")
            .clone();
        let mut expectation =
            CaseExpect::new(streams, receives, CallOrder::PerClient, vec![denied_call]);
        expectation.no_policy_last = no_policy_last;
        expectation.deny_before_allow = denied_client == NOTIFY_RECEIVER_ALLOWED;
        expectation
    };
    check_receive_case(&ReceiveCase {
        name,
        build: &build,
        act: &act,
        expect: &expect,
    });
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
    assert!(!fixture.state.policies_installed());
    fixture
}

// mutation: swap R3 and R4 (C01)
#[test]
fn receive_screen_resize_overlay_copy_denial() {
    run_screen_resize_receive_case(
        "screen resize denies the overlay StructureNotify copy",
        NOTIFY_RECEIVER_DENIED,
        EventSubject::Window(COMPOSITE_OVERLAY_WINDOW),
        false,
    );
}

// mutation: pass the overlay as R4's window (C02)
#[test]
fn receive_screen_resize_parent_copy_denial() {
    run_screen_resize_receive_case(
        "screen resize denies the COW parent SubstructureNotify copy",
        NOTIFY_RECEIVER_DENIED,
        EventSubject::Window(ROOT_WINDOW),
        true,
    );
}

// mutation: leave R2 unchecked (C03)
#[test]
fn receive_screen_resize_root_denial() {
    run_screen_resize_receive_case(
        "screen resize denies the independent root selector copy",
        NOTIFY_RECEIVER_ALLOWED,
        EventSubject::Window(ROOT_WINDOW),
        false,
    );
}

// mutation: leave R1 unchecked (C04)
#[test]
fn receive_root_config_notify_denial() {
    let build = || (monitor_fixture(), ());
    let act = |fixture: &mut NotifyFixture, _: &(), _: &std::sync::Mutex<Vec<ExpectedReceive>>| {
        let expected_name = fixture.state.atoms.intern("PR4C_CONFIGURE_NOTIFY", false).0;
        assert!(matches!(monitor_request(fixture), RequestOutcome::Handled));
        assert_eq!(fixture.state.randr_client_monitors.len(), 1);
        assert_eq!(fixture.state.randr_client_monitors[0].name, expected_name);
        assert!(fixture.drain(NOTIFY_SENDER).is_empty());
        assert!(crate::core_loop::client_io::failed_writers(&fixture.state.clients).is_empty());
    };
    let expect = |_: &()| {
        let request = request_opcodes(128, yserver_protocol::x11::randr::RR_SET_MONITOR);
        let receive = |client| ExpectedReceive {
            client,
            request,
            subject: EventSubject::Window(ROOT_WINDOW),
            events: vec![EventDescription::core(22)],
        };
        let denied = receive(NOTIFY_RECEIVER_DENIED);
        let mut expectation = CaseExpect::new(
            vec![
                Stream::unchanged(NOTIFY_SENDER, Vec::new()),
                Stream {
                    client: NOTIFY_RECEIVER_DENIED,
                    bytes: literal_configure_notify(
                        ClientByteOrder::BigEndian,
                        DUAL_SEQUENCE,
                        ROOT_WINDOW,
                        ROOT_WINDOW,
                        800,
                        600,
                    ),
                    denied: Some(Vec::new()),
                },
                Stream::unchanged(
                    NOTIFY_RECEIVER_ALLOWED,
                    literal_configure_notify(
                        ClientByteOrder::LittleEndian,
                        ROOT_SEQUENCE,
                        ROOT_WINDOW,
                        ROOT_WINDOW,
                        800,
                        600,
                    ),
                ),
            ],
            vec![denied.clone(), receive(NOTIFY_RECEIVER_ALLOWED)],
            CallOrder::Multiset,
            vec![denied],
        );
        expectation.deny_before_allow = true;
        expectation
    };
    check_receive_case(&ReceiveCase {
        name: "RRSetMonitor root ConfigureNotify denial",
        build: &build,
        act: &act,
        expect: &expect,
    });
}

const CROSS_PARENT: ResourceId = ResourceId(0x0051_0010);
const CROSS_CHILD: ResourceId = ResourceId(0x0051_0011);
const CROSS_PARENT_XID: u32 = 0xA010;
const CROSS_CHILD_XID: u32 = 0xA011;
const ENTER_LEAVE_MASK: u32 = 0x0000_0030;
const POINTER_MOTION_MASK: u32 = 0x0000_0040;
const BUTTON_PRESS_MASK: u32 = 0x0000_0004;
const CROSS_START_X: i16 = 25;
const CROSS_START_Y: i16 = 20;
const CROSS_X: i16 = 35;
const CROSS_Y: i16 = 20;
const GRAB_SEQUENCE: u16 = 0x0041;
const UNGRAB_SEQUENCE: u16 = 0x0042;

fn core_crossing_fixture() -> (NotifyFixture, HostXidMap) {
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
        let state = fixture.state.clients.get_mut(&client.0).unwrap();
        state.byte_order = order;
        state.last_sequence.store(sequence, Ordering::Relaxed);
    }
    for (window, parent, x, y, width, height) in [
        (CROSS_PARENT, ROOT_WINDOW, 20, 10, 100, 80),
        (CROSS_CHILD, CROSS_PARENT, 10, 5, 40, 30),
    ] {
        fixture.state.resources.create_window(
            NOTIFY_SENDER,
            yserver_protocol::x11::CreateWindowRequest {
                depth: 24,
                window,
                parent,
                x,
                y,
                width,
                height,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
        let _ = fixture.state.resources.map_window(window);
    }
    for client in [NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED] {
        let state = fixture.state.clients.get_mut(&client.0).unwrap();
        state.event_masks.insert(CROSS_PARENT, ENTER_LEAVE_MASK);
        state
            .event_masks
            .insert(CROSS_CHILD, ENTER_LEAVE_MASK | POINTER_MOTION_MASK);
    }
    fixture.state.pointer_root = (CROSS_START_X, CROSS_START_Y);
    fixture.state.master_sprite_root = (CROSS_START_X, CROSS_START_Y);
    let mut xid_map = HostXidMap::new();
    xid_map.insert(CROSS_PARENT_XID, CROSS_PARENT);
    xid_map.insert(CROSS_CHILD_XID, CROSS_CHILD);
    (fixture, xid_map)
}

fn core_pointer_event(
    kind: PointerEventKind,
    host_xid: u32,
    detail: u8,
    time: u32,
    root_x: i16,
    root_y: i16,
) -> HostPointerEvent {
    let (host_x, host_y) = if host_xid == CROSS_PARENT_XID {
        (20, 10)
    } else {
        (10, 5)
    };
    HostPointerEvent {
        origin: InputOrigin::XTest(4),
        kind,
        host_xid,
        detail,
        time,
        root_x,
        root_y,
        // translate_host_event uses the producer window's parent-relative
        // origin plus these coordinates to recover the root position.
        event_x: root_x - host_x,
        event_y: root_y - host_y,
        state: 0,
        crossing_mode: 0,
        child: 0,
        raw_dx: 0,
        raw_dy: 0,
        device_axes: None,
        tree_change: false,
    }
}

fn feed_core_pointer_event(
    fixture: &mut NotifyFixture,
    xid_map: &HostXidMap,
    event: HostPointerEvent,
) {
    let dropped = crate::core_loop::pointer_fanout::pointer_event_fanout_to_state(
        &mut fixture.state,
        &mut fixture.backend,
        xid_map,
        event,
        true,
        false,
    );
    assert!(
        dropped.is_empty(),
        "pointer event fanout does not report a client disconnect"
    );
    assert_no_writer_failures(fixture);
}

fn literal_core_crossing(
    order: ClientByteOrder,
    event_type: u8,
    detail: u8,
    sequence: u16,
    time: u32,
    window: ResourceId,
    child: ResourceId,
    root_x: i16,
    root_y: i16,
    event_x: i16,
    event_y: i16,
    mode: u8,
    focus: bool,
) -> Vec<u8> {
    let mut event = literal_event_header(event_type, detail, sequence, order);
    append_event_u32(&mut event, order, time);
    append_event_u32(&mut event, order, ROOT_WINDOW.0);
    append_event_u32(&mut event, order, window.0);
    append_event_u32(&mut event, order, child.0);
    append_event_u16(&mut event, order, root_x as u16);
    append_event_u16(&mut event, order, root_y as u16);
    append_event_u16(&mut event, order, event_x as u16);
    append_event_u16(&mut event, order, event_y as u16);
    append_event_u16(&mut event, order, 0); // state
    event.extend_from_slice(&[mode, 0x02 | u8::from(focus)]);
    finish_literal_event(event)
}

fn literal_core_motion(
    order: ClientByteOrder,
    sequence: u16,
    time: u32,
    window: ResourceId,
    root_x: i16,
    root_y: i16,
    event_x: i16,
    event_y: i16,
) -> Vec<u8> {
    let mut event = literal_event_header(6, 0, sequence, order);
    append_event_u32(&mut event, order, time);
    append_event_u32(&mut event, order, ROOT_WINDOW.0);
    append_event_u32(&mut event, order, window.0);
    append_event_u32(&mut event, order, 0); // no child
    append_event_u16(&mut event, order, root_x as u16);
    append_event_u16(&mut event, order, root_y as u16);
    append_event_u16(&mut event, order, event_x as u16);
    append_event_u16(&mut event, order, event_y as u16);
    append_event_u16(&mut event, order, 0); // state
    event.extend_from_slice(&[1, 0]); // same_screen, pad
    finish_literal_event(event)
}

fn literal_core_button_press(
    order: ClientByteOrder,
    sequence: u16,
    time: u32,
    window: ResourceId,
    root_x: i16,
    root_y: i16,
    event_x: i16,
    event_y: i16,
) -> Vec<u8> {
    let mut event = literal_event_header(4, 1, sequence, order);
    append_event_u32(&mut event, order, time);
    append_event_u32(&mut event, order, ROOT_WINDOW.0);
    append_event_u32(&mut event, order, window.0);
    append_event_u32(&mut event, order, 0); // no child
    append_event_u16(&mut event, order, root_x as u16);
    append_event_u16(&mut event, order, root_y as u16);
    append_event_u16(&mut event, order, event_x as u16);
    append_event_u16(&mut event, order, event_y as u16);
    append_event_u16(&mut event, order, 0); // state
    event.extend_from_slice(&[1, 0]); // same_screen, pad
    finish_literal_event(event)
}

fn literal_grab_reply(order: ClientByteOrder, sequence: u16, status: u8) -> Vec<u8> {
    let mut reply = literal_event_header(1, status, sequence, order);
    append_event_u32(&mut reply, order, 0); // no reply payload
    reply.extend_from_slice(&[0; 24]);
    reply
}

fn grab_pointer(
    fixture: &mut NotifyFixture,
    owner: ClientId,
    owner_events: bool,
    window: ResourceId,
    event_mask: u16,
    sequence: u16,
) -> RequestOutcome {
    let mut body = Vec::with_capacity(20);
    body.extend_from_slice(&window.0.to_le_bytes());
    body.extend_from_slice(&event_mask.to_le_bytes());
    body.extend_from_slice(&[1, 1]); // async pointer and keyboard modes
    body.extend_from_slice(&0_u32.to_le_bytes()); // no confinement
    body.extend_from_slice(&0_u32.to_le_bytes()); // no cursor
    body.extend_from_slice(&0_u32.to_le_bytes()); // CurrentTime
    let outcome = fixture
        .request(
            owner,
            SequenceNumber(sequence),
            RequestHeader {
                opcode: 26,
                data: u8::from(owner_events),
                length_units: 6,
            },
            &body,
        )
        .expect("GrabPointer through process_request");
    assert_no_writer_failures(fixture);
    outcome
}

fn ungrab_pointer(fixture: &mut NotifyFixture, owner: ClientId, sequence: u16) -> RequestOutcome {
    let outcome = fixture
        .request(
            owner,
            SequenceNumber(sequence),
            RequestHeader {
                opcode: 27,
                data: 0,
                length_units: 2,
            },
            &0_u32.to_le_bytes(),
        )
        .expect("UngrabPointer through process_request");
    assert_no_writer_failures(fixture);
    outcome
}

fn grab_chain_child_to_parent(order: ClientByteOrder, sequence: u16, mode: u8) -> Vec<u8> {
    [
        literal_core_crossing(
            order,
            8,
            0,
            sequence,
            0,
            CROSS_CHILD,
            ResourceId(0),
            CROSS_X,
            CROSS_Y,
            5,
            5,
            mode,
            true,
        )
        .as_slice(),
        literal_core_crossing(
            order,
            7,
            2,
            sequence,
            0,
            CROSS_PARENT,
            ResourceId(0),
            CROSS_X,
            CROSS_Y,
            15,
            10,
            mode,
            true,
        )
        .as_slice(),
    ]
    .concat()
}

fn grab_chain_parent_to_child(order: ClientByteOrder, sequence: u16, mode: u8) -> Vec<u8> {
    [
        literal_core_crossing(
            order,
            8,
            2,
            sequence,
            0,
            CROSS_PARENT,
            ResourceId(0),
            CROSS_X,
            CROSS_Y,
            15,
            10,
            mode,
            true,
        )
        .as_slice(),
        literal_core_crossing(
            order,
            7,
            0,
            sequence,
            0,
            CROSS_CHILD,
            ResourceId(0),
            CROSS_X,
            CROSS_Y,
            5,
            5,
            mode,
            true,
        )
        .as_slice(),
    ]
    .concat()
}

fn assert_no_writer_failures(fixture: &NotifyFixture) {
    assert!(
        crate::core_loop::client_io::failed_writers(&fixture.state.clients).is_empty(),
        "Receive denial is not a transport failure"
    );
}

// Kept focused: this case checkpoints explicit crossings, GrabPointer, and UngrabPointer streams.
// mutation: reverse C2's chain order (C05)
#[test]
fn receive_core_crossing_baseline_frames() {
    let (mut fixture, xid_map) = core_crossing_fixture();
    assert!(!fixture.state.policies_installed());
    run_explicit_parent_child_crossings(&mut fixture, &xid_map);
    assert!(fixture.drain(NOTIFY_SENDER).is_empty());
    for (client, order, sequence) in [
        (
            NOTIFY_RECEIVER_DENIED,
            ClientByteOrder::BigEndian,
            DUAL_SEQUENCE,
        ),
        (
            NOTIFY_RECEIVER_ALLOWED,
            ClientByteOrder::LittleEndian,
            ROOT_SEQUENCE,
        ),
    ] {
        assert_eq!(
            fixture.drain(client),
            literal_explicit_crossings(order, sequence, true),
            "explicit Leave/Enter followed by MotionNotify stream"
        );
    }

    assert!(matches!(
        grab_pointer(
            &mut fixture,
            NOTIFY_SENDER,
            false,
            CROSS_PARENT,
            0,
            GRAB_SEQUENCE,
        ),
        RequestOutcome::Handled
    ));
    assert!(fixture.state.active_pointer_grab.is_some());
    assert_eq!(
        fixture.drain(NOTIFY_SENDER),
        literal_grab_reply(ClientByteOrder::LittleEndian, GRAB_SEQUENCE, 0),
        "GrabPointer succeeds with its literal reply"
    );
    assert_eq!(
        fixture.drain(NOTIFY_RECEIVER_DENIED),
        grab_chain_child_to_parent(ClientByteOrder::BigEndian, DUAL_SEQUENCE, 1),
        "big-endian NotifyGrab chain is Leave child then Enter parent"
    );
    assert_eq!(
        fixture.drain(NOTIFY_RECEIVER_ALLOWED),
        grab_chain_child_to_parent(ClientByteOrder::LittleEndian, ROOT_SEQUENCE, 1),
        "little-endian NotifyGrab chain is Leave child then Enter parent"
    );
    assert!(matches!(
        ungrab_pointer(&mut fixture, NOTIFY_SENDER, UNGRAB_SEQUENCE),
        RequestOutcome::Handled
    ));
    assert!(fixture.state.active_pointer_grab.is_none());
    assert!(
        fixture.drain(NOTIFY_SENDER).is_empty(),
        "UngrabPointer is void"
    );
    assert_eq!(
        fixture.drain(NOTIFY_RECEIVER_DENIED),
        grab_chain_parent_to_child(ClientByteOrder::BigEndian, DUAL_SEQUENCE, 2),
        "big-endian NotifyUngrab chain is Leave parent then Enter child"
    );
    assert_eq!(
        fixture.drain(NOTIFY_RECEIVER_ALLOWED),
        grab_chain_parent_to_child(ClientByteOrder::LittleEndian, ROOT_SEQUENCE, 2),
        "little-endian NotifyUngrab chain is Leave parent then Enter child"
    );
    assert_no_writer_failures(&fixture);
}

fn run_explicit_parent_child_crossings(fixture: &mut NotifyFixture, xid_map: &HostXidMap) {
    assert_eq!(
        fixture.state.master_sprite_root,
        (CROSS_START_X, CROSS_START_Y),
        "explicit crossing run starts inside the parent and outside the child"
    );
    assert_eq!(
        fixture.state.pointer_root,
        (CROSS_START_X, CROSS_START_Y),
        "pointer starts inside the parent and outside the child"
    );
    feed_core_pointer_event(
        fixture,
        xid_map,
        core_pointer_event(
            PointerEventKind::LeaveNotify,
            CROSS_PARENT_XID,
            2,
            0x1357,
            CROSS_X,
            CROSS_Y,
        ),
    );
    feed_core_pointer_event(
        fixture,
        xid_map,
        core_pointer_event(
            PointerEventKind::EnterNotify,
            CROSS_CHILD_XID,
            0,
            0x1357,
            CROSS_X,
            CROSS_Y,
        ),
    );
    assert_eq!(
        fixture.state.master_sprite_root,
        (CROSS_START_X, CROSS_START_Y),
        "explicit Leave/Enter notifications do not move the sprite"
    );
    assert_eq!(
        fixture.state.pointer_root,
        (CROSS_X, CROSS_Y),
        "the Enter event records the target pointer position before motion"
    );
    feed_core_pointer_event(
        fixture,
        xid_map,
        core_pointer_event(
            PointerEventKind::MotionNotify,
            CROSS_CHILD_XID,
            0,
            0x1358,
            CROSS_X,
            CROSS_Y,
        ),
    );
    assert_eq!(
        fixture.state.master_sprite_root,
        (CROSS_X, CROSS_Y),
        "MotionNotify moves the sprite from the parent into the child"
    );
    assert_eq!(
        fixture.state.pointer_root,
        (CROSS_X, CROSS_Y),
        "MotionNotify moves the pointer from the parent into the child"
    );
}

fn literal_explicit_crossings(
    order: ClientByteOrder,
    sequence: u16,
    include_enter: bool,
) -> Vec<u8> {
    let leave = literal_core_crossing(
        order,
        8,
        2,
        sequence,
        0x1357,
        CROSS_PARENT,
        ResourceId(0),
        CROSS_X,
        CROSS_Y,
        15,
        10,
        0,
        true,
    );
    let motion = literal_core_motion(order, sequence, 0x1358, CROSS_CHILD, CROSS_X, CROSS_Y, 5, 5);
    if include_enter {
        [
            leave.as_slice(),
            literal_core_crossing(
                order,
                7,
                0,
                sequence,
                0x1357,
                CROSS_CHILD,
                ResourceId(0),
                CROSS_X,
                CROSS_Y,
                5,
                5,
                0,
                true,
            )
            .as_slice(),
            motion.as_slice(),
        ]
        .concat()
    } else {
        [leave.as_slice(), motion.as_slice()].concat()
    }
}

// mutation: leave C1 unchecked (C06)
#[test]
fn receive_core_crossing_chain_denies_one_window() {
    let build = || {
        let (fixture, xid_map) = core_crossing_fixture();
        (fixture, xid_map)
    };
    let act = |fixture: &mut NotifyFixture,
               xid_map: &HostXidMap,
               _: &std::sync::Mutex<Vec<ExpectedReceive>>| {
        run_explicit_parent_child_crossings(fixture, xid_map);
        assert_eq!(fixture.state.master_sprite_root, (CROSS_X, CROSS_Y));
        assert_eq!(fixture.state.pointer_root, (CROSS_X, CROSS_Y));
        assert_no_writer_failures(fixture);
    };
    let expect = |_: &HostXidMap| {
        let parent_leave = |client| ExpectedReceive {
            client,
            request: None,
            subject: EventSubject::Window(CROSS_PARENT),
            events: vec![EventDescription::core(8)],
        };
        let child_enter = |client| ExpectedReceive {
            client,
            request: None,
            subject: EventSubject::Window(CROSS_CHILD),
            events: vec![EventDescription::core(7)],
        };
        let denied_enter = child_enter(NOTIFY_RECEIVER_DENIED);
        CaseExpect::new(
            vec![
                Stream::unchanged(NOTIFY_SENDER, Vec::new()),
                Stream {
                    client: NOTIFY_RECEIVER_DENIED,
                    bytes: literal_explicit_crossings(
                        ClientByteOrder::BigEndian,
                        DUAL_SEQUENCE,
                        true,
                    ),
                    denied: Some(literal_explicit_crossings(
                        ClientByteOrder::BigEndian,
                        DUAL_SEQUENCE,
                        false,
                    )),
                },
                Stream::unchanged(
                    NOTIFY_RECEIVER_ALLOWED,
                    literal_explicit_crossings(ClientByteOrder::LittleEndian, ROOT_SEQUENCE, true),
                ),
            ],
            vec![
                parent_leave(NOTIFY_RECEIVER_DENIED),
                parent_leave(NOTIFY_RECEIVER_ALLOWED),
                denied_enter.clone(),
                child_enter(NOTIFY_RECEIVER_ALLOWED),
            ],
            CallOrder::Multiset,
            vec![denied_enter],
        )
    };
    check_receive_case(&ReceiveCase {
        name: "core crossing denies only the child Enter copy",
        build: &build,
        act: &act,
        expect: &expect,
    });
}

fn activate_parent_grab(
    fixture: &mut NotifyFixture,
    owner: ClientId,
    owner_events: bool,
    sequence: u16,
) {
    *fixture
        .state
        .clients
        .get_mut(&owner.0)
        .expect("grab owner")
        .event_masks
        .entry(CROSS_CHILD)
        .or_default() |= BUTTON_PRESS_MASK;
    fixture.state.pointer_root = (CROSS_X, CROSS_Y);
    fixture.state.master_sprite_root = (CROSS_X, CROSS_Y);
    assert!(matches!(
        grab_pointer(
            fixture,
            owner,
            owner_events,
            CROSS_PARENT,
            BUTTON_PRESS_MASK as u16,
            sequence,
        ),
        RequestOutcome::Handled
    ));
    assert!(fixture.state.active_pointer_grab.is_some());
    let order = if owner == NOTIFY_RECEIVER_DENIED {
        ClientByteOrder::BigEndian
    } else {
        ClientByteOrder::LittleEndian
    };
    assert_eq!(
        fixture.drain(owner),
        [
            grab_chain_child_to_parent(order, sequence, 1).as_slice(),
            literal_grab_reply(order, sequence, 0).as_slice(),
        ]
        .concat(),
        "grab owner sees NotifyGrab before the successful reply"
    );
    let other = if owner == NOTIFY_RECEIVER_DENIED {
        NOTIFY_RECEIVER_ALLOWED
    } else {
        NOTIFY_RECEIVER_DENIED
    };
    let other_order = if other == NOTIFY_RECEIVER_DENIED {
        ClientByteOrder::BigEndian
    } else {
        ClientByteOrder::LittleEndian
    };
    let other_sequence = if other == NOTIFY_RECEIVER_DENIED {
        DUAL_SEQUENCE
    } else {
        ROOT_SEQUENCE
    };
    assert_eq!(
        fixture.drain(other),
        grab_chain_child_to_parent(other_order, other_sequence, 1),
        "other selectors receive the activation chain"
    );
    assert!(fixture.drain(NOTIFY_SENDER).is_empty());
    assert_no_writer_failures(fixture);
}

// Kept focused: owner routing and the button event continue on one active-grab fixture.
// mutation: check the grab window instead of the crossing window (C07)
#[test]
fn receive_core_grabbed_crossing_owner_denied() {
    let (mut allowed, allowed_xid_map) = core_crossing_fixture();
    activate_parent_grab(&mut allowed, NOTIFY_RECEIVER_DENIED, true, GRAB_SEQUENCE);
    let allow_calls = allowed.install_policy(|_, _| Decision::Allow);
    feed_core_pointer_event(
        &mut allowed,
        &allowed_xid_map,
        core_pointer_event(
            PointerEventKind::EnterNotify,
            CROSS_CHILD_XID,
            0,
            0x2468,
            CROSS_X,
            CROSS_Y,
        ),
    );
    assert_eq!(
        allowed.drain(NOTIFY_RECEIVER_DENIED),
        literal_core_crossing(
            ClientByteOrder::BigEndian,
            7,
            0,
            GRAB_SEQUENCE,
            0x2468,
            CROSS_CHILD,
            ResourceId(0),
            CROSS_X,
            CROSS_Y,
            5,
            5,
            0,
            true,
        ),
        "allow control delivers the crossing to the grab owner on its selected child"
    );
    assert!(allowed.drain(NOTIFY_RECEIVER_ALLOWED).is_empty());
    let allow_log = allow_calls.lock().unwrap();
    let expected_crossing = [ExpectedReceive {
        client: NOTIFY_RECEIVER_DENIED,
        request: None,
        subject: EventSubject::Window(CROSS_CHILD),
        events: vec![EventDescription::core(7)],
    }];
    assert_receive_trace(
        "core grabbed crossing owner allow control",
        "allow-all",
        CallOrder::Multiset,
        &expected_crossing,
        &allow_log,
        "Receive trace",
    );
    assert_forbidden_hooks(
        "core grabbed crossing owner allow control",
        "allow-all",
        &allow_log,
        &[ExtensionHook::Send],
    );
    drop(allow_log);
    feed_core_pointer_event(
        &mut allowed,
        &allowed_xid_map,
        core_pointer_event(
            PointerEventKind::ButtonPress,
            CROSS_CHILD_XID,
            1,
            0x2469,
            CROSS_X,
            CROSS_Y,
        ),
    );
    assert_eq!(
        allowed.drain(NOTIFY_RECEIVER_DENIED),
        literal_core_button_press(
            ClientByteOrder::BigEndian,
            GRAB_SEQUENCE,
            0x2469,
            CROSS_CHILD,
            CROSS_X,
            CROSS_Y,
            5,
            5,
        ),
        "allow-control ButtonPress reaches the grab owner literally"
    );
    assert!(allowed.drain(NOTIFY_RECEIVER_ALLOWED).is_empty());
    assert!(
        allowed.state.active_pointer_grab.is_some(),
        "allow-control button leaves the grab active"
    );
    assert_eq!(
        receive_trace(
            "core grabbed crossing owner allow control",
            "allow-all",
            &allow_calls.lock().unwrap(),
        )
        .len(),
        1,
        "allow-control cooked button adds no Receive call"
    );
    assert_no_writer_failures(&allowed);

    let (mut denied, xid_map) = core_crossing_fixture();
    activate_parent_grab(&mut denied, NOTIFY_RECEIVER_DENIED, true, GRAB_SEQUENCE);
    let calls = denied.install_policy(|_, call| {
        if call.hook == ExtensionHook::Receive
            && call.client == NOTIFY_RECEIVER_DENIED
            && call.receive == Some(EventSubject::Window(CROSS_CHILD))
            && call.events == Some(vec![EventDescription::core(7)])
        {
            Decision::Deny(DENY)
        } else {
            Decision::Allow
        }
    });
    feed_core_pointer_event(
        &mut denied,
        &xid_map,
        core_pointer_event(
            PointerEventKind::EnterNotify,
            CROSS_CHILD_XID,
            0,
            0x2468,
            CROSS_X,
            CROSS_Y,
        ),
    );
    assert!(
        denied.drain(NOTIFY_RECEIVER_DENIED).is_empty(),
        "denied crossing copy is absent from the grab owner"
    );
    assert!(denied.drain(NOTIFY_RECEIVER_ALLOWED).is_empty());
    assert!(
        denied.state.active_pointer_grab.is_some(),
        "grab remains active"
    );
    let deny_log = calls.lock().unwrap();
    assert_receive_trace(
        "core grabbed crossing owner denial",
        "deny",
        CallOrder::Multiset,
        &expected_crossing,
        &deny_log,
        "Receive trace",
    );
    assert_forbidden_hooks(
        "core grabbed crossing owner denial",
        "deny",
        &deny_log,
        &[ExtensionHook::Send],
    );
    drop(deny_log);

    feed_core_pointer_event(
        &mut denied,
        &xid_map,
        core_pointer_event(
            PointerEventKind::ButtonPress,
            CROSS_CHILD_XID,
            1,
            0x2469,
            CROSS_X,
            CROSS_Y,
        ),
    );
    assert_eq!(
        denied.drain(NOTIFY_RECEIVER_DENIED),
        literal_core_button_press(
            ClientByteOrder::BigEndian,
            GRAB_SEQUENCE,
            0x2469,
            CROSS_CHILD,
            CROSS_X,
            CROSS_Y,
            5,
            5,
        ),
        "following cooked ButtonPress still reaches the owner without Receive"
    );
    assert!(denied.drain(NOTIFY_RECEIVER_ALLOWED).is_empty());
    assert!(
        denied.state.active_pointer_grab.is_some(),
        "button does not end grab"
    );
    assert_eq!(
        receive_trace(
            "core grabbed crossing owner denial",
            "deny",
            &calls.lock().unwrap(),
        )
        .len(),
        1,
        "cooked button path adds no Receive call"
    );
    assert_no_writer_failures(&denied);
}

fn assert_pointer_grabs_equal(
    left: Option<crate::server::ActivePointerGrab>,
    right: Option<crate::server::ActivePointerGrab>,
) {
    match (left, right) {
        (Some(left), Some(right)) => {
            assert_eq!(left.owner, right.owner);
            assert_eq!(left.grab_window, right.grab_window);
            assert_eq!(left.event_mask, right.event_mask);
            assert_eq!(left.cursor, right.cursor);
            assert_eq!(left.time, right.time);
            assert_eq!(left.owner_events, right.owner_events);
            assert_eq!(left.via_xi2, right.via_xi2);
            assert_eq!(left.implicit, right.implicit);
            assert_eq!(left.passive, right.passive);
            assert_eq!(left.xi2_mask, right.xi2_mask);
        }
        (None, None) => {}
        _ => panic!("active pointer grab presence differs"),
    }
}

// mutation: leave C2 unchecked (C08)
#[test]
fn receive_core_grab_chain_denial() {
    let build = || {
        let mut fixture = core_crossing_fixture().0;
        fixture.state.pointer_root = (CROSS_X, CROSS_Y);
        fixture.state.master_sprite_root = (CROSS_X, CROSS_Y);
        (fixture, ())
    };
    let act = |fixture: &mut NotifyFixture, _: &(), _: &std::sync::Mutex<Vec<ExpectedReceive>>| {
        assert!(matches!(
            grab_pointer(
                fixture,
                NOTIFY_SENDER,
                false,
                CROSS_PARENT,
                0,
                GRAB_SEQUENCE,
            ),
            RequestOutcome::Handled
        ));
        assert_eq!(
            fixture
                .state
                .active_pointer_grab
                .map(|grab| grab.grab_window),
            Some(CROSS_PARENT),
            "denying a grab crossing does not change the active grab"
        );
        assert_eq!(
            fixture.drain(NOTIFY_SENDER),
            literal_grab_reply(ClientByteOrder::LittleEndian, GRAB_SEQUENCE, 0),
            "Receive denial does not change GrabPointer's Success reply"
        );
        assert_no_writer_failures(fixture);
    };
    let expect = |_: &()| {
        let request = request_opcodes(26, 0);
        let child_leave = |client| ExpectedReceive {
            client,
            request,
            subject: EventSubject::Window(CROSS_CHILD),
            events: vec![EventDescription::core(8)],
        };
        let parent_enter = |client| ExpectedReceive {
            client,
            request,
            subject: EventSubject::Window(CROSS_PARENT),
            events: vec![EventDescription::core(7)],
        };
        let denied_leave = child_leave(NOTIFY_RECEIVER_DENIED);
        let mut expectation = CaseExpect::new(
            vec![
                Stream::unchanged(NOTIFY_SENDER, Vec::new()),
                Stream {
                    client: NOTIFY_RECEIVER_DENIED,
                    bytes: grab_chain_child_to_parent(ClientByteOrder::BigEndian, DUAL_SEQUENCE, 1),
                    denied: Some(literal_core_crossing(
                        ClientByteOrder::BigEndian,
                        7,
                        2,
                        DUAL_SEQUENCE,
                        0,
                        CROSS_PARENT,
                        ResourceId(0),
                        CROSS_X,
                        CROSS_Y,
                        15,
                        10,
                        1,
                        true,
                    )),
                },
                Stream::unchanged(
                    NOTIFY_RECEIVER_ALLOWED,
                    grab_chain_child_to_parent(ClientByteOrder::LittleEndian, ROOT_SEQUENCE, 1),
                ),
            ],
            vec![
                child_leave(NOTIFY_RECEIVER_DENIED),
                child_leave(NOTIFY_RECEIVER_ALLOWED),
                parent_enter(NOTIFY_RECEIVER_DENIED),
                parent_enter(NOTIFY_RECEIVER_ALLOWED),
            ],
            CallOrder::Multiset,
            vec![denied_leave],
        );
        expectation.after_deny = Some(Box::new(|allow, denied| {
            assert_pointer_grabs_equal(
                denied.state.active_pointer_grab,
                allow.state.active_pointer_grab,
            );
            assert_pointer_grabs_equal(
                denied.state.active_pointer_grab,
                Some(crate::server::ActivePointerGrab {
                    owner: NOTIFY_SENDER,
                    grab_window: CROSS_PARENT,
                    event_mask: 0,
                    cursor: ResourceId(0),
                    time: 0,
                    owner_events: false,
                    via_xi2: false,
                    implicit: false,
                    passive: false,
                    xi2_mask: 0,
                }),
            );
            assert!(
                denied.state.active_pointer_grab.is_some(),
                "active pointer grab remains established"
            );
        }));
        expectation
    };
    check_receive_case(&ReceiveCase {
        name: "GrabPointer chain drops only the denied child Leave",
        build: &build,
        act: &act,
        expect: &expect,
    });
}

fn literal_xi2_device_press(
    order: ClientByteOrder,
    sequence: u16,
    evtype: u16,
    deviceid: u16,
    sourceid: u16,
    time: u32,
    detail: u8,
    window: ResourceId,
    root_x: i16,
    root_y: i16,
    event_x: i16,
    event_y: i16,
) -> Vec<u8> {
    let mut event = vec![35, 137]; // GenericEvent, XI2 major opcode
    append_event_u16(&mut event, order, sequence);
    append_event_u32(&mut event, order, 22); // 88 bytes after 32-byte header
    append_event_u16(&mut event, order, evtype);
    append_event_u16(&mut event, order, deviceid);
    append_event_u32(&mut event, order, time);
    append_event_u32(&mut event, order, u32::from(detail));
    append_event_u32(&mut event, order, ROOT_WINDOW.0);
    append_event_u32(&mut event, order, window.0);
    append_event_u32(&mut event, order, 0); // child
    for coordinate in [root_x, root_y, event_x, event_y] {
        append_event_u32(&mut event, order, (i32::from(coordinate) << 16) as u32);
    }
    append_event_u16(&mut event, order, 8); // buttons length
    append_event_u16(&mut event, order, 2); // valuators length
    append_event_u16(&mut event, order, sourceid);
    append_event_u16(&mut event, order, 0); // pad
    append_event_u32(&mut event, order, 0); // flags
    append_event_u32(&mut event, order, 0); // mods.base
    append_event_u32(&mut event, order, 0); // mods.latched
    append_event_u32(&mut event, order, 0); // mods.locked
    append_event_u32(&mut event, order, 0); // mods.effective
    event.extend_from_slice(&[0; 4]); // group
    for _ in 0..8 {
        append_event_u32(&mut event, order, 0); // buttons mask
    }
    append_event_u32(&mut event, order, 0); // valuator mask word 0
    append_event_u32(&mut event, order, 0); // valuator mask word 1
    assert_eq!(event.len(), 120);
    event
}

const XI2_ENTER_LEAVE_MASK: u64 = (1 << 7) | (1 << 8);
const XI_GRAB_SEQUENCE: u16 = 0x5789;
const XI_REGRAB_SEQUENCE: u16 = 0x4b6d;
const XI_UNGRAB_SEQUENCE: u16 = 0x689a;

fn xi2_crossing_fixture() -> (NotifyFixture, HostXidMap) {
    let (mut fixture, xid_map) = core_crossing_fixture();
    for client in [NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED] {
        let client_state = fixture.state.clients.get_mut(&client.0).unwrap();
        client_state.xi2_masks.insert(
            (CROSS_PARENT, crate::xinput::DEVICEID_MASTER_POINTER),
            XI2_ENTER_LEAVE_MASK,
        );
        client_state.xi2_masks.insert(
            (CROSS_CHILD, crate::xinput::DEVICEID_MASTER_POINTER),
            XI2_ENTER_LEAVE_MASK,
        );
    }
    (fixture, xid_map)
}

fn literal_xi2_crossing(
    order: ClientByteOrder,
    sequence: u16,
    evtype: u16,
    deviceid: u16,
    time: u32,
    sourceid: u16,
    mode: u8,
    detail: u8,
    window: ResourceId,
    root_x: i16,
    root_y: i16,
    event_x: i16,
    event_y: i16,
    focus: bool,
) -> Vec<u8> {
    let mut event = vec![35, 137]; // GenericEvent, XI2 major opcode
    append_event_u16(&mut event, order, sequence);
    append_event_u32(&mut event, order, 11); // XGE length
    append_event_u16(&mut event, order, evtype);
    append_event_u16(&mut event, order, deviceid);
    append_event_u32(&mut event, order, time);
    append_event_u16(&mut event, order, sourceid);
    event.extend_from_slice(&[mode, detail]);
    append_event_u32(&mut event, order, ROOT_WINDOW.0);
    append_event_u32(&mut event, order, window.0);
    append_event_u32(&mut event, order, 0); // no child
    for coordinate in [root_x, root_y, event_x, event_y] {
        append_event_u32(&mut event, order, (i32::from(coordinate) << 16) as u32);
    }
    event.extend_from_slice(&[1, u8::from(focus)]); // same_screen, focus
    append_event_u16(&mut event, order, 1); // buttons_len
    append_event_u32(&mut event, order, 0); // mods.base
    append_event_u32(&mut event, order, 0); // mods.latched
    append_event_u32(&mut event, order, 0); // mods.locked
    append_event_u32(&mut event, order, 0); // mods.effective
    event.extend_from_slice(&[0; 4]); // group
    append_event_u32(&mut event, order, 0); // buttons mask
    assert_eq!(event.len(), 76);
    event
}

fn literal_xi2_explicit_crossings(order: ClientByteOrder, sequence: u16) -> Vec<u8> {
    [
        literal_xi2_crossing(
            order,
            sequence,
            8,
            2,
            0x1357,
            4,
            0,
            2,
            CROSS_PARENT,
            CROSS_X,
            CROSS_Y,
            15,
            10,
            true,
        )
        .as_slice(),
        literal_xi2_crossing(
            order,
            sequence,
            7,
            2,
            0x1357,
            4,
            0,
            0,
            CROSS_CHILD,
            CROSS_X,
            CROSS_Y,
            5,
            5,
            true,
        )
        .as_slice(),
        literal_core_motion(order, sequence, 0x1358, CROSS_CHILD, CROSS_X, CROSS_Y, 5, 5)
            .as_slice(),
    ]
    .concat()
}

fn literal_xi2_grab_chain(order: ClientByteOrder, sequence: u16, mode: u8) -> Vec<u8> {
    [
        literal_xi2_crossing(
            order,
            sequence,
            8,
            2,
            0,
            2,
            mode,
            0,
            CROSS_CHILD,
            CROSS_X,
            CROSS_Y,
            5,
            5,
            true,
        )
        .as_slice(),
        literal_xi2_crossing(
            order,
            sequence,
            7,
            2,
            0,
            2,
            mode,
            2,
            CROSS_PARENT,
            CROSS_X,
            CROSS_Y,
            15,
            10,
            true,
        )
        .as_slice(),
    ]
    .concat()
}

fn literal_xi2_ungrab_pointer_chain(order: ClientByteOrder, sequence: u16, mode: u8) -> Vec<u8> {
    [
        literal_xi2_crossing(
            order,
            sequence,
            8,
            2,
            0,
            2,
            mode,
            2,
            CROSS_PARENT,
            CROSS_X,
            CROSS_Y,
            15,
            10,
            true,
        )
        .as_slice(),
        literal_xi2_crossing(
            order,
            sequence,
            7,
            2,
            0,
            2,
            mode,
            0,
            CROSS_CHILD,
            CROSS_X,
            CROSS_Y,
            5,
            5,
            true,
        )
        .as_slice(),
    ]
    .concat()
}

fn xi2_grab_fixture() -> NotifyFixture {
    let mut fixture = core_crossing_fixture().0;
    for client in [NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED] {
        let client_state = fixture.state.clients.get_mut(&client.0).unwrap();
        client_state.event_masks.clear();
        client_state.xi2_masks.clear();
    }
    fixture
}

fn xi_grab_device(
    fixture: &mut NotifyFixture,
    deviceid: u16,
    window: ResourceId,
    sequence: u16,
    event_mask: u32,
) -> RequestOutcome {
    let mut body = Vec::with_capacity(24);
    body.extend_from_slice(&window.0.to_le_bytes());
    body.extend_from_slice(&0_u32.to_le_bytes()); // CurrentTime
    body.extend_from_slice(&0_u32.to_le_bytes()); // no cursor
    body.extend_from_slice(&deviceid.to_le_bytes());
    body.extend_from_slice(&[1, 1, 0, 0]); // async modes, owner_events false, pad
    body.extend_from_slice(&u16::from(event_mask != 0).to_le_bytes()); // mask_len in 4-byte units
    if event_mask != 0 {
        body.extend_from_slice(&event_mask.to_le_bytes());
    }
    let outcome = fixture
        .request(
            NOTIFY_RECEIVER_DENIED,
            SequenceNumber(sequence),
            RequestHeader {
                opcode: 137,
                data: 51,
                length_units: if event_mask == 0 { 6 } else { 7 },
            },
            &body,
        )
        .expect("XIGrabDevice through process_request");
    assert_no_writer_failures(fixture);
    outcome
}

fn xi_ungrab_device(fixture: &mut NotifyFixture, deviceid: u16, sequence: u16) -> RequestOutcome {
    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&0_u32.to_le_bytes()); // CurrentTime
    body.extend_from_slice(&deviceid.to_le_bytes());
    body.extend_from_slice(&0_u16.to_le_bytes());
    let outcome = fixture
        .request(
            NOTIFY_RECEIVER_DENIED,
            SequenceNumber(sequence),
            RequestHeader {
                opcode: 137,
                data: 52,
                length_units: 3,
            },
            &body,
        )
        .expect("XIUngrabDevice through process_request");
    assert_no_writer_failures(fixture);
    outcome
}

fn literal_xi_grab_reply(order: ClientByteOrder, sequence: u16, status: u8) -> Vec<u8> {
    let mut reply = literal_event_header(1, 0, sequence, order);
    append_event_u32(&mut reply, order, 0); // no reply payload
    reply.push(status);
    reply.resize(32, 0);
    reply
}

fn literal_xi2_pointer_grab_chain(order: ClientByteOrder, sequence: u16, mode: u8) -> Vec<u8> {
    [
        literal_xi2_crossing(
            order,
            sequence,
            8,
            2,
            0,
            2,
            mode,
            2,
            ROOT_WINDOW,
            0,
            0,
            0,
            0,
            true,
        )
        .as_slice(),
        literal_xi2_crossing(
            order,
            sequence,
            7,
            2,
            0,
            2,
            mode,
            1,
            CROSS_PARENT,
            0,
            0,
            -20,
            -10,
            true,
        )
        .as_slice(),
        literal_xi2_crossing(
            order,
            sequence,
            7,
            2,
            0,
            2,
            mode,
            0,
            CROSS_CHILD,
            0,
            0,
            -30,
            -15,
            true,
        )
        .as_slice(),
    ]
    .concat()
}

fn literal_xi2_pointer_ungrab_chain(order: ClientByteOrder, sequence: u16) -> Vec<u8> {
    [
        literal_xi2_crossing(
            order,
            sequence,
            8,
            2,
            0,
            2,
            2,
            0,
            CROSS_CHILD,
            0,
            0,
            -30,
            -15,
            true,
        )
        .as_slice(),
        literal_xi2_crossing(
            order,
            sequence,
            8,
            2,
            0,
            2,
            2,
            1,
            CROSS_PARENT,
            0,
            0,
            -20,
            -10,
            true,
        )
        .as_slice(),
        literal_xi2_crossing(
            order,
            sequence,
            7,
            2,
            0,
            2,
            2,
            2,
            ROOT_WINDOW,
            0,
            0,
            0,
            0,
            true,
        )
        .as_slice(),
    ]
    .concat()
}

fn literal_xi2_keyboard_focus(
    order: ClientByteOrder,
    sequence: u16,
    evtype: u16,
    deviceid: u16,
    mode: u8,
    window: ResourceId,
) -> Vec<u8> {
    let (event_x, event_y) = if window == CROSS_CHILD {
        (-30, -15)
    } else {
        (-20, -10)
    };
    literal_xi2_crossing(
        order, sequence, evtype, deviceid, 0, deviceid, mode, 3, window, 0, 0, event_x, event_y,
        false,
    )
}

fn literal_xi2_keyboard_key_press(
    order: ClientByteOrder,
    sequence: u16,
    deviceid: u16,
    sourceid: u16,
) -> Vec<u8> {
    literal_xi2_device_press(
        order,
        sequence,
        2, // XI_KeyPress
        deviceid,
        sourceid,
        0x3579,
        38, // key detail
        CROSS_PARENT,
        CROSS_START_X,
        CROSS_START_Y,
        CROSS_START_X - 20,
        CROSS_START_Y - 10,
    )
}

fn xi_grab_event_mask(deviceid: u16) -> u32 {
    if deviceid == crate::xinput::DEVICEID_MASTER_POINTER {
        1 << 4 // XI_ButtonPress
    } else {
        1 << 2 // XI_KeyPress
    }
}

fn drive_unaffected_xi_grab_event(fixture: &mut NotifyFixture, deviceid: u16) {
    if deviceid == crate::xinput::DEVICEID_MASTER_POINTER {
        let mut xid_map = HostXidMap::new();
        xid_map.insert(CROSS_CHILD_XID, CROSS_CHILD);
        feed_core_pointer_event(
            fixture,
            &xid_map,
            core_pointer_event(
                PointerEventKind::ButtonPress,
                CROSS_CHILD_XID,
                1,
                0x3579,
                CROSS_X,
                CROSS_Y,
            ),
        );
    } else {
        let dropped = crate::core_loop::key_fanout::key_event_fanout_to_state(
            &mut fixture.state,
            &mut fixture.backend,
            HostKeyEvent {
                origin: InputOrigin::NestedHost,
                pressed: true,
                keycode: 38,
                time: 0x3579,
                root_x: CROSS_START_X,
                root_y: CROSS_START_Y,
                event_x: CROSS_START_X,
                event_y: CROSS_START_Y,
                state: 0,
            },
        );
        assert!(dropped.is_empty(), "key fanout has no disconnected clients");
        assert_no_writer_failures(fixture);
    }
}

fn literal_unaffected_xi_grab_event(
    order: ClientByteOrder,
    sequence: u16,
    deviceid: u16,
) -> Vec<u8> {
    if deviceid == crate::xinput::DEVICEID_MASTER_POINTER {
        literal_xi2_device_press(
            order,
            sequence,
            4, // XI_ButtonPress
            crate::xinput::DEVICEID_MASTER_POINTER,
            4, // XTEST pointer source
            0x3579,
            1, // button detail
            CROSS_CHILD,
            CROSS_X,
            CROSS_Y,
            5,
            5,
        )
    } else {
        literal_xi2_keyboard_key_press(
            order,
            sequence,
            crate::xinput::DEVICEID_MASTER_KEYBOARD,
            crate::xinput::DEVICEID_MASTER_KEYBOARD,
        )
    }
}

fn assert_xi_grab_device_baseline(deviceid: u16) {
    let mut fixture = xi2_grab_fixture();
    assert!(!fixture.state.policies_installed());
    let is_pointer = deviceid == crate::xinput::DEVICEID_MASTER_POINTER;
    let grab_mask = xi_grab_event_mask(deviceid);
    assert!(matches!(
        xi_grab_device(
            &mut fixture,
            deviceid,
            CROSS_CHILD,
            XI_GRAB_SEQUENCE,
            grab_mask,
        ),
        RequestOutcome::Handled
    ));
    let active = if is_pointer {
        fixture.state.active_pointer_grab.is_some()
    } else {
        fixture.state.active_keyboard_grab.is_some()
    };
    assert!(active, "XIGrabDevice activates its master device grab");
    let activation = if is_pointer {
        literal_xi2_pointer_grab_chain(ClientByteOrder::BigEndian, XI_GRAB_SEQUENCE, 1)
    } else {
        literal_xi2_keyboard_focus(
            ClientByteOrder::BigEndian,
            XI_GRAB_SEQUENCE,
            9,
            deviceid,
            1,
            CROSS_CHILD,
        )
    };
    assert_eq!(
        fixture.drain(NOTIFY_RECEIVER_DENIED),
        [
            activation.as_slice(),
            literal_xi_grab_reply(ClientByteOrder::BigEndian, XI_GRAB_SEQUENCE, 0).as_slice(),
        ]
        .concat(),
        "XIGrabDevice activation notification precedes its successful reply"
    );

    if is_pointer {
        drive_unaffected_xi_grab_event(&mut fixture, deviceid);
        assert_eq!(
            fixture.drain(NOTIFY_RECEIVER_DENIED),
            literal_unaffected_xi_grab_event(
                ClientByteOrder::BigEndian,
                XI_GRAB_SEQUENCE,
                deviceid,
            ),
            "XI_ButtonPress remains unchanged while the pointer grab is active"
        );
    } else {
        assert!(matches!(
            xi_grab_device(
                &mut fixture,
                deviceid,
                CROSS_PARENT,
                XI_REGRAB_SEQUENCE,
                grab_mask,
            ),
            RequestOutcome::Handled
        ));
        assert_eq!(
            fixture
                .state
                .active_keyboard_grab
                .map(|grab| grab.grab_window),
            Some(CROSS_PARENT),
            "keyboard re-grab replaces the grab window"
        );
        assert_eq!(
            fixture.drain(NOTIFY_RECEIVER_DENIED),
            [
                literal_xi2_keyboard_focus(
                    ClientByteOrder::BigEndian,
                    XI_REGRAB_SEQUENCE,
                    10,
                    deviceid,
                    1,
                    CROSS_CHILD,
                )
                .as_slice(),
                literal_xi2_keyboard_focus(
                    ClientByteOrder::BigEndian,
                    XI_REGRAB_SEQUENCE,
                    9,
                    deviceid,
                    1,
                    CROSS_PARENT,
                )
                .as_slice(),
                literal_xi_grab_reply(ClientByteOrder::BigEndian, XI_REGRAB_SEQUENCE, 0,)
                    .as_slice(),
            ]
            .concat(),
            "keyboard re-grab sends previous FocusOut then new FocusIn"
        );
        drive_unaffected_xi_grab_event(&mut fixture, deviceid);
        assert_eq!(
            fixture.drain(NOTIFY_RECEIVER_DENIED),
            literal_unaffected_xi_grab_event(
                ClientByteOrder::BigEndian,
                XI_REGRAB_SEQUENCE,
                deviceid,
            ),
            "XI_KeyPress remains unchanged while the re-grab is active"
        );
    }

    assert!(matches!(
        xi_ungrab_device(&mut fixture, deviceid, XI_UNGRAB_SEQUENCE),
        RequestOutcome::Handled
    ));
    let active = if is_pointer {
        fixture.state.active_pointer_grab.is_some()
    } else {
        fixture.state.active_keyboard_grab.is_some()
    };
    assert!(!active, "XIUngrabDevice releases its master device grab");
    let deactivation = if is_pointer {
        literal_xi2_pointer_ungrab_chain(ClientByteOrder::BigEndian, XI_UNGRAB_SEQUENCE)
    } else {
        literal_xi2_keyboard_focus(
            ClientByteOrder::BigEndian,
            XI_UNGRAB_SEQUENCE,
            10,
            deviceid,
            2,
            CROSS_PARENT,
        )
    };
    assert_eq!(fixture.drain(NOTIFY_RECEIVER_DENIED), deactivation);
    assert!(fixture.drain(NOTIFY_SENDER).is_empty());
    assert!(fixture.drain(NOTIFY_RECEIVER_ALLOWED).is_empty());
    assert_no_writer_failures(&fixture);
}

// Kept focused: byte order, sequence numbers, pointer/keyboard grabs, and ungrabs are checkpointed across requests.
// mutation: drop the focus flag from the XI2 crossing encoder call (C09)
// mutation: swap the high and low bytes of a client sequence number (C10)
// mutation: skip keyboard FocusOut when re-grabbing another window (C11)
#[test]
fn receive_xi2_crossing_baseline_frames() {
    let (mut fixture, xid_map) = xi2_crossing_fixture();
    assert!(!fixture.state.policies_installed());
    run_explicit_parent_child_crossings(&mut fixture, &xid_map);
    for (client, order, sequence) in [
        (
            NOTIFY_RECEIVER_DENIED,
            ClientByteOrder::BigEndian,
            DUAL_SEQUENCE,
        ),
        (
            NOTIFY_RECEIVER_ALLOWED,
            ClientByteOrder::LittleEndian,
            ROOT_SEQUENCE,
        ),
    ] {
        assert_eq!(
            fixture.drain(client),
            literal_xi2_explicit_crossings(order, sequence),
            "XI2 Leave/Enter followed by the core MotionNotify stream"
        );
    }
    assert_eq!(fixture.state.master_sprite_root, (CROSS_X, CROSS_Y));

    assert!(matches!(
        grab_pointer(
            &mut fixture,
            NOTIFY_SENDER,
            false,
            CROSS_PARENT,
            0,
            GRAB_SEQUENCE,
        ),
        RequestOutcome::Handled
    ));
    assert!(fixture.state.active_pointer_grab.is_some());
    assert_eq!(
        fixture.drain(NOTIFY_SENDER),
        literal_grab_reply(ClientByteOrder::LittleEndian, GRAB_SEQUENCE, 0)
    );
    for (client, order, sequence) in [
        (
            NOTIFY_RECEIVER_DENIED,
            ClientByteOrder::BigEndian,
            DUAL_SEQUENCE,
        ),
        (
            NOTIFY_RECEIVER_ALLOWED,
            ClientByteOrder::LittleEndian,
            ROOT_SEQUENCE,
        ),
    ] {
        assert_eq!(
            fixture.drain(client),
            [
                grab_chain_child_to_parent(order, sequence, 1).as_slice(),
                literal_xi2_grab_chain(order, sequence, 1).as_slice(),
            ]
            .concat(),
            "core and XI2 NotifyGrab chains preserve their literal order"
        );
    }
    assert!(matches!(
        ungrab_pointer(&mut fixture, NOTIFY_SENDER, UNGRAB_SEQUENCE),
        RequestOutcome::Handled
    ));
    assert!(fixture.state.active_pointer_grab.is_none());
    assert!(
        fixture.drain(NOTIFY_SENDER).is_empty(),
        "XI2 baseline UngrabPointer requester stream is empty"
    );
    for (client, order, sequence) in [
        (
            NOTIFY_RECEIVER_DENIED,
            ClientByteOrder::BigEndian,
            DUAL_SEQUENCE,
        ),
        (
            NOTIFY_RECEIVER_ALLOWED,
            ClientByteOrder::LittleEndian,
            ROOT_SEQUENCE,
        ),
    ] {
        assert_eq!(
            fixture.drain(client),
            [
                grab_chain_parent_to_child(order, sequence, 2).as_slice(),
                literal_xi2_ungrab_pointer_chain(order, sequence, 2).as_slice(),
            ]
            .concat(),
            "core and XI2 NotifyUngrab chains preserve their literal order"
        );
    }
    assert_no_writer_failures(&fixture);

    assert_xi_grab_device_baseline(crate::xinput::DEVICEID_MASTER_POINTER);
    assert_xi_grab_device_baseline(crate::xinput::DEVICEID_MASTER_KEYBOARD);
}

// mutation: when the dual selector's XI2 copy is denied, deliver its core copy (C12)
#[test]
fn receive_xi2_crossing_denial_keeps_core_dedup() {
    let build = || {
        let (mut fixture, xid_map) = core_crossing_fixture();
        fixture
            .state
            .clients
            .get_mut(&NOTIFY_RECEIVER_DENIED.0)
            .unwrap()
            .event_masks
            .insert(CROSS_CHILD, ENTER_LEAVE_MASK);
        fixture
            .state
            .clients
            .get_mut(&NOTIFY_RECEIVER_ALLOWED.0)
            .unwrap()
            .event_masks
            .clear();
        for client in [NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED] {
            fixture
                .state
                .clients
                .get_mut(&client.0)
                .unwrap()
                .xi2_masks
                .insert(
                    (CROSS_CHILD, crate::xinput::DEVICEID_MASTER_POINTER),
                    1 << 7,
                );
        }
        (fixture, xid_map)
    };
    let act = |fixture: &mut NotifyFixture,
               xid_map: &HostXidMap,
               _: &std::sync::Mutex<Vec<ExpectedReceive>>| {
        feed_core_pointer_event(
            fixture,
            xid_map,
            core_pointer_event(
                PointerEventKind::EnterNotify,
                CROSS_CHILD_XID,
                0,
                0x2468,
                CROSS_X,
                CROSS_Y,
            ),
        );
        feed_core_pointer_event(
            fixture,
            xid_map,
            core_pointer_event(
                PointerEventKind::MotionNotify,
                CROSS_CHILD_XID,
                0,
                0x2469,
                CROSS_X,
                CROSS_Y,
            ),
        );
        assert_eq!(fixture.state.master_sprite_root, (CROSS_X, CROSS_Y));
        assert_eq!(fixture.state.pointer_root, (CROSS_X, CROSS_Y));
        assert_no_writer_failures(fixture);
    };
    let expect = |_: &HostXidMap| {
        let receive = |client| ExpectedReceive {
            client,
            request: None,
            subject: EventSubject::Window(CROSS_CHILD),
            events: vec![EventDescription::extension(35, 137, 7)],
        };
        let denied = receive(NOTIFY_RECEIVER_DENIED);
        let mut expectation = CaseExpect::new(
            vec![
                Stream::unchanged(NOTIFY_SENDER, Vec::new()),
                Stream {
                    client: NOTIFY_RECEIVER_DENIED,
                    bytes: literal_xi2_crossing(
                        ClientByteOrder::BigEndian,
                        DUAL_SEQUENCE,
                        7,
                        2,
                        0x2468,
                        4,
                        0,
                        0,
                        CROSS_CHILD,
                        CROSS_X,
                        CROSS_Y,
                        5,
                        5,
                        true,
                    ),
                    denied: Some(Vec::new()),
                },
                Stream::unchanged(
                    NOTIFY_RECEIVER_ALLOWED,
                    literal_xi2_crossing(
                        ClientByteOrder::LittleEndian,
                        ROOT_SEQUENCE,
                        7,
                        2,
                        0x2468,
                        4,
                        0,
                        0,
                        CROSS_CHILD,
                        CROSS_X,
                        CROSS_Y,
                        5,
                        5,
                        true,
                    ),
                ),
            ],
            vec![denied.clone(), receive(NOTIFY_RECEIVER_ALLOWED)],
            CallOrder::Multiset,
            vec![denied],
        );
        expectation.deny_before_allow = true;
        expectation.after_deny = Some(Box::new(|allow, denied| {
            assert_eq!(
                denied.state.master_sprite_root,
                allow.state.master_sprite_root
            );
            assert_eq!(denied.state.pointer_root, allow.state.pointer_root);
        }));
        expectation
    };
    check_receive_case(&ReceiveCase {
        name: "denying XI2 Enter keeps the shadowed core Enter suppressed",
        build: &build,
        act: &act,
        expect: &expect,
    });
}

// Kept focused: this case checks grab reply, active state, and both crossing streams before ungrab.
// mutation: leave C4 unchecked (C13)
#[test]
fn receive_xi2_grab_chain_denial() {
    let make_fixture = || {
        let (mut fixture, _) = xi2_crossing_fixture();
        for client in [NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED] {
            fixture
                .state
                .clients
                .get_mut(&client.0)
                .unwrap()
                .event_masks
                .clear();
        }
        fixture
    };
    let (allowed, allowed_calls) = {
        let mut fixture = make_fixture();
        let calls = fixture.install_policy(|_, _| Decision::Allow);
        fixture.state.pointer_root = (CROSS_X, CROSS_Y);
        fixture.state.master_sprite_root = (CROSS_X, CROSS_Y);
        assert!(matches!(
            grab_pointer(
                &mut fixture,
                NOTIFY_SENDER,
                false,
                CROSS_PARENT,
                0,
                GRAB_SEQUENCE,
            ),
            RequestOutcome::Handled
        ));
        assert!(fixture.state.active_pointer_grab.is_some());
        assert_eq!(
            fixture.drain(NOTIFY_SENDER),
            literal_grab_reply(ClientByteOrder::LittleEndian, GRAB_SEQUENCE, 0)
        );
        for (client, order, sequence) in [
            (
                NOTIFY_RECEIVER_DENIED,
                ClientByteOrder::BigEndian,
                DUAL_SEQUENCE,
            ),
            (
                NOTIFY_RECEIVER_ALLOWED,
                ClientByteOrder::LittleEndian,
                ROOT_SEQUENCE,
            ),
        ] {
            assert_eq!(
                fixture.drain(client),
                literal_xi2_grab_chain(order, sequence, 1),
                "allow-control NotifyGrab chain"
            );
        }
        assert!(matches!(
            ungrab_pointer(&mut fixture, NOTIFY_SENDER, UNGRAB_SEQUENCE),
            RequestOutcome::Handled
        ));
        assert!(fixture.state.active_pointer_grab.is_none());
        assert!(fixture.drain(NOTIFY_SENDER).is_empty());
        for (client, order, sequence) in [
            (
                NOTIFY_RECEIVER_DENIED,
                ClientByteOrder::BigEndian,
                DUAL_SEQUENCE,
            ),
            (
                NOTIFY_RECEIVER_ALLOWED,
                ClientByteOrder::LittleEndian,
                ROOT_SEQUENCE,
            ),
        ] {
            assert_eq!(
                fixture.drain(client),
                literal_xi2_ungrab_pointer_chain(order, sequence, 2),
                "allow-control NotifyUngrab chain"
            );
        }
        (fixture, calls)
    };
    assert_no_writer_failures(&allowed);

    let mut denied = make_fixture();
    let calls = denied.install_policy(|_, call| {
        if call.hook == ExtensionHook::Receive
            && call.client == NOTIFY_RECEIVER_DENIED
            && call.receive == Some(EventSubject::Window(CROSS_CHILD))
            && call.events.as_ref().is_some_and(|events| {
                events.as_slice() == [EventDescription::extension(35, 137, 7)]
                    || events.as_slice() == [EventDescription::extension(35, 137, 8)]
            })
        {
            Decision::Deny(DENY)
        } else {
            Decision::Allow
        }
    });
    denied.state.pointer_root = (CROSS_X, CROSS_Y);
    denied.state.master_sprite_root = (CROSS_X, CROSS_Y);
    assert!(matches!(
        grab_pointer(
            &mut denied,
            NOTIFY_SENDER,
            false,
            CROSS_PARENT,
            0,
            GRAB_SEQUENCE,
        ),
        RequestOutcome::Handled
    ));
    assert!(denied.state.active_pointer_grab.is_some());
    assert_eq!(
        denied.drain(NOTIFY_SENDER),
        literal_grab_reply(ClientByteOrder::LittleEndian, GRAB_SEQUENCE, 0),
        "denied NotifyGrab does not change the successful reply"
    );
    assert_eq!(
        denied.drain(NOTIFY_RECEIVER_DENIED),
        literal_xi2_crossing(
            ClientByteOrder::BigEndian,
            DUAL_SEQUENCE,
            7,
            2,
            0,
            2,
            1,
            2,
            CROSS_PARENT,
            CROSS_X,
            CROSS_Y,
            15,
            10,
            true,
        ),
        "denied child Leave leaves the parent Enter"
    );
    assert_eq!(
        denied.drain(NOTIFY_RECEIVER_ALLOWED),
        literal_xi2_grab_chain(ClientByteOrder::LittleEndian, ROOT_SEQUENCE, 1)
    );
    assert!(matches!(
        ungrab_pointer(&mut denied, NOTIFY_SENDER, UNGRAB_SEQUENCE),
        RequestOutcome::Handled
    ));
    assert!(denied.state.active_pointer_grab.is_none());
    assert!(denied.drain(NOTIFY_SENDER).is_empty());
    assert_eq!(
        denied.drain(NOTIFY_RECEIVER_DENIED),
        literal_xi2_crossing(
            ClientByteOrder::BigEndian,
            DUAL_SEQUENCE,
            8,
            2,
            0,
            2,
            2,
            2,
            CROSS_PARENT,
            CROSS_X,
            CROSS_Y,
            15,
            10,
            true,
        ),
        "denied child Enter leaves the parent Leave"
    );
    assert_eq!(
        denied.drain(NOTIFY_RECEIVER_ALLOWED),
        literal_xi2_ungrab_pointer_chain(ClientByteOrder::LittleEndian, ROOT_SEQUENCE, 2)
    );
    let expected_calls = [
        (
            NOTIFY_RECEIVER_DENIED,
            CROSS_CHILD,
            8,
            request_opcodes(26, 0),
        ),
        (
            NOTIFY_RECEIVER_ALLOWED,
            CROSS_CHILD,
            8,
            request_opcodes(26, 0),
        ),
        (
            NOTIFY_RECEIVER_DENIED,
            CROSS_PARENT,
            7,
            request_opcodes(26, 0),
        ),
        (
            NOTIFY_RECEIVER_ALLOWED,
            CROSS_PARENT,
            7,
            request_opcodes(26, 0),
        ),
        (
            NOTIFY_RECEIVER_DENIED,
            CROSS_PARENT,
            8,
            request_opcodes(27, 0),
        ),
        (
            NOTIFY_RECEIVER_ALLOWED,
            CROSS_PARENT,
            8,
            request_opcodes(27, 0),
        ),
        (
            NOTIFY_RECEIVER_DENIED,
            CROSS_CHILD,
            7,
            request_opcodes(27, 0),
        ),
        (
            NOTIFY_RECEIVER_ALLOWED,
            CROSS_CHILD,
            7,
            request_opcodes(27, 0),
        ),
    ];
    let expected_receives = expected_calls
        .iter()
        .map(|(client, window, evtype, request)| ExpectedReceive {
            client: *client,
            request: *request,
            subject: EventSubject::Window(*window),
            events: vec![EventDescription::extension(35, 137, *evtype)],
        })
        .collect::<Vec<_>>();
    let deny_log = calls.lock().unwrap();
    assert_receive_trace(
        "XI2 grab-chain denial",
        "deny",
        CallOrder::Multiset,
        &expected_receives,
        &deny_log,
        "Receive trace",
    );
    assert_forbidden_hooks(
        "XI2 grab-chain denial",
        "deny",
        &deny_log,
        &[ExtensionHook::Send],
    );
    let allow_log = allowed_calls.lock().unwrap();
    assert_receive_trace(
        "XI2 grab-chain denial",
        "allow-all",
        CallOrder::Multiset,
        &expected_receives,
        &allow_log,
        "Receive trace",
    );
    assert_forbidden_hooks(
        "XI2 grab-chain denial",
        "allow-all",
        &allow_log,
        &[ExtensionHook::Send],
    );
    assert_no_writer_failures(&denied);
}

// mutation: leave the XIUngrabDevice site unchecked (C14)
// mutation: omit keyboard FocusOut when re-grabbing another window (C15)
// mutation: suppress a selected XI2 button/key event during an active grab (C16)
// Focused multi-request scenario: keeps activation, re-grab, input, and ungrab checkpoints.
#[test]
fn receive_xi_grab_device_synthetic_denial() {
    let run_case = |deviceid: u16| {
        let is_pointer = deviceid == crate::xinput::DEVICEID_MASTER_POINTER;
        let grab_mask = xi_grab_event_mask(deviceid);
        let expected_calls = if is_pointer {
            vec![
                (
                    NOTIFY_RECEIVER_DENIED,
                    ROOT_WINDOW,
                    8,
                    request_opcodes(137, 51),
                ),
                (
                    NOTIFY_RECEIVER_DENIED,
                    CROSS_PARENT,
                    7,
                    request_opcodes(137, 51),
                ),
                (
                    NOTIFY_RECEIVER_DENIED,
                    CROSS_CHILD,
                    7,
                    request_opcodes(137, 51),
                ),
                (
                    NOTIFY_RECEIVER_DENIED,
                    CROSS_CHILD,
                    8,
                    request_opcodes(137, 52),
                ),
                (
                    NOTIFY_RECEIVER_DENIED,
                    CROSS_PARENT,
                    8,
                    request_opcodes(137, 52),
                ),
                (
                    NOTIFY_RECEIVER_DENIED,
                    ROOT_WINDOW,
                    7,
                    request_opcodes(137, 52),
                ),
            ]
        } else {
            vec![
                (
                    NOTIFY_RECEIVER_DENIED,
                    CROSS_CHILD,
                    9,
                    request_opcodes(137, 51),
                ),
                (
                    NOTIFY_RECEIVER_DENIED,
                    CROSS_CHILD,
                    10,
                    request_opcodes(137, 51),
                ),
                (
                    NOTIFY_RECEIVER_DENIED,
                    CROSS_PARENT,
                    9,
                    request_opcodes(137, 51),
                ),
                (
                    NOTIFY_RECEIVER_DENIED,
                    CROSS_PARENT,
                    10,
                    request_opcodes(137, 52),
                ),
            ]
        };
        let mut allowed = xi2_grab_fixture();
        let allow_calls = allowed.install_policy(|_, _| Decision::Allow);
        assert!(matches!(
            xi_grab_device(
                &mut allowed,
                deviceid,
                CROSS_CHILD,
                XI_GRAB_SEQUENCE,
                grab_mask,
            ),
            RequestOutcome::Handled
        ));
        let allow_activation = if is_pointer {
            literal_xi2_pointer_grab_chain(ClientByteOrder::BigEndian, XI_GRAB_SEQUENCE, 1)
        } else {
            literal_xi2_keyboard_focus(
                ClientByteOrder::BigEndian,
                XI_GRAB_SEQUENCE,
                9,
                deviceid,
                1,
                CROSS_CHILD,
            )
        };
        assert_eq!(
            allowed.drain(NOTIFY_RECEIVER_DENIED),
            [
                allow_activation.as_slice(),
                literal_xi_grab_reply(ClientByteOrder::BigEndian, XI_GRAB_SEQUENCE, 0).as_slice(),
            ]
            .concat(),
            "allow-control successful grab keeps its synthetic activation"
        );
        assert!(allowed.drain(NOTIFY_RECEIVER_ALLOWED).is_empty());
        let allowed_initial_grab_state = if is_pointer {
            allowed
                .state
                .active_pointer_grab
                .map(|grab| (grab.owner, grab.grab_window))
        } else {
            allowed
                .state
                .active_keyboard_grab
                .map(|grab| (grab.owner, grab.grab_window))
        };
        assert_eq!(
            allowed_initial_grab_state,
            Some((NOTIFY_RECEIVER_DENIED, CROSS_CHILD)),
            "allow-control initial XIGrabDevice state"
        );

        if !is_pointer {
            assert!(matches!(
                xi_grab_device(
                    &mut allowed,
                    deviceid,
                    CROSS_PARENT,
                    XI_REGRAB_SEQUENCE,
                    grab_mask,
                ),
                RequestOutcome::Handled
            ));
            assert_eq!(
                allowed
                    .state
                    .active_keyboard_grab
                    .map(|grab| grab.grab_window),
                Some(CROSS_PARENT),
                "allow-control keyboard re-grab targets the new window"
            );
            assert_eq!(
                allowed.drain(NOTIFY_RECEIVER_DENIED),
                [
                    literal_xi2_keyboard_focus(
                        ClientByteOrder::BigEndian,
                        XI_REGRAB_SEQUENCE,
                        10,
                        deviceid,
                        1,
                        CROSS_CHILD,
                    )
                    .as_slice(),
                    literal_xi2_keyboard_focus(
                        ClientByteOrder::BigEndian,
                        XI_REGRAB_SEQUENCE,
                        9,
                        deviceid,
                        1,
                        CROSS_PARENT,
                    )
                    .as_slice(),
                    literal_xi_grab_reply(ClientByteOrder::BigEndian, XI_REGRAB_SEQUENCE, 0,)
                        .as_slice(),
                ]
                .concat(),
                "allow-control re-grab pins old FocusOut before new FocusIn"
            );
            assert!(allowed.drain(NOTIFY_RECEIVER_ALLOWED).is_empty());
        }
        let allowed_regrab_state = if is_pointer {
            allowed
                .state
                .active_pointer_grab
                .map(|grab| (grab.owner, grab.grab_window))
        } else {
            allowed
                .state
                .active_keyboard_grab
                .map(|grab| (grab.owner, grab.grab_window))
        };
        assert_eq!(
            allowed_regrab_state,
            Some((
                NOTIFY_RECEIVER_DENIED,
                if is_pointer {
                    CROSS_CHILD
                } else {
                    CROSS_PARENT
                },
            )),
            "allow-control active XIGrabDevice state after the re-grab step"
        );
        let event_sequence = if is_pointer {
            XI_GRAB_SEQUENCE
        } else {
            XI_REGRAB_SEQUENCE
        };
        drive_unaffected_xi_grab_event(&mut allowed, deviceid);
        assert_eq!(
            allowed.drain(NOTIFY_RECEIVER_DENIED),
            literal_unaffected_xi_grab_event(ClientByteOrder::BigEndian, event_sequence, deviceid,),
            "selected XI2 input event stays unchanged during the active grab"
        );
        assert!(allowed.drain(NOTIFY_RECEIVER_ALLOWED).is_empty());

        assert!(matches!(
            xi_ungrab_device(&mut allowed, deviceid, XI_UNGRAB_SEQUENCE),
            RequestOutcome::Handled
        ));
        assert!(allowed.drain(NOTIFY_SENDER).is_empty());
        let allow_deactivation = if is_pointer {
            literal_xi2_pointer_ungrab_chain(ClientByteOrder::BigEndian, XI_UNGRAB_SEQUENCE)
        } else {
            literal_xi2_keyboard_focus(
                ClientByteOrder::BigEndian,
                XI_UNGRAB_SEQUENCE,
                10,
                deviceid,
                2,
                CROSS_PARENT,
            )
        };
        assert_eq!(allowed.drain(NOTIFY_RECEIVER_DENIED), allow_deactivation);
        assert!(
            allowed.state.active_pointer_grab.is_none()
                && allowed.state.active_keyboard_grab.is_none(),
            "allow-control ungrab releases the master grab"
        );
        assert!(allowed.drain(NOTIFY_RECEIVER_ALLOWED).is_empty());

        let mut denied = xi2_grab_fixture();
        let denied_pairs = if is_pointer {
            vec![
                (ROOT_WINDOW, 8),
                (CROSS_PARENT, 7),
                (CROSS_CHILD, 7),
                (CROSS_CHILD, 8),
                (CROSS_PARENT, 8),
                (ROOT_WINDOW, 7),
            ]
        } else {
            vec![
                (CROSS_CHILD, 9),
                (CROSS_CHILD, 10),
                (CROSS_PARENT, 9),
                (CROSS_PARENT, 10),
            ]
        };
        let calls = denied.install_policy(move |_, call| {
            if call.hook == ExtensionHook::Receive
                && call.client == NOTIFY_RECEIVER_DENIED
                && call.receive.is_some_and(|subject| match subject {
                    EventSubject::Window(window) => call.events.as_ref().is_some_and(|events| {
                        events.len() == 1
                            && denied_pairs.iter().any(|(expected_window, evtype)| {
                                *expected_window == window
                                    && events[0] == EventDescription::extension(35, 137, *evtype)
                            })
                    }),
                    _ => false,
                })
            {
                Decision::Deny(DENY)
            } else {
                Decision::Allow
            }
        });
        assert!(matches!(
            xi_grab_device(
                &mut denied,
                deviceid,
                CROSS_CHILD,
                XI_GRAB_SEQUENCE,
                grab_mask,
            ),
            RequestOutcome::Handled
        ));
        let active = if is_pointer {
            denied.state.active_pointer_grab.is_some()
        } else {
            denied.state.active_keyboard_grab.is_some()
        };
        assert!(active, "denial leaves the XIGrabDevice active");
        let denied_initial_grab_state = if is_pointer {
            denied
                .state
                .active_pointer_grab
                .map(|grab| (grab.owner, grab.grab_window))
        } else {
            denied
                .state
                .active_keyboard_grab
                .map(|grab| (grab.owner, grab.grab_window))
        };
        assert_eq!(
            allowed_initial_grab_state, denied_initial_grab_state,
            "Receive denial leaves the same initial XIGrabDevice state as Allow"
        );
        assert_eq!(
            denied_initial_grab_state,
            Some((NOTIFY_RECEIVER_DENIED, CROSS_CHILD)),
            "denied synthetic activation leaves the requested grab active"
        );
        assert_eq!(
            denied.drain(NOTIFY_RECEIVER_DENIED),
            literal_xi_grab_reply(ClientByteOrder::BigEndian, XI_GRAB_SEQUENCE, 0),
            "denied synthetic activation does not alter the Success reply"
        );
        assert!(denied.drain(NOTIFY_RECEIVER_ALLOWED).is_empty());

        if !is_pointer {
            assert!(matches!(
                xi_grab_device(
                    &mut denied,
                    deviceid,
                    CROSS_PARENT,
                    XI_REGRAB_SEQUENCE,
                    grab_mask,
                ),
                RequestOutcome::Handled
            ));
            assert_eq!(
                denied
                    .state
                    .active_keyboard_grab
                    .map(|grab| grab.grab_window),
                Some(CROSS_PARENT),
                "denied keyboard FocusOut and FocusIn keep the re-grab active"
            );
            assert_eq!(
                denied.drain(NOTIFY_RECEIVER_DENIED),
                literal_xi_grab_reply(ClientByteOrder::BigEndian, XI_REGRAB_SEQUENCE, 0,),
                "denied keyboard re-grab keeps its Success reply"
            );
            assert!(denied.drain(NOTIFY_RECEIVER_ALLOWED).is_empty());
            let denied_grab = denied
                .state
                .active_keyboard_grab
                .map(|grab| (grab.owner, grab.grab_window));
            assert_eq!(
                allowed_regrab_state, denied_grab,
                "denial keeps the same re-grab as the Allow run"
            );
        }

        drive_unaffected_xi_grab_event(&mut denied, deviceid);
        assert_eq!(
            denied.drain(NOTIFY_RECEIVER_DENIED),
            literal_unaffected_xi_grab_event(ClientByteOrder::BigEndian, event_sequence, deviceid,),
            "denying synthetic events leaves selected XI2 input unchanged"
        );
        assert!(denied.drain(NOTIFY_RECEIVER_ALLOWED).is_empty());
        assert!(matches!(
            xi_ungrab_device(&mut denied, deviceid, XI_UNGRAB_SEQUENCE),
            RequestOutcome::Handled
        ));
        let active = if is_pointer {
            denied.state.active_pointer_grab.is_some()
        } else {
            denied.state.active_keyboard_grab.is_some()
        };
        assert!(!active, "denial does not prevent XIUngrabDevice release");
        assert!(
            denied.drain(NOTIFY_RECEIVER_DENIED).is_empty(),
            "denied synthetic deactivation is absent"
        );
        assert!(denied.drain(NOTIFY_SENDER).is_empty());
        assert!(denied.drain(NOTIFY_RECEIVER_ALLOWED).is_empty());
        let expected_receives = expected_calls
            .iter()
            .map(|(client, window, evtype, request)| ExpectedReceive {
                client: *client,
                request: *request,
                subject: EventSubject::Window(*window),
                events: vec![EventDescription::extension(35, 137, *evtype)],
            })
            .collect::<Vec<_>>();
        let deny_log = calls.lock().unwrap();
        assert_receive_trace(
            "XI grab-device synthetic denial",
            "deny",
            CallOrder::Total,
            &expected_receives,
            &deny_log,
            "Receive trace",
        );
        assert_forbidden_hooks(
            "XI grab-device synthetic denial",
            "deny",
            &deny_log,
            &[ExtensionHook::Send],
        );
        let allow_log = allow_calls.lock().unwrap();
        assert_receive_trace(
            "XI grab-device synthetic denial",
            "allow-all",
            CallOrder::Total,
            &expected_receives,
            &allow_log,
            "Receive trace",
        );
        assert_forbidden_hooks(
            "XI grab-device synthetic denial",
            "allow-all",
            &allow_log,
            &[ExtensionHook::Send],
        );
        assert_no_writer_failures(&denied);
    };
    run_case(crate::xinput::DEVICEID_MASTER_POINTER);
    run_case(crate::xinput::DEVICEID_MASTER_KEYBOARD);
}

// Kept focused: this is the explicit no-Receive assertion for cooked core ButtonPress.
// mutation: check every kind in C1's writer (C17)
#[test]
fn receive_core_cooked_pointer_unchecked() {
    let (mut fixture, xid_map) = core_crossing_fixture();
    fixture
        .state
        .clients
        .get_mut(&NOTIFY_RECEIVER_ALLOWED.0)
        .unwrap()
        .event_masks
        .insert(CROSS_CHILD, BUTTON_PRESS_MASK);
    fixture
        .state
        .clients
        .get_mut(&NOTIFY_RECEIVER_DENIED.0)
        .unwrap()
        .xi2_masks
        .insert((CROSS_CHILD, 4), 1 << 4); // XI_ButtonPress on XTEST pointer
    fixture
        .state
        .clients
        .get_mut(&NOTIFY_RECEIVER_DENIED.0)
        .unwrap()
        .event_masks
        .insert(CROSS_CHILD, ENTER_LEAVE_MASK);
    let calls = fixture.install_policy(|_, call| {
        if call.hook == ExtensionHook::Receive {
            Decision::Deny(DENY)
        } else {
            Decision::Allow
        }
    });
    feed_core_pointer_event(
        &mut fixture,
        &xid_map,
        core_pointer_event(
            PointerEventKind::ButtonPress,
            CROSS_CHILD_XID,
            1,
            0x3579,
            CROSS_X,
            CROSS_Y,
        ),
    );
    assert_eq!(
        fixture.drain(NOTIFY_RECEIVER_ALLOWED),
        literal_core_button_press(
            ClientByteOrder::LittleEndian,
            ROOT_SEQUENCE,
            0x3579,
            CROSS_CHILD,
            CROSS_X,
            CROSS_Y,
            5,
            5,
        ),
        "core cooked ButtonPress remains unchecked"
    );
    assert_eq!(
        fixture.drain(NOTIFY_RECEIVER_DENIED),
        literal_xi2_device_press(
            ClientByteOrder::BigEndian,
            DUAL_SEQUENCE,
            4, // XI_ButtonPress
            4,
            4,
            0x3579,
            1, // button detail
            CROSS_CHILD,
            CROSS_X,
            CROSS_Y,
            5,
            5,
        ),
        "XI2 cooked ButtonPress remains unchecked"
    );
    assert!(receive_trace("cooked core pointer", "deny", &calls.lock().unwrap()).is_empty());
    assert_eq!(fixture.state.master_sprite_root, (CROSS_X, CROSS_Y));
    assert_no_writer_failures(&fixture);
}

const FOCUS_WINDOW: ResourceId = ResourceId(0x0052_0001);
const FOCUS_XI_SEQUENCE: u16 = 0x2345;
const FOCUS_OTHER_SEQUENCE: u16 = 0x4567;
const CORE_FOCUS_CHANGE_MASK: u32 = 0x0020_0000;
const XI_FOCUS_IN: u16 = 9;
const XI_FOCUS_OUT: u16 = 10;

fn focus_notify_fixture() -> (NotifyFixture, u16) {
    use yserver_protocol::x11::CreateWindowRequest;

    let mut fixture = NotifyFixture::new();
    fixture.state.start_instant = std::time::Instant::now() + std::time::Duration::from_secs(30);
    fixture.state.resources.create_window(
        NOTIFY_SENDER,
        CreateWindowRequest {
            depth: 24,
            window: FOCUS_WINDOW,
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 80,
            height: 60,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    assert!(
        fixture
            .state
            .resources
            .map_window(FOCUS_WINDOW)
            .mapping_changed
    );

    let pointer = fixture
        .state
        .xi_register_source(&crate::core_loop::DeviceInfo {
            source_id: crate::xinput::InputSourceId(0x5410),
            enabled: true,
            resume_key: None,
            capabilities: crate::xinput::InputCapabilities {
                pointer: true,
                ..Default::default()
            },
            name: "access-focus-four-axis-pointer".into(),
            device_node: "/dev/input/event-access-focus-pointer".into(),
            sysname: "event-access-focus-pointer".into(),
            vendor_id: 1,
            product_id: 4,
            is_touchpad: false,
            config: Default::default(),
        })
        .into_iter()
        .find(|device| {
            fixture
                .state
                .xi_devices
                .device(*device)
                .is_some_and(|entry| entry.facet == Some(crate::xinput::XiFacetKind::PointerTouch))
        })
        .expect("physical pointer facet");
    assert_eq!(
        fixture
            .state
            .xi_devices
            .device(pointer)
            .expect("registered physical pointer")
            .class_shape
            .valuator_count(),
        4,
        "the physical pointer fixture carries four valuators"
    );

    let focus_in_class = (u32::from(crate::xinput::DEVICEID_XTEST_KEYBOARD) << 8)
        | u32::from(crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_FOCUS_IN_OFFSET);
    let state_class = (u32::from(crate::xinput::DEVICEID_XTEST_KEYBOARD) << 8)
        | u32::from(crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_STATE_NOTIFY_OFFSET);
    let pointer_focus_in_class = (u32::from(pointer) << 8)
        | u32::from(crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_FOCUS_IN_OFFSET);
    let pointer_state_class = (u32::from(pointer) << 8)
        | u32::from(crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_STATE_NOTIFY_OFFSET);
    for (client, sequence, order) in [
        (
            NOTIFY_RECEIVER_DENIED,
            FOCUS_XI_SEQUENCE,
            ClientByteOrder::BigEndian,
        ),
        (
            NOTIFY_RECEIVER_ALLOWED,
            FOCUS_OTHER_SEQUENCE,
            ClientByteOrder::LittleEndian,
        ),
    ] {
        let state = fixture.state.clients.get_mut(&client.0).expect("client");
        state.byte_order = order;
        state.last_sequence.store(sequence, Ordering::Relaxed);
        state
            .event_masks
            .insert(FOCUS_WINDOW, CORE_FOCUS_CHANGE_MASK);
        state.xi2_masks.insert(
            (FOCUS_WINDOW, crate::xinput::DEVICEID_MASTER_KEYBOARD),
            1 << XI_FOCUS_IN,
        );
        state
            .xi1_window_event_classes
            .entry(FOCUS_WINDOW)
            .or_default()
            .extend([
                focus_in_class,
                state_class,
                pointer_focus_in_class,
                pointer_state_class,
            ]);
    }
    (fixture, pointer)
}

fn request_core_focus(fixture: &mut NotifyFixture) -> RequestOutcome {
    fixture
        .request(
            NOTIFY_SENDER,
            SequenceNumber(51),
            RequestHeader {
                opcode: 42,
                data: 0,
                length_units: 3,
            },
            &[FOCUS_WINDOW.0.to_le_bytes(), 0u32.to_le_bytes()].concat(),
        )
        .expect("SetInputFocus through process_request")
}

fn request_xi1_device_focus(
    fixture: &mut NotifyFixture,
    sequence: u16,
    device: u16,
) -> RequestOutcome {
    let mut body = Vec::with_capacity(12);
    body.extend_from_slice(&FOCUS_WINDOW.0.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    body.push(0); // RevertToNone
    body.push(u8::try_from(device).expect("XI1 device id fits CARD8"));
    body.extend_from_slice(&[0, 0]);
    fixture
        .request(
            NOTIFY_SENDER,
            SequenceNumber(sequence),
            RequestHeader {
                opcode: 137,
                data: 21, // XI1 SetDeviceFocus
                length_units: 4,
            },
            &body,
        )
        .expect("SetDeviceFocus through process_request")
}

fn literal_core_focus_in(order: ClientByteOrder, sequence: u16) -> Vec<u8> {
    let mut event = literal_event_header(9, 3, sequence, order); // FocusIn, NotifyNonlinear
    append_event_u32(&mut event, order, FOCUS_WINDOW.0);
    event.push(0); // NotifyNormal
    finish_literal_event(event)
}

fn literal_core_focus_out_pointer(order: ClientByteOrder, sequence: u16) -> Vec<u8> {
    let mut event = literal_event_header(10, 5, sequence, order); // FocusOut, NotifyPointer
    append_event_u32(&mut event, order, FOCUS_WINDOW.0);
    event.push(0); // NotifyNormal
    finish_literal_event(event)
}

fn literal_xi2_focus_event(
    order: ClientByteOrder,
    sequence: u16,
    evtype: u16,
    detail: u8,
) -> Vec<u8> {
    literal_xi2_crossing(
        order,
        sequence,
        evtype,
        crate::xinput::DEVICEID_MASTER_KEYBOARD,
        0,
        crate::xinput::DEVICEID_MASTER_KEYBOARD,
        0,
        detail,
        FOCUS_WINDOW,
        0,
        0,
        0,
        0,
        false,
    )
}

fn literal_xi2_focus_in(order: ClientByteOrder, sequence: u16) -> Vec<u8> {
    literal_xi2_focus_event(order, sequence, XI_FOCUS_IN, 3) // NotifyNonlinear
}

fn literal_xi2_focus_out(order: ClientByteOrder, sequence: u16) -> Vec<u8> {
    literal_xi2_focus_event(order, sequence, XI_FOCUS_OUT, 5) // NotifyPointer
}

fn literal_xi1_device_focus_event(
    order: ClientByteOrder,
    sequence: u16,
    device: u16,
    focus_in: bool,
) -> Vec<u8> {
    let mut event = literal_event_header(
        crate::server::XI_FIRST_EVENT
            + if focus_in {
                crate::xinput::XI_DEVICE_FOCUS_IN_OFFSET
            } else {
                crate::xinput::XI_DEVICE_FOCUS_OUT_OFFSET
            },
        if focus_in { 3 } else { 5 }, // NotifyNonlinear / NotifyPointer
        sequence,
        order,
    );
    append_event_u32(&mut event, order, 0); // time
    append_event_u32(&mut event, order, FOCUS_WINDOW.0);
    event.extend_from_slice(&[0, u8::try_from(device).expect("XI1 device id fits CARD8")]);
    event.extend_from_slice(&[0; 18]);
    event
}

fn literal_xi1_device_focus_in(order: ClientByteOrder, sequence: u16, device: u16) -> Vec<u8> {
    literal_xi1_device_focus_event(order, sequence, device, true)
}

fn literal_xi1_device_focus_out(order: ClientByteOrder, sequence: u16, device: u16) -> Vec<u8> {
    literal_xi1_device_focus_event(order, sequence, device, false)
}

fn literal_xi1_keyboard_state_chain(order: ClientByteOrder, sequence: u16) -> Vec<u8> {
    let keyboard = crate::xinput::DEVICEID_XTEST_KEYBOARD;
    let mut lead = literal_event_header(
        crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_STATE_NOTIFY_OFFSET,
        u8::try_from(keyboard).unwrap() | crate::xinput::XI1_MORE_EVENTS,
        sequence,
        order,
    );
    append_event_u32(&mut lead, order, 0); // time
    lead.extend_from_slice(&[247, 0, 0, 1]); // key count, buttons, valuators, KeyClass
    lead.extend_from_slice(&[0; 20]); // buttons, first 32 key bits, valuators
    let mut continuation = literal_event_header(
        crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_KEY_STATE_NOTIFY_OFFSET,
        u8::try_from(keyboard).unwrap(),
        sequence,
        order,
    );
    continuation.extend_from_slice(&[0; 28]); // keycodes 40..255 are up
    [lead, continuation].concat()
}

fn literal_xi1_four_valuator_state_chain(
    order: ClientByteOrder,
    sequence: u16,
    device: u16,
) -> Vec<u8> {
    let mut lead = literal_event_header(
        crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_STATE_NOTIFY_OFFSET,
        u8::try_from(device).unwrap() | crate::xinput::XI1_MORE_EVENTS,
        sequence,
        order,
    );
    append_event_u32(&mut lead, order, 0); // time
    lead.extend_from_slice(&[0, 7, 3, 6]); // keys, seven buttons, three axes, Button+Valuator
    lead.extend_from_slice(&[0; 20]); // buttons, key bits, axes
    let mut continuation = literal_event_header(
        crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_VALUATOR_OFFSET,
        u8::try_from(device).unwrap(),
        sequence,
        order,
    );
    append_event_u16(&mut continuation, order, 0); // device_state
    continuation.extend_from_slice(&[1, 3]); // one valuator, starting at axis 3
    continuation.extend_from_slice(&[0; 24]);
    [lead, continuation].concat()
}

fn literal_xi1_focus_and_state_chain(
    order: ClientByteOrder,
    sequence: u16,
    device: u16,
    has_key_continuation: bool,
) -> Vec<u8> {
    let state_chain = if has_key_continuation {
        literal_xi1_keyboard_state_chain(order, sequence)
    } else {
        literal_xi1_four_valuator_state_chain(order, sequence, device)
    };
    [
        literal_xi1_device_focus_in(order, sequence, device),
        state_chain,
    ]
    .concat()
}

fn xi1_focus_descriptions(has_key_continuation: bool) -> Vec<EventDescription> {
    let focus_type = crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_FOCUS_IN_OFFSET;
    let state_type = crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_STATE_NOTIFY_OFFSET;
    let mut descriptions = vec![
        EventDescription::extension(
            focus_type,
            137,
            u16::from(crate::xinput::XI_DEVICE_FOCUS_IN_OFFSET),
        ),
        EventDescription::extension(
            state_type,
            137,
            u16::from(crate::xinput::XI_DEVICE_STATE_NOTIFY_OFFSET),
        ),
    ];
    if has_key_continuation {
        descriptions.push(EventDescription::extension(
            crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_KEY_STATE_NOTIFY_OFFSET,
            137,
            u16::from(crate::xinput::XI_DEVICE_KEY_STATE_NOTIFY_OFFSET),
        ));
    } else {
        descriptions.push(EventDescription::extension(
            crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_VALUATOR_OFFSET,
            137,
            u16::from(crate::xinput::XI_DEVICE_VALUATOR_OFFSET),
        ));
    }
    descriptions
}

// Kept focused: core SetInputFocus and XI1 keyboard/pointer SetDeviceFocus retain separate request checkpoints.
// mutation: emit the state batch before DeviceFocusIn (C18)
// mutation: omit the XI2 FocusOut frame (C19)
// mutation: omit the XI1 DeviceFocusOut frame (C20)
#[test]
fn receive_focus_baseline_frames() {
    let (mut fixture, pointer) = focus_notify_fixture();
    assert!(!fixture.state.policies_installed());
    let keyboard_focus_out_class = (u32::from(crate::xinput::DEVICEID_XTEST_KEYBOARD) << 8)
        | u32::from(crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_FOCUS_OUT_OFFSET);
    let pointer_focus_out_class = (u32::from(pointer) << 8)
        | u32::from(crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_FOCUS_OUT_OFFSET);
    for client in [NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED] {
        let state = fixture.state.clients.get_mut(&client.0).expect("receiver");
        state.xi2_masks.insert(
            (FOCUS_WINDOW, crate::xinput::DEVICEID_MASTER_KEYBOARD),
            (1 << XI_FOCUS_IN) | (1 << XI_FOCUS_OUT),
        );
        state
            .xi1_window_event_classes
            .get_mut(&FOCUS_WINDOW)
            .expect("focus-in selectors")
            .extend([keyboard_focus_out_class, pointer_focus_out_class]);
    }
    assert!(matches!(
        request_core_focus(&mut fixture),
        RequestOutcome::Handled
    ));
    assert_eq!(fixture.state.core_focus.raw, FOCUS_WINDOW.0);
    assert!(
        fixture.drain(NOTIFY_SENDER).is_empty(),
        "SetInputFocus has no reply"
    );
    assert_no_writer_failures(&fixture);
    assert!(matches!(
        request_xi1_device_focus(&mut fixture, 52, crate::xinput::DEVICEID_XTEST_KEYBOARD,),
        RequestOutcome::Handled
    ));
    assert_eq!(
        crate::core_loop::xi1_focus::device_focus(
            &fixture.state,
            crate::xinput::DEVICEID_XTEST_KEYBOARD,
        )
        .focus,
        FOCUS_WINDOW.0
    );
    assert!(
        fixture.drain(NOTIFY_SENDER).is_empty(),
        "keyboard SetDeviceFocus is void"
    );
    assert_no_writer_failures(&fixture);
    assert!(matches!(
        request_xi1_device_focus(&mut fixture, 53, pointer),
        RequestOutcome::Handled
    ));
    assert_eq!(
        crate::core_loop::xi1_focus::device_focus(&fixture.state, pointer).focus,
        FOCUS_WINDOW.0
    );
    assert!(
        fixture.drain(NOTIFY_SENDER).is_empty(),
        "pointer SetDeviceFocus is void"
    );
    assert_no_writer_failures(&fixture);
    for (client, order, sequence) in [
        (
            NOTIFY_RECEIVER_DENIED,
            ClientByteOrder::BigEndian,
            FOCUS_XI_SEQUENCE,
        ),
        (
            NOTIFY_RECEIVER_ALLOWED,
            ClientByteOrder::LittleEndian,
            FOCUS_OTHER_SEQUENCE,
        ),
    ] {
        let expected = [
            literal_core_focus_out_pointer(order, sequence),
            literal_core_focus_in(order, sequence),
            literal_xi2_focus_out(order, sequence),
            literal_xi2_focus_in(order, sequence),
            literal_xi1_device_focus_out(order, sequence, crate::xinput::DEVICEID_XTEST_KEYBOARD),
            literal_xi1_focus_and_state_chain(
                order,
                sequence,
                crate::xinput::DEVICEID_XTEST_KEYBOARD,
                true,
            ),
            literal_xi1_device_focus_out(order, sequence, pointer),
            literal_xi1_focus_and_state_chain(order, sequence, pointer, false),
        ]
        .concat();
        assert_eq!(
            fixture.drain(client),
            expected,
            "literal focus and state stream"
        );
    }
    assert_no_writer_failures(&fixture);
}

// mutation: leave F1 unchecked (C21)
#[test]
fn receive_xi2_focus_denial() {
    let build = || focus_notify_fixture();
    let act = |fixture: &mut NotifyFixture, _: &u16, _: &std::sync::Mutex<Vec<ExpectedReceive>>| {
        assert!(matches!(
            request_core_focus(fixture),
            RequestOutcome::Handled
        ));
        assert_eq!(fixture.state.core_focus.raw, FOCUS_WINDOW.0);
        assert!(fixture.drain(NOTIFY_SENDER).is_empty());
        assert_no_writer_failures(fixture);
    };
    let expect = |_: &u16| {
        let core_out = |client| ExpectedReceive {
            client,
            request: request_opcodes(42, 0),
            subject: EventSubject::Window(FOCUS_WINDOW),
            events: vec![EventDescription::core(10)],
        };
        let core_in = |client| ExpectedReceive {
            client,
            request: request_opcodes(42, 0),
            subject: EventSubject::Window(FOCUS_WINDOW),
            events: vec![EventDescription::core(9)],
        };
        let xi_in = |client| ExpectedReceive {
            client,
            request: request_opcodes(42, 0),
            subject: EventSubject::Window(FOCUS_WINDOW),
            events: vec![EventDescription::extension(35, 137, XI_FOCUS_IN)],
        };
        let denied = xi_in(NOTIFY_RECEIVER_DENIED);
        let mut expectation = CaseExpect::new(
            vec![
                Stream::unchanged(NOTIFY_SENDER, Vec::new()),
                Stream {
                    client: NOTIFY_RECEIVER_DENIED,
                    bytes: [
                        literal_core_focus_out_pointer(
                            ClientByteOrder::BigEndian,
                            FOCUS_XI_SEQUENCE,
                        )
                        .as_slice(),
                        literal_core_focus_in(ClientByteOrder::BigEndian, FOCUS_XI_SEQUENCE)
                            .as_slice(),
                        literal_xi2_focus_in(ClientByteOrder::BigEndian, FOCUS_XI_SEQUENCE)
                            .as_slice(),
                    ]
                    .concat(),
                    denied: Some(
                        [
                            literal_core_focus_out_pointer(
                                ClientByteOrder::BigEndian,
                                FOCUS_XI_SEQUENCE,
                            )
                            .as_slice(),
                            literal_core_focus_in(ClientByteOrder::BigEndian, FOCUS_XI_SEQUENCE)
                                .as_slice(),
                        ]
                        .concat(),
                    ),
                },
                Stream::unchanged(
                    NOTIFY_RECEIVER_ALLOWED,
                    [
                        literal_core_focus_out_pointer(
                            ClientByteOrder::LittleEndian,
                            FOCUS_OTHER_SEQUENCE,
                        )
                        .as_slice(),
                        literal_core_focus_in(ClientByteOrder::LittleEndian, FOCUS_OTHER_SEQUENCE)
                            .as_slice(),
                        literal_xi2_focus_in(ClientByteOrder::LittleEndian, FOCUS_OTHER_SEQUENCE)
                            .as_slice(),
                    ]
                    .concat(),
                ),
            ],
            vec![
                core_out(NOTIFY_RECEIVER_DENIED),
                core_in(NOTIFY_RECEIVER_DENIED),
                denied.clone(),
                core_out(NOTIFY_RECEIVER_ALLOWED),
                core_in(NOTIFY_RECEIVER_ALLOWED),
                xi_in(NOTIFY_RECEIVER_ALLOWED),
            ],
            CallOrder::PerClient,
            vec![denied],
        );
        expectation.deny_before_allow = true;
        expectation.after_deny = Some(Box::new(|allow, denied| {
            assert_eq!(denied.state.core_focus.raw, allow.state.core_focus.raw);
        }));
        expectation
    };
    check_receive_case(&ReceiveCase {
        name: "XI2 FocusIn denial preserves the core focus stream",
        build: &build,
        act: &act,
        expect: &expect,
    });
}

fn run_xi1_focus_receive_case(is_keyboard: bool, deny_focus: bool, name: &'static str) {
    let build = || focus_notify_fixture();
    let act =
        |fixture: &mut NotifyFixture, pointer: &u16, _: &std::sync::Mutex<Vec<ExpectedReceive>>| {
            let device = if is_keyboard {
                crate::xinput::DEVICEID_XTEST_KEYBOARD
            } else {
                *pointer
            };
            assert!(matches!(
                request_xi1_device_focus(fixture, 61, device),
                RequestOutcome::Handled
            ));
            assert_eq!(
                crate::core_loop::xi1_focus::device_focus(&fixture.state, device).focus,
                FOCUS_WINDOW.0,
                "Receive denial does not prevent XI1 focus movement"
            );
            assert!(
                fixture.drain(NOTIFY_SENDER).is_empty(),
                "SetDeviceFocus is void"
            );
            assert_no_writer_failures(fixture);
        };
    let expect = |pointer: &u16| {
        let device = if is_keyboard {
            crate::xinput::DEVICEID_XTEST_KEYBOARD
        } else {
            *pointer
        };
        let descriptions = xi1_focus_descriptions(is_keyboard);
        let focus_description = descriptions[0];
        let state_batch = descriptions[1..].to_vec();
        let focus = |client| ExpectedReceive {
            client,
            request: request_opcodes(137, 21),
            subject: EventSubject::Window(FOCUS_WINDOW),
            events: vec![focus_description],
        };
        let batch = |client| ExpectedReceive {
            client,
            request: request_opcodes(137, 21),
            subject: EventSubject::Window(FOCUS_WINDOW),
            events: state_batch.clone(),
        };
        let denied = if deny_focus {
            focus(NOTIFY_RECEIVER_DENIED)
        } else {
            batch(NOTIFY_RECEIVER_DENIED)
        };
        let full_denied = literal_xi1_focus_and_state_chain(
            ClientByteOrder::BigEndian,
            FOCUS_XI_SEQUENCE,
            device,
            is_keyboard,
        );
        let focus_only =
            literal_xi1_device_focus_in(ClientByteOrder::BigEndian, FOCUS_XI_SEQUENCE, device);
        let state_only = if is_keyboard {
            literal_xi1_keyboard_state_chain(ClientByteOrder::BigEndian, FOCUS_XI_SEQUENCE)
        } else {
            literal_xi1_four_valuator_state_chain(
                ClientByteOrder::BigEndian,
                FOCUS_XI_SEQUENCE,
                device,
            )
        };
        let other = literal_xi1_focus_and_state_chain(
            ClientByteOrder::LittleEndian,
            FOCUS_OTHER_SEQUENCE,
            device,
            is_keyboard,
        );
        let mut expectation = CaseExpect::new(
            vec![
                Stream::unchanged(NOTIFY_SENDER, Vec::new()),
                Stream {
                    client: NOTIFY_RECEIVER_DENIED,
                    bytes: full_denied,
                    denied: Some(if deny_focus { state_only } else { focus_only }),
                },
                Stream::unchanged(NOTIFY_RECEIVER_ALLOWED, other),
            ],
            vec![
                focus(NOTIFY_RECEIVER_DENIED),
                batch(NOTIFY_RECEIVER_DENIED),
                focus(NOTIFY_RECEIVER_ALLOWED),
                batch(NOTIFY_RECEIVER_ALLOWED),
            ],
            CallOrder::PerClient,
            vec![denied],
        );
        expectation.deny_before_allow = true;
        let focus_device = if is_keyboard {
            crate::xinput::DEVICEID_XTEST_KEYBOARD
        } else {
            *pointer
        };
        expectation.after_deny = Some(Box::new(move |allow, denied| {
            assert_eq!(
                crate::core_loop::xi1_focus::device_focus(&denied.state, focus_device).focus,
                crate::core_loop::xi1_focus::device_focus(&allow.state, focus_device).focus
            );
        }));
        expectation
    };
    check_receive_case(&ReceiveCase {
        name,
        build: &build,
        act: &act,
        expect: &expect,
    });
}

// mutation: describe only the lead DeviceStateNotify in F3 (C22)
// mutation: check the batch once per event instead of once per recipient (C23)
#[test]
fn receive_xi1_focus_and_state_batches_independent() {
    run_xi1_focus_receive_case(
        true,
        true,
        "XI1 keyboard focus denial keeps the state batch",
    );
    run_xi1_focus_receive_case(true, false, "XI1 keyboard state batch once check");
    run_xi1_focus_receive_case(
        false,
        true,
        "XI1 pointer focus denial keeps the state batch",
    );
    run_xi1_focus_receive_case(false, false, "XI1 pointer state batch once check");
}

// mutation: on owner denial fall back to selector targets (C26)
// Focused scenario: checks the post-grab snapshot and reply before BarrierHit.
#[test]
fn receive_barrier_grab_owner_denial() {
    let run_grabbed_owner = |deny_owner: bool| {
        let mut fixture = barrier_fixture();
        assert!(matches!(
            grab_pointer(
                &mut fixture,
                NOTIFY_SENDER,
                false,
                BARRIER_WINDOW,
                POINTER_MOTION_MASK as u16,
                GRAB_SEQUENCE,
            ),
            RequestOutcome::Handled
        ));
        assert_eq!(
            fixture.drain(NOTIFY_SENDER),
            literal_grab_reply(ClientByteOrder::LittleEndian, GRAB_SEQUENCE, 0)
        );
        let grab_after_reply = fixture
            .state
            .active_pointer_grab
            .expect("active owner grab after successful reply");
        let calls = fixture.install_policy(move |_, call| {
            if deny_owner
                && call.hook == ExtensionHook::Receive
                && call.client == NOTIFY_SENDER
                && call.receive == Some(EventSubject::Window(BARRIER_WINDOW))
                && call.events == Some(vec![EventDescription::extension(35, 137, 25)])
            {
                Decision::Deny(DENY)
            } else {
                Decision::Allow
            }
        });
        feed_barrier_motion(&mut fixture, 190, 95, 50);
        assert_eq!(fixture.state.pointer_root, (95, 50));
        assert!(!fixture.state.pointer_barriers[&BARRIER_XID].hit);
        feed_barrier_motion(&mut fixture, 200, 110, 50);
        assert_eq!(fixture.state.pointer_root, (99, 50));
        assert_eq!(fixture.state.master_sprite_root, (99, 50));
        assert!(fixture.state.pointer_barriers[&BARRIER_XID].hit);
        assert_eq!(fixture.state.pointer_barriers[&BARRIER_XID].event_id, 1);
        assert_pointer_grabs_equal(fixture.state.active_pointer_grab, Some(grab_after_reply));
        let pre_hit_motion = literal_core_motion(
            ClientByteOrder::LittleEndian,
            GRAB_SEQUENCE,
            190,
            BARRIER_WINDOW,
            95,
            50,
            95,
            50,
        );
        let hit_motion = literal_core_motion(
            ClientByteOrder::LittleEndian,
            GRAB_SEQUENCE,
            200,
            BARRIER_WINDOW,
            99,
            50,
            99,
            50,
        );
        assert_eq!(
            fixture.drain(NOTIFY_SENDER),
            if deny_owner {
                [pre_hit_motion.as_slice(), hit_motion.as_slice()].concat()
            } else {
                [
                    pre_hit_motion.as_slice(),
                    literal_grabbed_barrier_hit().as_slice(),
                    hit_motion.as_slice(),
                ]
                .concat()
            },
            "grab denial drops only BarrierHit; cooked motion remains unchanged"
        );
        assert!(fixture.drain(NOTIFY_RECEIVER_DENIED).is_empty());
        assert!(fixture.drain(NOTIFY_RECEIVER_ALLOWED).is_empty());
        let expected_receives = [ExpectedReceive {
            client: NOTIFY_SENDER,
            request: None,
            subject: EventSubject::Window(BARRIER_WINDOW),
            events: vec![EventDescription::extension(35, 137, 25)],
        }];
        let receive_log = calls.lock().unwrap();
        assert_receive_trace(
            "grabbed barrier owner denial",
            "deny",
            CallOrder::Total,
            &expected_receives,
            &receive_log,
            "Receive trace",
        );
        assert_forbidden_hooks(
            "grabbed barrier owner denial",
            "deny",
            &receive_log,
            &[ExtensionHook::Send],
        );
        assert_no_writer_failures(&fixture);
        (
            fixture.state.pointer_root,
            fixture.state.pointer_barriers[&BARRIER_XID].clone(),
            fixture.state.active_pointer_grab,
        )
    };

    let denied_owner_state = run_grabbed_owner(true);
    let allowed_owner_state = run_grabbed_owner(false);
    assert_eq!(denied_owner_state.0, allowed_owner_state.0);
    assert_eq!(denied_owner_state.1, allowed_owner_state.1);
    assert_pointer_grabs_equal(denied_owner_state.2, allowed_owner_state.2);
}

const BARRIER_XID: u32 = 0x0051_0044;
const BARRIER_WINDOW: ResourceId = ResourceId(0x0051_0045);
const BARRIER_HIT_MASK: u64 = 1 << 25;
const BARRIER_LEAVE_MASK: u64 = 1 << 26;
const BARRIER_EVENT_MASK: u64 = BARRIER_HIT_MASK | BARRIER_LEAVE_MASK;

fn barrier_fixture() -> NotifyFixture {
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
        let state = fixture.state.clients.get_mut(&client.0).unwrap();
        state.byte_order = order;
        state.last_sequence.store(sequence, Ordering::Relaxed);
    }
    fixture.state.resources.create_window(
        NOTIFY_SENDER,
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: BARRIER_WINDOW,
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 10,
            height: 10,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = fixture.state.resources.map_window(BARRIER_WINDOW);
    for client in [NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED] {
        let state = fixture.state.clients.get_mut(&client.0).unwrap();
        state.event_masks.insert(ROOT_WINDOW, POINTER_MOTION_MASK);
        state.xi2_masks.insert(
            (BARRIER_WINDOW, crate::xinput::DEVICEID_MASTER_POINTER),
            BARRIER_EVENT_MASK,
        );
    }
    fixture.state.pointer_barriers.insert(
        BARRIER_XID,
        crate::server::PointerBarrier {
            owner: NOTIFY_SENDER,
            window: BARRIER_WINDOW,
            x1: 100,
            y1: 0,
            x2: 100,
            y2: 200,
            directions: 0,
            devices: Vec::new(),
            hit: false,
            seen: false,
            event_id: 1,
            release_event_id: 0,
            last_timestamp: 0,
        },
    );
    fixture.state.pointer_root = (90, 50);
    fixture.state.master_sprite_root = (90, 50);
    fixture
}

fn feed_barrier_motion(fixture: &mut NotifyFixture, time: u32, x: i16, y: i16) {
    let (old_x, old_y) = fixture.state.pointer_root;
    let mut event = core_pointer_event(PointerEventKind::MotionNotify, 0, 0, time, x, y);
    event.event_x = x;
    event.event_y = y;
    event.raw_dx = i32::from(x - old_x);
    event.raw_dy = i32::from(y - old_y);
    feed_core_pointer_event(fixture, &HostXidMap::new(), event);
}

fn literal_barrier_event(
    order: ClientByteOrder,
    sequence: u16,
    evtype: u16,
    time: u32,
    event_id: u32,
    dtime: u32,
    flags: u32,
    root_x: i16,
    root_y: i16,
    dx: i32,
    dy: i32,
) -> Vec<u8> {
    let mut event = vec![35, 137]; // GenericEvent, XI2 major opcode
    append_event_u16(&mut event, order, sequence);
    append_event_u32(&mut event, order, 9); // nine extra four-byte words
    append_event_u16(&mut event, order, evtype);
    append_event_u16(&mut event, order, 2); // master pointer
    append_event_u32(&mut event, order, time);
    append_event_u32(&mut event, order, event_id);
    append_event_u32(&mut event, order, ROOT_WINDOW.0);
    append_event_u32(&mut event, order, BARRIER_WINDOW.0);
    append_event_u32(&mut event, order, BARRIER_XID);
    append_event_u32(&mut event, order, dtime);
    append_event_u32(&mut event, order, flags);
    append_event_u16(&mut event, order, 2); // master pointer source
    append_event_u16(&mut event, order, 0); // pad
    append_event_u32(&mut event, order, (i32::from(root_x) * 65_536) as u32);
    append_event_u32(&mut event, order, (i32::from(root_y) * 65_536) as u32);
    append_event_u32(&mut event, order, dx as u32); // FP3232 integral, zero fraction
    append_event_u32(&mut event, order, 0);
    append_event_u32(&mut event, order, dy as u32); // FP3232 integral, zero fraction
    append_event_u32(&mut event, order, 0);
    assert_eq!(event.len(), 68);
    event
}

fn literal_grabbed_barrier_hit() -> Vec<u8> {
    literal_barrier_event(
        ClientByteOrder::LittleEndian,
        GRAB_SEQUENCE,
        25,
        200,
        1,
        0,
        2,
        99,
        50,
        4,
        0,
    )
}

fn literal_ungrabbed_barrier_frames(order: ClientByteOrder, sequence: u16) -> [Vec<u8>; 5] {
    [
        literal_core_motion(order, sequence, 90, ROOT_WINDOW, 95, 50, 95, 50),
        literal_barrier_event(order, sequence, 25, 100, 1, 0, 0, 99, 50, 4, 0),
        literal_core_motion(order, sequence, 100, ROOT_WINDOW, 99, 50, 99, 50),
        literal_barrier_event(order, sequence, 26, 120, 1, 20, 0, 99, 203, 0, 0),
        literal_core_motion(order, sequence, 120, ROOT_WINDOW, 99, 203, 99, 203),
    ]
}

// Kept focused: the ungrabbed and grabbed motion sequences retain every pointer, barrier, and byte-stream checkpoint.
// mutation: swap BarrierHit and BarrierLeave evtypes (C24)
#[test]
fn receive_barrier_baseline_frames() {
    let mut fixture = barrier_fixture();
    assert!(!fixture.state.policies_installed());

    feed_barrier_motion(&mut fixture, 90, 95, 50);
    assert_eq!(fixture.state.pointer_root, (95, 50));
    feed_barrier_motion(&mut fixture, 100, 110, 50);
    assert_eq!(
        fixture.state.pointer_root,
        (99, 50),
        "BarrierHit clamps motion"
    );
    assert_eq!(fixture.state.master_sprite_root, (99, 50));
    assert!(fixture.state.pointer_barriers[&BARRIER_XID].hit);
    feed_barrier_motion(&mut fixture, 120, 99, 203);
    assert_eq!(fixture.state.pointer_root, (99, 203));
    assert_eq!(fixture.state.master_sprite_root, (99, 203));
    assert!(!fixture.state.pointer_barriers[&BARRIER_XID].hit);
    assert_eq!(fixture.state.pointer_barriers[&BARRIER_XID].event_id, 2);
    assert!(fixture.drain(NOTIFY_SENDER).is_empty());
    for (client, order, sequence) in [
        (
            NOTIFY_RECEIVER_DENIED,
            ClientByteOrder::BigEndian,
            DUAL_SEQUENCE,
        ),
        (
            NOTIFY_RECEIVER_ALLOWED,
            ClientByteOrder::LittleEndian,
            ROOT_SEQUENCE,
        ),
    ] {
        let frames = literal_ungrabbed_barrier_frames(order, sequence);
        assert_eq!(
            fixture.drain(client),
            [
                frames[0].as_slice(),
                frames[1].as_slice(),
                frames[2].as_slice(),
                frames[3].as_slice(),
                frames[4].as_slice(),
            ]
            .concat(),
            "ungrabbed motion, BarrierHit, motion, BarrierLeave, motion stream"
        );
    }

    let mut grabbed = barrier_fixture();
    assert!(!grabbed.state.policies_installed());
    assert!(matches!(
        grab_pointer(
            &mut grabbed,
            NOTIFY_SENDER,
            false,
            BARRIER_WINDOW,
            POINTER_MOTION_MASK as u16,
            GRAB_SEQUENCE,
        ),
        RequestOutcome::Handled
    ));
    assert!(grabbed.state.active_pointer_grab.is_some());
    assert_eq!(
        grabbed.drain(NOTIFY_SENDER),
        literal_grab_reply(ClientByteOrder::LittleEndian, GRAB_SEQUENCE, 0),
        "GrabPointer succeeds with its literal reply"
    );

    feed_barrier_motion(&mut grabbed, 190, 95, 50);
    assert_eq!(grabbed.state.pointer_root, (95, 50));
    assert!(!grabbed.state.pointer_barriers[&BARRIER_XID].hit);
    feed_barrier_motion(&mut grabbed, 200, 110, 50);
    assert_eq!(grabbed.state.pointer_root, (99, 50));
    assert_eq!(grabbed.state.master_sprite_root, (99, 50));
    assert!(grabbed.state.pointer_barriers[&BARRIER_XID].hit);
    assert_eq!(grabbed.state.pointer_barriers[&BARRIER_XID].event_id, 1);
    feed_barrier_motion(&mut grabbed, 220, 99, 203);
    assert_eq!(grabbed.state.pointer_root, (99, 203));
    assert_eq!(grabbed.state.master_sprite_root, (99, 203));
    assert!(!grabbed.state.pointer_barriers[&BARRIER_XID].hit);
    assert_eq!(grabbed.state.pointer_barriers[&BARRIER_XID].event_id, 2);
    assert_eq!(
        grabbed.drain(NOTIFY_SENDER),
        [
            literal_core_motion(
                ClientByteOrder::LittleEndian,
                GRAB_SEQUENCE,
                190,
                BARRIER_WINDOW,
                95,
                50,
                95,
                50,
            )
            .as_slice(),
            literal_grabbed_barrier_hit().as_slice(),
            literal_core_motion(
                ClientByteOrder::LittleEndian,
                GRAB_SEQUENCE,
                200,
                BARRIER_WINDOW,
                99,
                50,
                99,
                50,
            )
            .as_slice(),
            literal_barrier_event(
                ClientByteOrder::LittleEndian,
                GRAB_SEQUENCE,
                26,
                220,
                1,
                20,
                2,
                99,
                203,
                0,
                0,
            )
            .as_slice(),
            literal_core_motion(
                ClientByteOrder::LittleEndian,
                GRAB_SEQUENCE,
                220,
                BARRIER_WINDOW,
                99,
                203,
                99,
                203,
            )
            .as_slice(),
        ]
        .concat(),
        "grab owner receives literal motion, BarrierHit, motion, BarrierLeave, motion stream"
    );
    assert!(grabbed.drain(NOTIFY_RECEIVER_DENIED).is_empty());
    assert!(grabbed.drain(NOTIFY_RECEIVER_ALLOWED).is_empty());
    assert!(grabbed.state.active_pointer_grab.is_some());
    assert_no_writer_failures(&grabbed);
}

// mutation: leave B1 unchecked (C25)
// mutation: use ROOT_WINDOW for the barrier Receive subject (C27)
#[test]
fn receive_barrier_selector_denial() {
    let build = || (barrier_fixture(), ());
    let act = |fixture: &mut NotifyFixture, _: &(), _: &std::sync::Mutex<Vec<ExpectedReceive>>| {
        feed_barrier_motion(fixture, 90, 95, 50);
        assert_eq!(fixture.state.pointer_root, (95, 50));
        feed_barrier_motion(fixture, 100, 110, 50);
        assert_eq!(fixture.state.pointer_root, (99, 50));
        assert_eq!(fixture.state.master_sprite_root, (99, 50));
        assert!(fixture.state.pointer_barriers[&BARRIER_XID].hit);
        feed_barrier_motion(fixture, 120, 99, 203);
        assert_eq!(fixture.state.pointer_root, (99, 203));
        assert_eq!(fixture.state.master_sprite_root, (99, 203));
        assert!(!fixture.state.pointer_barriers[&BARRIER_XID].hit);
        assert_eq!(fixture.state.pointer_barriers[&BARRIER_XID].event_id, 2);
        assert!(fixture.drain(NOTIFY_SENDER).is_empty());
        assert_no_writer_failures(fixture);
    };
    let expect = |_: &()| {
        let receive = |client, evtype| ExpectedReceive {
            client,
            request: None,
            subject: EventSubject::Window(BARRIER_WINDOW),
            events: vec![EventDescription::extension(35, 137, evtype)],
        };
        let denied_hit = receive(NOTIFY_RECEIVER_DENIED, 25);
        let denied_frames =
            literal_ungrabbed_barrier_frames(ClientByteOrder::BigEndian, DUAL_SEQUENCE);
        let allowed_frames =
            literal_ungrabbed_barrier_frames(ClientByteOrder::LittleEndian, ROOT_SEQUENCE);
        let denied_bytes = [
            denied_frames[0].as_slice(),
            denied_frames[2].as_slice(),
            denied_frames[3].as_slice(),
            denied_frames[4].as_slice(),
        ]
        .concat();
        let mut expectation = CaseExpect::new(
            vec![
                Stream::unchanged(NOTIFY_SENDER, Vec::new()),
                Stream {
                    client: NOTIFY_RECEIVER_DENIED,
                    bytes: denied_frames.concat(),
                    denied: Some(denied_bytes),
                },
                Stream::unchanged(NOTIFY_RECEIVER_ALLOWED, allowed_frames.concat()),
            ],
            vec![
                receive(NOTIFY_RECEIVER_DENIED, 25),
                receive(NOTIFY_RECEIVER_DENIED, 26),
                receive(NOTIFY_RECEIVER_ALLOWED, 25),
                receive(NOTIFY_RECEIVER_ALLOWED, 26),
            ],
            CallOrder::PerClient,
            vec![denied_hit],
        );
        expectation.deny_before_allow = true;
        expectation.after_deny = Some(Box::new(|allow, denied| {
            assert_eq!(denied.state.pointer_root, allow.state.pointer_root);
            assert_eq!(
                denied.state.master_sprite_root,
                allow.state.master_sprite_root
            );
            assert_eq!(
                denied.state.pointer_barriers[&BARRIER_XID],
                allow.state.pointer_barriers[&BARRIER_XID]
            );
        }));
        expectation
    };
    check_receive_case(&ReceiveCase {
        name: "BarrierHit denial preserves selectors and barrier state",
        build: &build,
        act: &act,
        expect: &expect,
    });
}
