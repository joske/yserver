use super::*;

use crate::{
    backend::recording::RecordingBackend,
    resources::{ROOT_VISUAL, ROOT_WINDOW},
};
use yserver_protocol::x11::{RequestHeader, ResourceId, SequenceNumber};

const GRABBER: u32 = 0xA753;
const OTHER: u32 = 0xA754;
const WINDOW: u32 = 0x0010_A753;
const CHILD: u32 = 0x0020_A753;
const OUTSIDE: u32 = 0x0010_A754;
const HOST_WINDOW: u32 = 0xCAFE_A753;
const HOST_OUTSIDE: u32 = 0xCAFE_A754;
const XI2_OPCODE: u8 = 137;
const XI_SELECT_EVENTS: u8 = 46;
const XI_GRAB_DEVICE: u8 = 51;
const XI_UNGRAB_DEVICE: u8 = 52;
const XI_MOTION: u32 = 1 << 6;
const XI_BUTTONS: u32 = (1 << 4) | (1 << 5);
const XI_GRAB_MASK: u32 = XI_MOTION | XI_BUTTONS;
const XI_MASTER: u16 = 2;
const XI_XTEST: u16 = 4;
const CORE_POINTER: u32 = 0x4c;
const CORE_MOTION: u32 = 0x40;
const CORE_BUTTONS: u32 = 0x0c;

struct Fixture {
    state: ServerState,
    backend: RecordingBackend,
    xid_map: HostXidMap,
    grabber: crate::transport::CapturedPeer,
    other: crate::transport::CapturedPeer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Delivery {
    xi2: bool,
    kind: u16,
    window: u32,
    child: u32,
}

fn fixture() -> Fixture {
    fixture_with_child_owner(OTHER)
}

fn fixture_with_child_owner(child_owner: u32) -> Fixture {
    let mut state = ServerState::new();
    let grabber = install_capture_client(&mut state, GRABBER);
    let other = install_capture_client(&mut state, OTHER);
    let backend = RecordingBackend::new();
    for (owner, window, parent, x, y, width, height) in [
        (GRABBER, WINDOW, ROOT_WINDOW, 100, 100, 200, 200),
        (child_owner, CHILD, ResourceId(WINDOW), 20, 20, 100, 100),
        (GRABBER, OUTSIDE, ROOT_WINDOW, 550, 100, 300, 300),
    ] {
        state.resources.create_window(
            ClientId(owner),
            yserver_protocol::x11::CreateWindowRequest {
                depth: 24,
                window: ResourceId(window),
                parent,
                x,
                y,
                width,
                height,
                border_width: 0,
                class: 1,
                visual: ROOT_VISUAL,
                ..Default::default()
            },
        );
        let _ = state.resources.map_window(ResourceId(window));
    }
    let mut xid_map = HostXidMap::new();
    xid_map.insert(HOST_WINDOW, ResourceId(WINDOW));
    xid_map.insert(HOST_OUTSIDE, ResourceId(OUTSIDE));
    Fixture {
        state,
        backend,
        xid_map,
        grabber,
        other,
    }
}

fn dispatch(f: &mut Fixture, client: u32, sequence: u16, opcode: u8, data: u8, body: &[u8]) {
    crate::core_loop::process_request::process_request(
        &mut f.state,
        &mut f.backend,
        ClientId(client),
        SequenceNumber(sequence),
        RequestHeader {
            opcode,
            data,
            length_units: u32::try_from((body.len() + 4) / 4).unwrap(),
        },
        body,
        None,
    )
    .expect("production request dispatch");
}

fn select_xi2(f: &mut Fixture, client: u32, window: u32, device: u16, mask: u32) {
    let mut body = Vec::with_capacity(20);
    body.extend_from_slice(&window.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes());
    body.extend_from_slice(&[0; 2]);
    body.extend_from_slice(&device.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes());
    body.extend_from_slice(&mask.to_le_bytes());
    dispatch(f, client, 1, XI2_OPCODE, XI_SELECT_EVENTS, &body);
    assert_eq!(
        f.state.clients[&client]
            .xi2_masks
            .get(&(ResourceId(window), device)),
        Some(&u64::from(mask)),
        "XISelectEvents must register the test's real selection"
    );
}

fn select_core(f: &mut Fixture, client: u32, window: u32, mask: u32) {
    let mut body = Vec::with_capacity(12);
    body.extend_from_slice(&window.to_le_bytes());
    body.extend_from_slice(&(1u32 << 11).to_le_bytes()); // CWEventMask
    body.extend_from_slice(&mask.to_le_bytes());
    dispatch(f, client, 1, 2, 0, &body); // ChangeWindowAttributes
    assert_eq!(
        f.state.clients[&client]
            .event_masks
            .get(&ResourceId(window)),
        Some(&mask),
        "ChangeWindowAttributes must register the test's real selection"
    );
}

fn grab_xi2(f: &mut Fixture, window: u32) {
    grab_xi2_with_options(f, window, true, XI_GRAB_MASK);
}

fn grab_xi2_with_options(f: &mut Fixture, window: u32, owner_events: bool, mask: u32) {
    let mut body = Vec::with_capacity(24);
    body.extend_from_slice(&window.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    body.extend_from_slice(&0u32.to_le_bytes()); // no cursor
    body.extend_from_slice(&XI_MASTER.to_le_bytes());
    body.extend_from_slice(&[1, 1, u8::from(owner_events), 0]); // async, async, owner_events
    body.extend_from_slice(&1u16.to_le_bytes());
    body.extend_from_slice(&mask.to_le_bytes());
    dispatch(f, GRABBER, 2, XI2_OPCODE, XI_GRAB_DEVICE, &body);
    assert_eq!(
        f.state.active_pointer_grab.map(|grab| grab.owner),
        Some(ClientId(GRABBER)),
        "XIGrabDevice must install the test's real grab"
    );
    let _ = read_all_capture_available(&mut f.grabber); // reply and crossings
    let _ = read_all_capture_available(&mut f.other);
}

fn ungrab_xi2(f: &mut Fixture) {
    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    body.extend_from_slice(&XI_MASTER.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes());
    dispatch(f, GRABBER, 3, XI2_OPCODE, XI_UNGRAB_DEVICE, &body);
    let _ = read_all_capture_available(&mut f.grabber); // crossings
    let _ = read_all_capture_available(&mut f.other);
}

fn grab_core(f: &mut Fixture, window: u32) {
    grab_core_with_options(f, window, true, CORE_POINTER);
}

fn grab_core_with_options(f: &mut Fixture, window: u32, owner_events: bool, mask: u32) {
    let mut body = Vec::with_capacity(20);
    body.extend_from_slice(&window.to_le_bytes());
    body.extend_from_slice(&(mask as u16).to_le_bytes());
    body.extend_from_slice(&[1, 1]); // async pointer and keyboard modes
    body.extend_from_slice(&0u32.to_le_bytes()); // no confine window
    body.extend_from_slice(&0u32.to_le_bytes()); // no cursor
    body.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    dispatch(f, GRABBER, 2, 26, u8::from(owner_events), &body); // GrabPointer
    assert_eq!(
        f.state.active_pointer_grab.unwrap().owner,
        ClientId(GRABBER),
        "GrabPointer must install the test's real grab"
    );
    assert_eq!(
        f.state.active_pointer_grab.unwrap().owner_events,
        owner_events,
        "GrabPointer must install the requested owner_events value"
    );
    let _ = read_all_capture_available(&mut f.grabber); // reply and crossings
    let _ = read_all_capture_available(&mut f.other);
}

fn inject(f: &mut Fixture, kind: PointerEventKind, x: i16, y: i16, host_xid: u32) {
    let (host_x, host_y) = if host_xid == HOST_OUTSIDE {
        (550, 100)
    } else {
        (100, 100)
    };
    let _ = pointer_event_fanout_to_state(
        &mut f.state,
        &mut f.backend,
        &f.xid_map,
        HostPointerEvent {
            origin: crate::core_loop::InputOrigin::XTest(XI_XTEST),
            kind,
            host_xid,
            detail: if matches!(
                kind,
                PointerEventKind::ButtonPress | PointerEventKind::ButtonRelease
            ) {
                1
            } else {
                0
            },
            time: 50,
            root_x: x,
            root_y: y,
            event_x: x - host_x,
            event_y: y - host_y,
            state: if kind == PointerEventKind::ButtonRelease {
                0x100
            } else {
                0
            },
            crossing_mode: 0,
            child: 0,
            raw_dx: 0,
            raw_dy: 0,
            tree_change: false,
        },
        true,
        false,
    );
}

fn decode_deliveries(bytes: &[u8]) -> Vec<Delivery> {
    let mut result = Vec::new();
    let mut offset = 0;
    while offset + 32 <= bytes.len() {
        if bytes[offset] == 35 {
            let units = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap());
            let kind = u16::from_le_bytes([bytes[offset + 8], bytes[offset + 9]]);
            if (4..=6).contains(&kind) {
                result.push(Delivery {
                    xi2: true,
                    kind,
                    window: u32::from_le_bytes(bytes[offset + 24..offset + 28].try_into().unwrap()),
                    child: u32::from_le_bytes(bytes[offset + 28..offset + 32].try_into().unwrap()),
                });
            }
            offset += 32 + usize::try_from(units).unwrap() * 4;
        } else {
            let kind = bytes[offset] & 0x7f;
            if (4..=6).contains(&kind) {
                result.push(Delivery {
                    xi2: false,
                    kind: u16::from(kind),
                    window: u32::from_le_bytes(bytes[offset + 12..offset + 16].try_into().unwrap()),
                    child: u32::from_le_bytes(bytes[offset + 16..offset + 20].try_into().unwrap()),
                });
            }
            offset += 32;
        }
    }
    assert_eq!(offset, bytes.len(), "event stream fully consumed");
    result
}

fn read_deliveries(peer: &mut crate::transport::CapturedPeer) -> Vec<Delivery> {
    decode_deliveries(&read_all_capture_available(peer))
}

fn delivery_summary(events: &[Delivery]) -> Vec<(bool, u16, u32)> {
    events
        .iter()
        .map(|event| (event.xi2, event.kind, event.window))
        .collect()
}

#[test]
fn owner_events_xi2_grab_window_foreign_selector_gets_nothing() {
    // C1: Xvfb 21.1.24 delivers the grab-window fallback to A and nothing to B.
    let mut f = fixture();
    select_xi2(&mut f, OTHER, WINDOW, XI_MASTER, XI_GRAB_MASK);
    grab_xi2(&mut f, WINDOW);
    inject(
        &mut f,
        PointerEventKind::MotionNotify,
        150,
        150,
        HOST_WINDOW,
    );
    assert_eq!(
        delivery_summary(&read_deliveries(&mut f.grabber)),
        vec![(true, 6, WINDOW)]
    );
    assert!(read_deliveries(&mut f.other).is_empty());
}

#[test]
fn owner_events_xi2_descendant_foreign_selector_gets_nothing() {
    // C2: only the recipient set is pinned; XI2 child semantics are Task 4.
    let mut f = fixture();
    select_xi2(&mut f, OTHER, CHILD, XI_MASTER, XI_GRAB_MASK);
    grab_xi2(&mut f, WINDOW);
    inject(
        &mut f,
        PointerEventKind::MotionNotify,
        150,
        150,
        HOST_WINDOW,
    );
    assert_eq!(
        delivery_summary(&read_deliveries(&mut f.grabber)),
        vec![(true, 6, WINDOW)]
    );
    assert!(read_deliveries(&mut f.other).is_empty());
}

#[test]
fn owner_events_xi2_owned_toplevel_foreign_selector_gets_nothing() {
    // C3: OUT is owned by A but outside the grab window's tree.
    let mut f = fixture();
    select_xi2(&mut f, OTHER, OUTSIDE, XI_MASTER, XI_GRAB_MASK);
    grab_xi2(&mut f, WINDOW);
    assert_eq!(
        f.state.root_pointer_target_at(550, 150).map(|hit| hit.0),
        Some(ResourceId(OUTSIDE)),
        "C3 input must hit A-owned OUT"
    );
    inject(
        &mut f,
        PointerEventKind::MotionNotify,
        550,
        150,
        HOST_OUTSIDE,
    );
    assert_eq!(
        delivery_summary(&read_deliveries(&mut f.grabber)),
        vec![(true, 6, WINDOW)]
    );
    assert!(read_deliveries(&mut f.other).is_empty());
}

#[test]
fn owner_events_core_after_rejected_xi2_on_same_window() {
    // C4: a rejected XI2 attempt does not prevent A's core delivery on W.
    let mut f = fixture();
    select_xi2(&mut f, OTHER, WINDOW, XI_MASTER, XI_GRAB_MASK);
    select_core(&mut f, GRABBER, WINDOW, CORE_MOTION);
    grab_xi2(&mut f, WINDOW);
    inject(
        &mut f,
        PointerEventKind::MotionNotify,
        150,
        150,
        HOST_WINDOW,
    );
    assert_eq!(
        read_deliveries(&mut f.grabber),
        vec![Delivery {
            xi2: false,
            kind: 6,
            window: WINDOW,
            child: CHILD
        }]
    );
    assert!(read_deliveries(&mut f.other).is_empty());
}

#[test]
fn owner_events_owner_xi2_and_core_on_same_window_delivers_once() {
    // C4b: owner XI2 wins the attempt, so no core duplicate and no fallback.
    let mut f = fixture();
    select_xi2(&mut f, OTHER, CHILD, XI_MASTER, XI_GRAB_MASK);
    select_xi2(&mut f, GRABBER, CHILD, XI_MASTER, XI_GRAB_MASK);
    select_core(&mut f, GRABBER, CHILD, CORE_MOTION);
    grab_xi2(&mut f, WINDOW);
    inject(
        &mut f,
        PointerEventKind::MotionNotify,
        150,
        150,
        HOST_WINDOW,
    );
    assert_eq!(
        read_deliveries(&mut f.grabber),
        vec![Delivery {
            xi2: true,
            kind: 6,
            window: CHILD,
            child: 0
        }]
    );
    assert!(read_deliveries(&mut f.other).is_empty());
}

#[test]
fn core_implicit_grab_foreign_xi2_motion_follows_row() {
    // C8c: Xvfb sends B's pre-press Motion, then none during A's core implicit grab.
    let mut f = fixture();
    select_core(&mut f, GRABBER, WINDOW, CORE_POINTER);
    select_xi2(&mut f, OTHER, CHILD, XI_MASTER, XI_MOTION);
    inject(
        &mut f,
        PointerEventKind::MotionNotify,
        150,
        150,
        HOST_WINDOW,
    );
    inject(&mut f, PointerEventKind::ButtonPress, 150, 150, HOST_WINDOW);
    inject(
        &mut f,
        PointerEventKind::MotionNotify,
        160,
        160,
        HOST_WINDOW,
    );
    inject(
        &mut f,
        PointerEventKind::ButtonRelease,
        160,
        160,
        HOST_WINDOW,
    );
    assert_eq!(
        read_deliveries(&mut f.grabber),
        vec![
            Delivery {
                xi2: false,
                kind: 4,
                window: WINDOW,
                child: CHILD
            },
            Delivery {
                xi2: false,
                kind: 6,
                window: WINDOW,
                child: CHILD
            },
            Delivery {
                xi2: false,
                kind: 5,
                window: WINDOW,
                child: CHILD
            },
        ],
        "C8c A receives exactly the core press, in-grab motion, and release on W"
    );
    assert_eq!(
        read_deliveries(&mut f.other),
        vec![Delivery {
            xi2: true,
            kind: 6,
            window: CHILD,
            child: 0
        }],
        "C8c B receives exactly the single pre-grab master Motion on D"
    );
}

#[test]
fn core_owner_events_stops_at_foreign_master_xi2_selection() {
    // C6: the foreign master selection on D stops the owner walk and falls back to OUT.
    let mut f = fixture();
    select_core(&mut f, GRABBER, WINDOW, CORE_MOTION);
    select_xi2(&mut f, OTHER, CHILD, XI_MASTER, XI_MOTION);
    grab_core(&mut f, OUTSIDE);
    inject(
        &mut f,
        PointerEventKind::MotionNotify,
        150,
        150,
        HOST_WINDOW,
    );
    assert_eq!(
        read_deliveries(&mut f.grabber),
        vec![Delivery {
            xi2: false,
            kind: 6,
            window: OUTSIDE,
            child: 0
        }]
    );
    assert!(read_deliveries(&mut f.other).is_empty());
}

#[test]
fn core_owner_events_owner_core_wins_over_foreign_xi2_on_same_window() {
    // C7: after rejecting B's XI2 attempt on W, A's core attempt still wins there.
    let mut f = fixture();
    select_core(&mut f, GRABBER, WINDOW, CORE_MOTION);
    select_xi2(&mut f, OTHER, WINDOW, XI_MASTER, XI_GRAB_MASK);
    grab_core(&mut f, WINDOW);
    inject(
        &mut f,
        PointerEventKind::MotionNotify,
        150,
        150,
        HOST_WINDOW,
    );
    assert_eq!(
        read_deliveries(&mut f.grabber),
        vec![Delivery {
            xi2: false,
            kind: 6,
            window: WINDOW,
            child: CHILD
        }]
    );
    assert!(read_deliveries(&mut f.other).is_empty());
}

#[test]
fn core_owner_events_slave_only_xi2_does_not_block() {
    // C7s measured fixture: D is also owned by A; the master owner walk
    // ignores B's slave selection, and B still gets device 4/source 4.
    let mut f = fixture_with_child_owner(GRABBER);
    select_core(&mut f, GRABBER, WINDOW, CORE_MOTION);
    select_xi2(&mut f, OTHER, CHILD, XI_XTEST, XI_MOTION);
    grab_core(&mut f, OUTSIDE);
    inject(
        &mut f,
        PointerEventKind::MotionNotify,
        150,
        150,
        HOST_WINDOW,
    );
    assert_eq!(
        read_deliveries(&mut f.grabber),
        vec![Delivery {
            xi2: false,
            kind: 6,
            window: WINDOW,
            child: CHILD
        }]
    );
    let other_bytes = read_all_capture_available(&mut f.other);
    assert_eq!(
        decode_deliveries(&other_bytes),
        vec![Delivery {
            xi2: true,
            kind: 6,
            window: CHILD,
            child: 0
        }]
    );
    assert_eq!(xi2_event_ids(&other_bytes), vec![(6, XI_XTEST, XI_XTEST)]);
}

#[test]
fn core_owner_events_slave_only_xi2_does_not_block_b_owned_variant() {
    // C7s B-owned variant: this extra fixture intentionally differs from
    // the measured W:A,D:A,OUT:A ownership; it preserves coverage of the
    // pre-existing ownership shortcut.
    let mut f = fixture();
    select_core(&mut f, GRABBER, WINDOW, CORE_MOTION);
    select_xi2(&mut f, OTHER, CHILD, XI_XTEST, XI_MOTION);
    grab_core(&mut f, OUTSIDE);
    inject(
        &mut f,
        PointerEventKind::MotionNotify,
        150,
        150,
        HOST_WINDOW,
    );
    assert_eq!(
        read_deliveries(&mut f.grabber),
        vec![Delivery {
            xi2: false,
            kind: 6,
            window: WINDOW,
            child: CHILD
        }]
    );
    let other_bytes = read_all_capture_available(&mut f.other);
    assert_eq!(
        decode_deliveries(&other_bytes),
        vec![Delivery {
            xi2: true,
            kind: 6,
            window: CHILD,
            child: 0
        }]
    );
    assert_eq!(xi2_event_ids(&other_bytes), vec![(6, XI_XTEST, XI_XTEST)]);
}

#[test]
fn owner_events_ungrab_restores_natural_xi2_delivery() {
    // C11: B is filtered during the grab, then gets the second motion after XIUngrabDevice.
    let mut f = fixture();
    select_xi2(&mut f, OTHER, WINDOW, XI_MASTER, XI_GRAB_MASK);
    grab_xi2(&mut f, WINDOW);
    inject(
        &mut f,
        PointerEventKind::MotionNotify,
        150,
        150,
        HOST_WINDOW,
    );
    let owner_before = read_deliveries(&mut f.grabber);
    let other_before = read_deliveries(&mut f.other);
    ungrab_xi2(&mut f);
    inject(
        &mut f,
        PointerEventKind::MotionNotify,
        160,
        160,
        HOST_WINDOW,
    );
    assert_eq!(delivery_summary(&owner_before), vec![(true, 6, WINDOW)]);
    assert!(other_before.is_empty());
    assert!(read_deliveries(&mut f.grabber).is_empty());
    assert_eq!(
        delivery_summary(&read_deliveries(&mut f.other)),
        vec![(true, 6, WINDOW)]
    );
}

#[test]
fn exclusive_master_grab_scroll_motion_reaches_only_grab_owner() {
    // Source-backed exclusive case: ../xserver/dix/events.c:4431-4462
    // filters a grabbed event through the grab mask, and
    // ../xserver/dix/getevents.c:1646-1697, 1703-1718 creates the separate
    // smooth-scroll Motion before the wheel ButtonPress.
    const SOURCE: u64 = 0xA7531;
    let mut f = fixture();
    let pointer = pointer_source(&mut f.state, SOURCE, true);
    select_xi2(&mut f, OTHER, CHILD, XI_MASTER, XI_MOTION);
    grab_xi2_with_options(&mut f, WINDOW, false, XI_MOTION | (1 << 4));

    let dropped = pointer_event_fanout_to_state(
        &mut f.state,
        &mut f.backend,
        &f.xid_map,
        HostPointerEvent {
            origin: crate::core_loop::InputOrigin::Physical(crate::xinput::InputSourceId(SOURCE)),
            kind: PointerEventKind::ButtonPress,
            host_xid: HOST_WINDOW,
            detail: 5,
            time: 55,
            root_x: 150,
            root_y: 150,
            event_x: 50,
            event_y: 50,
            ..motion_event()
        },
        true,
        false,
    );
    assert!(dropped.is_empty());

    let grabber_bytes = read_all_capture_available(&mut f.grabber);
    assert_eq!(
        decode_deliveries(&grabber_bytes),
        vec![
            Delivery {
                xi2: true,
                kind: 6,
                window: WINDOW,
                child: 0
            },
            Delivery {
                xi2: true,
                kind: 4,
                window: WINDOW,
                child: 0
            },
        ],
        "the exclusive grab redirects both the smooth Motion and wheel press to W"
    );
    assert_eq!(
        xi2_event_ids(&grabber_bytes),
        vec![(6, XI_MASTER, pointer), (4, XI_MASTER, pointer)]
    );
    assert!(read_all_capture_available(&mut f.other).is_empty());
}

#[test]
fn exclusive_core_grab_scroll_motion_does_not_leak_to_master_selector() {
    // ../xserver/dix/events.c:4431-4462 delivers only selected grabbed
    // events. getevents.c:1646-1697, 1703-1718 builds smooth Motion and
    // legacy wheel edges; eventconvert.c:113-122 rejects core conversion of
    // wheel-only Motion, so no core Motion is synthesized for the grab owner.
    const SOURCE: u64 = 0xA7532;
    let mut f = fixture();
    let pointer = pointer_source(&mut f.state, SOURCE, true);
    select_xi2(&mut f, OTHER, CHILD, XI_MASTER, XI_MOTION);
    grab_core_with_options(&mut f, WINDOW, false, CORE_BUTTONS);

    for (kind, time, state) in [
        (PointerEventKind::ButtonPress, 55, 0),
        (PointerEventKind::ButtonRelease, 56, 0x100),
    ] {
        let dropped = pointer_event_fanout_to_state(
            &mut f.state,
            &mut f.backend,
            &f.xid_map,
            HostPointerEvent {
                origin: crate::core_loop::InputOrigin::Physical(crate::xinput::InputSourceId(
                    SOURCE,
                )),
                kind,
                host_xid: HOST_WINDOW,
                detail: 5,
                time,
                root_x: 150,
                root_y: 150,
                event_x: 50,
                event_y: 50,
                state,
                ..motion_event()
            },
            true,
            false,
        );
        assert!(dropped.is_empty());
    }

    let grabber_bytes = read_all_capture_available(&mut f.grabber);
    assert_eq!(
        grabber_bytes.len(),
        64,
        "the core grab owner gets only two events"
    );
    assert_eq!(
        decode_deliveries(&grabber_bytes),
        vec![
            Delivery {
                xi2: false,
                kind: 4,
                window: WINDOW,
                child: CHILD,
            },
            Delivery {
                xi2: false,
                kind: 5,
                window: WINDOW,
                child: CHILD,
            },
        ],
        "the core grab owner gets the selected wheel button edges, with no synthetic Motion"
    );
    assert!(
        read_all_capture_available(&mut f.other).is_empty(),
        "the foreign XI2 master selector gets no event during the exclusive core grab"
    );
    assert_ne!(pointer, 0, "the scroll source is registered as a pointer");
}

#[test]
fn core_owner_events_release_follows_press_to_owned_window() {
    // C14: the natural press and release on A-owned OUT remain paired under a core grab.
    let mut f = fixture();
    select_core(&mut f, GRABBER, OUTSIDE, CORE_BUTTONS);
    grab_core(&mut f, WINDOW);
    assert_eq!(
        f.state.root_pointer_target_at(650, 150).map(|hit| hit.0),
        Some(ResourceId(OUTSIDE)),
        "C14 live pointer hit must be OUT before input; the injected host XID is unmapped"
    );
    inject(&mut f, PointerEventKind::ButtonPress, 650, 150, 0);
    inject(&mut f, PointerEventKind::ButtonRelease, 650, 150, 0);
    assert_eq!(
        read_deliveries(&mut f.grabber),
        vec![
            Delivery {
                xi2: false,
                kind: 4,
                window: OUTSIDE,
                child: 0
            },
            Delivery {
                xi2: false,
                kind: 5,
                window: OUTSIDE,
                child: 0
            },
        ]
    );
    assert!(read_deliveries(&mut f.other).is_empty());
}
