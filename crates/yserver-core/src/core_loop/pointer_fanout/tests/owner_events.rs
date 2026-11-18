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
const XTEST_OPCODE: u8 = 146;
const XI_SELECT_EVENTS: u8 = 46;
const XI_GRAB_DEVICE: u8 = 51;
const XI_UNGRAB_DEVICE: u8 = 52;
const XI_SELECT_EXTENSION_EVENT: u8 = 6;
const XI_MOTION: u32 = 1 << 6;
const XI_BUTTONS: u32 = (1 << 4) | (1 << 5);
const XI_GRAB_MASK: u32 = XI_MOTION | XI_BUTTONS;
const XI_MASTER: u16 = 2;
const XI_XTEST: u16 = 4;
const XTEST_FAKE_INPUT: u8 = 2;
const XI1_MORE_EVENTS: u8 = 0x80;
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

fn select_xi1_motion(f: &mut Fixture, client: u32, window: u32, device: u16) {
    let class = (u32::from(device) << 8)
        | u32::from(crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_MOTION_NOTIFY_OFFSET);
    let mut body = Vec::with_capacity(12);
    body.extend_from_slice(&window.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes());
    body.extend_from_slice(&[0; 2]);
    body.extend_from_slice(&class.to_le_bytes());
    dispatch(f, client, 1, XI2_OPCODE, XI_SELECT_EXTENSION_EVENT, &body);
    assert!(
        f.state.clients[&client]
            .xi1_window_event_classes
            .get(&ResourceId(window))
            .is_some_and(|classes| classes.contains(&class)),
        "SelectExtensionEvent must register the test's real XI1 selection"
    );
}

fn fake_device_motion(f: &mut Fixture, device: u16, first: u8, values: &[i32]) {
    assert!(!values.is_empty() && values.len() <= 6);
    let mut body = vec![0u8; 64];
    body[0] = crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_MOTION_NOTIFY_OFFSET;
    body[4..8].copy_from_slice(&50u32.to_le_bytes());
    body[31] = u8::try_from(device).unwrap() | XI1_MORE_EVENTS;
    body[32] = crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_VALUATOR_OFFSET;
    body[33] = u8::try_from(device).unwrap();
    body[38] = u8::try_from(values.len()).unwrap();
    body[39] = first;
    for (index, value) in values.iter().enumerate() {
        let offset = 40 + index * 4;
        body[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    dispatch(f, GRABBER, 4, XTEST_OPCODE, XTEST_FAKE_INPUT, &body);
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
    grab_xi2_device_with_options(f, window, XI_MASTER, owner_events, mask);
}

fn grab_xi2_device_with_options(
    f: &mut Fixture,
    window: u32,
    device: u16,
    owner_events: bool,
    mask: u32,
) {
    let mut body = Vec::with_capacity(24);
    body.extend_from_slice(&window.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    body.extend_from_slice(&0u32.to_le_bytes()); // no cursor
    body.extend_from_slice(&device.to_le_bytes());
    body.extend_from_slice(&[1, 1, u8::from(owner_events), 0]); // async, async, owner_events
    body.extend_from_slice(&1u16.to_le_bytes());
    body.extend_from_slice(&mask.to_le_bytes());
    dispatch(f, GRABBER, 2, XI2_OPCODE, XI_GRAB_DEVICE, &body);
    let grabbed_owner = if device == XI_MASTER {
        f.state.active_pointer_grab.map(|grab| grab.owner)
    } else {
        f.state
            .xi2_pointer_grabs
            .get(&device)
            .map(|grab| grab.owner)
    };
    assert_eq!(
        grabbed_owner,
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
    inject_from_origin(
        f,
        crate::core_loop::InputOrigin::XTest(XI_XTEST),
        kind,
        x,
        y,
        host_xid,
    );
}

fn inject_from_origin(
    f: &mut Fixture,
    origin: crate::core_loop::InputOrigin,
    kind: PointerEventKind,
    x: i16,
    y: i16,
    host_xid: u32,
) {
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
            origin,
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

fn xi2_geometry(bytes: &[u8]) -> Vec<(u32, u32, i16, i16, i16, i16)> {
    let mut result = Vec::new();
    let mut offset = 0;
    while offset + 32 <= bytes.len() {
        if bytes[offset] != 35 {
            offset += 32;
            continue;
        }
        let units = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap());
        let kind = u16::from_le_bytes([bytes[offset + 8], bytes[offset + 9]]);
        if (4..=6).contains(&kind) {
            result.push((
                u32::from_le_bytes(bytes[offset + 24..offset + 28].try_into().unwrap()),
                u32::from_le_bytes(bytes[offset + 28..offset + 32].try_into().unwrap()),
                i16::from_le_bytes(bytes[offset + 34..offset + 36].try_into().unwrap()),
                i16::from_le_bytes(bytes[offset + 38..offset + 40].try_into().unwrap()),
                i16::from_le_bytes(bytes[offset + 42..offset + 44].try_into().unwrap()),
                i16::from_le_bytes(bytes[offset + 46..offset + 48].try_into().unwrap()),
            ));
        }
        offset += 32 + usize::try_from(units).unwrap() * 4;
    }
    assert_eq!(offset, bytes.len(), "event stream fully consumed");
    result
}

fn xi1_valuator_values(bytes: &[u8]) -> Vec<(u8, i32)> {
    let mut values = Vec::new();
    let mut offset = 0;
    while offset + 32 <= bytes.len() {
        if bytes[offset] == crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_VALUATOR_OFFSET
        {
            let count = bytes[offset + 6].min(6);
            let first = bytes[offset + 7];
            for index in 0..count {
                let start = offset + 8 + usize::from(index) * 4;
                let value = i32::from_le_bytes(bytes[start..start + 4].try_into().unwrap());
                values.push((first + index, value));
            }
        }
        offset += 32;
    }
    assert_eq!(offset, bytes.len(), "XI1 wire packets fully consumed");
    values
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
fn owner_events_master_grab_leaves_slave_form_to_selectors() {
    // C5: the master grab redirects only the master form; B keeps its natural slave form.
    let mut f = fixture();
    select_xi2(&mut f, OTHER, WINDOW, XI_XTEST, XI_MOTION);
    grab_xi2(&mut f, WINDOW);
    inject(
        &mut f,
        PointerEventKind::MotionNotify,
        150,
        150,
        HOST_WINDOW,
    );

    let owner_bytes = read_all_capture_available(&mut f.grabber);
    let other_bytes = read_all_capture_available(&mut f.other);
    assert_eq!(
        decode_deliveries(&owner_bytes),
        vec![Delivery {
            xi2: true,
            kind: 6,
            window: WINDOW,
            child: CHILD,
        }],
        "C5 master form goes to the grab owner"
    );
    assert_eq!(
        xi2_event_ids(&owner_bytes),
        vec![(6, XI_MASTER, XI_XTEST)],
        "C5 owner receives the master-stamped form"
    );
    assert_eq!(
        xi2_event_ids(&other_bytes),
        vec![(6, XI_XTEST, XI_XTEST)],
        "C5 other client keeps the independently selected slave form"
    );
    assert_eq!(
        decode_deliveries(&other_bytes),
        vec![Delivery {
            xi2: true,
            kind: 6,
            window: WINDOW,
            child: CHILD,
        }]
    );
}

#[test]
fn slave_grab_leaves_master_form_from_other_source() {
    // C15: a slave grab on one device must not consume another source's master copy.
    const GRABBED_SOURCE: u64 = 0xA7551;
    let mut f = fixture();
    let grabbed_device = pointer_source(&mut f.state, GRABBED_SOURCE, true);
    let injecting_device = XI_XTEST;
    assert_ne!(
        grabbed_device, injecting_device,
        "C15 grabbed slave id must differ from the injecting source id"
    );
    select_xi2(&mut f, OTHER, CHILD, XI_MASTER, XI_MOTION);
    grab_xi2_device_with_options(&mut f, WINDOW, grabbed_device, true, XI_GRAB_MASK);
    inject(
        &mut f,
        PointerEventKind::MotionNotify,
        150,
        150,
        HOST_WINDOW,
    );

    let bytes = read_all_capture_available(&mut f.other);
    assert_eq!(
        xi2_event_ids(&bytes),
        vec![(6, XI_MASTER, injecting_device)],
        "C15 delivers the other source's master form selected on D"
    );
    assert_eq!(
        decode_deliveries(&bytes),
        vec![Delivery {
            xi2: true,
            kind: 6,
            window: CHILD,
            child: 0,
        }]
    );
    assert!(read_deliveries(&mut f.grabber).is_empty());
}

#[test]
fn owner_events_fallback_child_toward_pointer() {
    // C2/C13: Xorg reports the direct child of W for fallback Motion, Press, and Release.
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
    inject(&mut f, PointerEventKind::ButtonPress, 150, 150, HOST_WINDOW);
    inject(
        &mut f,
        PointerEventKind::ButtonRelease,
        150,
        150,
        HOST_WINDOW,
    );

    let owner_events = read_deliveries(&mut f.grabber);
    assert_eq!(
        owner_events,
        vec![
            Delivery {
                xi2: true,
                kind: 6,
                window: WINDOW,
                child: CHILD
            },
            Delivery {
                xi2: true,
                kind: 4,
                window: WINDOW,
                child: CHILD
            },
            Delivery {
                xi2: true,
                kind: 5,
                window: WINDOW,
                child: CHILD
            },
        ],
        "C2 and C13 fallback events carry the child toward the pointer"
    );
    assert!(read_deliveries(&mut f.other).is_empty());
}

#[test]
fn owner_events_fallback_outside_tree_geometry() {
    // C12: fallback stays relative to grab window W when the pointer is over OUT.
    let mut f = fixture();
    grab_xi2(&mut f, WINDOW);
    inject(
        &mut f,
        PointerEventKind::MotionNotify,
        650,
        150,
        HOST_OUTSIDE,
    );

    let bytes = read_all_capture_available(&mut f.grabber);
    assert_eq!(
        decode_deliveries(&bytes),
        vec![Delivery {
            xi2: true,
            kind: 6,
            window: WINDOW,
            child: 0,
        }]
    );
    assert_eq!(
        xi2_geometry(&bytes),
        vec![(WINDOW, 0, 650, 150, 550, 50)],
        "C12 event and root coordinates remain anchored to the grab window"
    );
}

#[test]
fn e27_release_child_toward_pointer() {
    // C9: release forms on W report child D, as measured on Xvfb 21.1.24.
    let mut f = fixture();
    select_xi2(&mut f, GRABBER, WINDOW, XI_XTEST, XI_BUTTONS);
    select_xi2(&mut f, GRABBER, WINDOW, XI_MASTER, XI_BUTTONS);
    inject(&mut f, PointerEventKind::ButtonPress, 150, 150, HOST_WINDOW);
    inject(
        &mut f,
        PointerEventKind::ButtonRelease,
        150,
        150,
        HOST_WINDOW,
    );

    let owner_bytes = read_all_capture_available(&mut f.grabber);
    assert_eq!(
        xi2_event_ids(&owner_bytes),
        vec![
            (4, XI_XTEST, XI_XTEST),
            (4, XI_MASTER, XI_XTEST),
            (5, XI_XTEST, XI_XTEST),
            (5, XI_MASTER, XI_XTEST),
        ],
        "C9 contains slave 4/source 4 then master 2/source 4 for press and release"
    );
    assert_eq!(
        decode_deliveries(&owner_bytes),
        vec![
            Delivery {
                xi2: true,
                kind: 4,
                window: WINDOW,
                child: CHILD
            },
            Delivery {
                xi2: true,
                kind: 4,
                window: WINDOW,
                child: CHILD
            },
            Delivery {
                xi2: true,
                kind: 5,
                window: WINDOW,
                child: CHILD
            },
            Delivery {
                xi2: true,
                kind: 5,
                window: WINDOW,
                child: CHILD
            },
        ],
        "C9 press and release both keep W/D for the slave and master forms"
    );
    assert!(read_all_capture_available(&mut f.other).is_empty());
}

#[test]
fn e27_slave_xi2_delivered_before_master_core() {
    // C9e: Xorg processes the slave event before its master copy (../xserver/mi/mieq.c:502-508).
    let mut f = fixture();
    select_core(&mut f, GRABBER, WINDOW, CORE_BUTTONS);
    select_xi2(&mut f, GRABBER, WINDOW, XI_XTEST, XI_BUTTONS);
    inject(&mut f, PointerEventKind::ButtonPress, 150, 150, HOST_WINDOW);
    inject(
        &mut f,
        PointerEventKind::ButtonRelease,
        150,
        150,
        HOST_WINDOW,
    );

    let owner_bytes = read_all_capture_available(&mut f.grabber);
    assert_eq!(
        xi2_event_ids(&owner_bytes),
        vec![(4, XI_XTEST, XI_XTEST), (5, XI_XTEST, XI_XTEST)],
        "C9e's XI2 packets are slave 4/source 4 and have no master XI2 selection"
    );
    assert!(xi1_device_event_ids(&owner_bytes).is_empty());
    assert_eq!(
        decode_deliveries(&owner_bytes),
        vec![
            Delivery {
                xi2: true,
                kind: 4,
                window: WINDOW,
                child: CHILD
            },
            Delivery {
                xi2: false,
                kind: 4,
                window: WINDOW,
                child: CHILD
            },
            Delivery {
                xi2: true,
                kind: 5,
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
        "C9e delivers slave XI2 before the master-form core event for press and release"
    );
    assert!(read_all_capture_available(&mut f.other).is_empty());
}

#[test]
fn owner_events_slave_xi1_selection_only_reaches_grab_owner() {
    const SOURCE: u64 = 0xA7561;
    let mut f = fixture();
    let pointer = pointer_source(&mut f.state, SOURCE, true);
    select_xi1_motion(&mut f, GRABBER, WINDOW, pointer);
    select_xi1_motion(&mut f, OTHER, WINDOW, pointer);
    grab_xi2_device_with_options(&mut f, WINDOW, pointer, true, XI_MOTION);

    inject_from_origin(
        &mut f,
        crate::core_loop::InputOrigin::Physical(crate::xinput::InputSourceId(SOURCE)),
        PointerEventKind::MotionNotify,
        150,
        150,
        HOST_WINDOW,
    );

    assert_eq!(
        xi1_device_event_ids(&read_all_capture_available(&mut f.grabber)),
        vec![(
            crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_MOTION_NOTIFY_OFFSET,
            pointer as u8,
        )],
        "the XI1 grab owner receives the natural slave event"
    );
    assert!(
        read_all_capture_available(&mut f.other).is_empty(),
        "a foreign XI1 selector on the owner's natural window is rejected by the grab"
    );
}

#[test]
fn owner_events_slave_xi2_delivery_suppresses_xi1_duplicate() {
    const SOURCE: u64 = 0xA7562;
    let mut f = fixture();
    let pointer = pointer_source(&mut f.state, SOURCE, true);
    select_xi1_motion(&mut f, GRABBER, WINDOW, pointer);
    select_xi2(&mut f, GRABBER, WINDOW, pointer, XI_MOTION);
    grab_xi2_device_with_options(&mut f, WINDOW, pointer, true, XI_MOTION);

    inject_from_origin(
        &mut f,
        crate::core_loop::InputOrigin::Physical(crate::xinput::InputSourceId(SOURCE)),
        PointerEventKind::MotionNotify,
        150,
        150,
        HOST_WINDOW,
    );

    let owner_bytes = read_all_capture_available(&mut f.grabber);
    assert_eq!(
        xi2_event_ids(&owner_bytes),
        vec![(6, pointer, pointer)],
        "XI2 succeeds first for this slave event"
    );
    assert!(xi1_device_event_ids(&owner_bytes).is_empty());
    assert!(read_all_capture_available(&mut f.other).is_empty());
}

#[test]
fn xtest_device_motion_owner_events_and_exclusive_grabs_route_xi2() {
    // C16/C17: direct XTestFakeDeviceMotionEvent goes through Xorg's
    // grabbed device route. Owner-events chooses natural XI2 before XI1;
    // an exclusive grab uses its XI2 Motion mask and neither leaks XI1.
    let mut f = fixture();
    inject_from_origin(
        &mut f,
        crate::core_loop::InputOrigin::XTest(XI_XTEST),
        PointerEventKind::MotionNotify,
        150,
        150,
        HOST_WINDOW,
    );
    select_xi1_motion(&mut f, GRABBER, WINDOW, XI_XTEST);
    select_xi2(&mut f, GRABBER, WINDOW, XI_XTEST, XI_MOTION);
    grab_xi2_device_with_options(&mut f, WINDOW, XI_XTEST, true, XI_MOTION);
    fake_device_motion(&mut f, XI_XTEST, 0, &[150, 150]);

    let owner_bytes = read_all_capture_available(&mut f.grabber);
    assert_eq!(
        xi2_event_ids(&owner_bytes),
        vec![(6, XI_XTEST, XI_XTEST)],
        "C16: owner-events chooses one slave XI_Motion"
    );
    assert_eq!(
        decode_deliveries(&owner_bytes),
        vec![Delivery {
            xi2: true,
            kind: 6,
            window: WINDOW,
            child: CHILD,
        }],
        "C16: the natural XI2 selection reports W with child D"
    );
    assert_eq!(
        xi2_geometry(&owner_bytes),
        vec![(WINDOW, CHILD, 150, 150, 50, 50)],
        "C16: the event uses the request's absolute X/Y valuators"
    );
    assert!(xi1_device_event_ids(&owner_bytes).is_empty());
    assert!(read_all_capture_available(&mut f.other).is_empty());

    let mut exclusive = fixture();
    inject_from_origin(
        &mut exclusive,
        crate::core_loop::InputOrigin::XTest(XI_XTEST),
        PointerEventKind::MotionNotify,
        150,
        150,
        HOST_WINDOW,
    );
    select_xi1_motion(&mut exclusive, GRABBER, WINDOW, XI_XTEST);
    grab_xi2_device_with_options(&mut exclusive, WINDOW, XI_XTEST, false, XI_MOTION);
    fake_device_motion(&mut exclusive, XI_XTEST, 0, &[150, 150]);

    let owner_bytes = read_all_capture_available(&mut exclusive.grabber);
    assert_eq!(
        xi2_event_ids(&owner_bytes),
        vec![(6, XI_XTEST, XI_XTEST)],
        "C17: the exclusive grab's Motion mask receives slave XI_Motion"
    );
    assert_eq!(
        decode_deliveries(&owner_bytes),
        vec![Delivery {
            xi2: true,
            kind: 6,
            window: WINDOW,
            child: CHILD,
        }],
        "C17: exclusive fallback reports the grab window and its child toward the pointer"
    );
    assert_eq!(
        xi2_geometry(&owner_bytes),
        vec![(WINDOW, CHILD, 150, 150, 50, 50)]
    );
    assert!(xi1_device_event_ids(&owner_bytes).is_empty());
    assert!(read_all_capture_available(&mut exclusive.other).is_empty());

    let mut xi1_only = fixture();
    inject_from_origin(
        &mut xi1_only,
        crate::core_loop::InputOrigin::XTest(XI_XTEST),
        PointerEventKind::MotionNotify,
        150,
        150,
        HOST_WINDOW,
    );
    select_xi1_motion(&mut xi1_only, GRABBER, WINDOW, XI_XTEST);
    grab_xi2_device_with_options(&mut xi1_only, WINDOW, XI_XTEST, true, XI_MOTION);
    fake_device_motion(&mut xi1_only, XI_XTEST, 0, &[150, 150]);

    let owner_bytes = read_all_capture_available(&mut xi1_only.grabber);
    assert!(xi2_event_ids(&owner_bytes).is_empty());
    assert_eq!(
        xi1_device_event_ids(&owner_bytes),
        vec![(
            crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_MOTION_NOTIFY_OFFSET,
            XI_XTEST as u8,
        )],
        "C10: XI1 still wins when the grab owner has no XI2 selection"
    );
    assert_eq!(owner_bytes[31], XI_XTEST as u8 | 0x80, "C10 MORE_EVENTS");
    assert_eq!(
        xi1_valuator_values(&owner_bytes),
        vec![(0, 150), (1, 150)],
        "C10: direct FakeInput keeps both explicit device valuators"
    );
}

#[test]
fn xtest_device_motion_preserves_partial_valuator_range() {
    // C18: Xvfb 21.1.24 receives a Y-only DeviceValuator range (axis 1,
    // value 175) and emits XI_Motion with only axis 1, whose measured value
    // is 0. Xorg's XTest range-copy behavior is recorded in the findings doc.
    let mut f = fixture();
    inject_from_origin(
        &mut f,
        crate::core_loop::InputOrigin::XTest(XI_XTEST),
        PointerEventKind::MotionNotify,
        150,
        150,
        HOST_WINDOW,
    );
    select_xi1_motion(&mut f, GRABBER, WINDOW, XI_XTEST);
    select_xi2(&mut f, GRABBER, WINDOW, XI_XTEST, XI_MOTION);
    grab_xi2_device_with_options(&mut f, WINDOW, XI_XTEST, true, XI_MOTION);
    fake_device_motion(&mut f, XI_XTEST, 1, &[175]);

    let owner_bytes = read_all_capture_available(&mut f.grabber);
    assert_eq!(xi2_event_ids(&owner_bytes), vec![(6, XI_XTEST, XI_XTEST)]);
    assert_eq!(
        u32::from_le_bytes(owner_bytes[112..116].try_into().unwrap()),
        0x2,
        "C18: XI_Motion carries only the requested Y valuator bit"
    );
    assert_eq!(
        u32::from_le_bytes(owner_bytes[116..120].try_into().unwrap()),
        0,
        "C18: second valuator mask word is empty"
    );
    assert_eq!(
        i32::from_le_bytes(owner_bytes[120..124].try_into().unwrap()),
        0,
        "C18: Xvfb 21.1.24 emits Y=0 for this nonzero first-valuator range"
    );
    assert_eq!(
        u32::from_le_bytes(owner_bytes[124..128].try_into().unwrap()),
        0,
        "C18: Y fraction is zero"
    );
    assert!(xi1_device_event_ids(&owner_bytes).is_empty());
    assert!(read_all_capture_available(&mut f.other).is_empty());
}

#[test]
fn exclusive_slave_grab_suppresses_foreign_xi1_selection() {
    const SOURCE: u64 = 0xA7563;
    let mut f = fixture();
    let pointer = pointer_source(&mut f.state, SOURCE, true);
    select_xi1_motion(&mut f, OTHER, WINDOW, pointer);
    grab_xi2_device_with_options(&mut f, WINDOW, pointer, false, XI_MOTION);

    inject_from_origin(
        &mut f,
        crate::core_loop::InputOrigin::Physical(crate::xinput::InputSourceId(SOURCE)),
        PointerEventKind::MotionNotify,
        150,
        150,
        HOST_WINDOW,
    );

    assert_eq!(
        xi2_event_ids(&read_all_capture_available(&mut f.grabber)),
        vec![(6, pointer, pointer)],
        "the exclusive XI2 grab receives its selected Motion"
    );
    assert!(
        read_all_capture_available(&mut f.other).is_empty(),
        "the foreign XI1 selection does not bypass the exclusive slave grab"
    );
}

#[test]
fn exclusive_slave_grab_scroll_motion_reaches_only_grab_owner() {
    // Xorg creates the smooth Motion before the wheel button edge and sends
    // grabbed XI2 events through the grab's Motion mask and window
    // (../xserver/dix/getevents.c:1646-1697, 1703-1718;
    // ../xserver/dix/events.c:4354-4394, 4431-4462).
    const SOURCE: u64 = 0xA7564;
    let mut f = fixture();
    let pointer = pointer_source(&mut f.state, SOURCE, true);
    select_xi2(&mut f, OTHER, CHILD, XI_MASTER, XI_MOTION);
    grab_xi2_device_with_options(&mut f, WINDOW, pointer, false, XI_MOTION);

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

    let owner_bytes = read_all_capture_available(&mut f.grabber);
    assert_eq!(
        xi2_event_ids(&owner_bytes),
        vec![(6, pointer, pointer)],
        "the Motion-only slave grab receives the exact slave-stamped smooth Motion"
    );
    assert_eq!(
        decode_deliveries(&owner_bytes),
        vec![Delivery {
            xi2: true,
            kind: 6,
            window: WINDOW,
            child: 0,
        }]
    );
    assert_eq!(
        xi2_geometry(&owner_bytes),
        vec![(WINDOW, 0, 150, 150, 50, 50)],
        "the exclusive slave Motion uses grab-window-relative geometry"
    );
    assert!(
        read_all_capture_available(&mut f.other).is_empty(),
        "the unrelated master selector does not receive grabbed scroll Motion"
    );
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
fn owner_events_scroll_motion_routed_independently_of_wheel_press() {
    // Smooth-scroll Motion is a separate event from the wheel press. Xorg
    // routes it through its own owner-events attempt and falls back when the
    // only natural selector belongs to another client
    // (../xserver/dix/getevents.c:1646-1692;
    // ../xserver/dix/events.c:2071-2075,4431-4462). C2 measures child D
    // on this same W fallback, and Xorg applies FixUpEventFromWindow to the
    // scroll Motion as well (../xserver/dix/events.c:4388-4389).
    const SOURCE: u64 = 0xA7533;
    let mut f = fixture();
    let pointer = pointer_source(&mut f.state, SOURCE, true);
    select_xi2(&mut f, GRABBER, CHILD, XI_MASTER, 1 << 4);
    select_xi2(&mut f, OTHER, CHILD, XI_MASTER, XI_MOTION);
    grab_xi2_with_options(&mut f, WINDOW, true, XI_MOTION | (1 << 4));

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
                child: CHILD,
            },
            Delivery {
                xi2: true,
                kind: 4,
                window: CHILD,
                child: 0,
            },
        ],
        "the Motion falls back to W while the wheel press is delivered naturally on D"
    );
    assert_eq!(
        xi2_event_ids(&grabber_bytes),
        vec![(6, XI_MASTER, pointer), (4, XI_MASTER, pointer)]
    );
    assert!(
        read_all_capture_available(&mut f.other).is_empty(),
        "B's Motion selection on D is rejected by the owner-events grab"
    );
}

#[test]
fn owner_events_scroll_motion_has_no_core_attempt() {
    // A wheel-only Motion has no X/Y and Xorg rejects its core conversion
    // (../xserver/dix/eventconvert.c:113-122). Its owner-events walk must
    // still fall back to the grab window when B owns the XI2 Motion attempt.
    // C2 measures child D for that W fallback; the same Xorg fixup applies
    // to smooth-scroll Motion (../xserver/dix/events.c:4388-4389).
    const SOURCE: u64 = 0xA7534;
    let mut f = fixture();
    let pointer = pointer_source(&mut f.state, SOURCE, true);
    select_xi2(&mut f, GRABBER, CHILD, XI_MASTER, 1 << 4);
    select_core(&mut f, GRABBER, CHILD, CORE_MOTION);
    select_xi2(&mut f, OTHER, CHILD, XI_MASTER, XI_MOTION);
    grab_xi2_with_options(&mut f, WINDOW, true, XI_MOTION | (1 << 4));

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
                child: CHILD,
            },
            Delivery {
                xi2: true,
                kind: 4,
                window: CHILD,
                child: 0,
            },
        ],
        "the scroll Motion is XI2-only and falls back to W despite A's core selection on D"
    );
    assert_eq!(
        xi2_event_ids(&grabber_bytes),
        vec![(6, XI_MASTER, pointer), (4, XI_MASTER, pointer)]
    );
    assert!(
        read_all_capture_available(&mut f.other).is_empty(),
        "B's Motion selection on D is rejected by the owner-events grab"
    );
}

#[test]
fn owner_events_scroll_master_fallback_keeps_slave_geometry() {
    // C2 measures child D on the master fallback to W; C5 establishes that
    // an ungrabbed slave form keeps its own natural delivery independently
    // of the master grab. The scroll Motion uses the same Xorg grabbed-event
    // fixup (../xserver/dix/events.c:4388-4389), and Xorg computes event
    // coordinates relative to each reported window (../xserver/dix/events.c:2634-2637).
    const SOURCE: u64 = 0xA7535;
    let mut f = fixture();
    let pointer = pointer_source(&mut f.state, SOURCE, true);
    select_xi2(&mut f, GRABBER, CHILD, pointer, XI_MOTION);
    select_xi2(&mut f, GRABBER, CHILD, XI_MASTER, 1 << 4);
    select_xi2(&mut f, OTHER, CHILD, XI_MASTER, XI_MOTION);
    grab_xi2_with_options(&mut f, WINDOW, true, XI_MOTION | (1 << 4));

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
                window: CHILD,
                child: 0,
            },
            Delivery {
                xi2: true,
                kind: 6,
                window: WINDOW,
                child: CHILD,
            },
            Delivery {
                xi2: true,
                kind: 4,
                window: CHILD,
                child: 0,
            },
        ],
        "only the master Motion uses W fallback geometry; A's slave Motion stays on D"
    );
    assert_eq!(
        xi2_event_ids(&grabber_bytes),
        vec![
            (6, pointer, pointer),
            (6, XI_MASTER, pointer),
            (4, XI_MASTER, pointer),
        ]
    );
    assert_eq!(
        xi2_geometry(&grabber_bytes),
        vec![
            (CHILD, 0, 150, 150, 30, 30),
            (WINDOW, CHILD, 150, 150, 50, 50),
            (CHILD, 0, 150, 150, 30, 30),
        ],
        "the slave stays D-relative while the master fallback is W-relative"
    );
    assert!(
        read_all_capture_available(&mut f.other).is_empty(),
        "B's master Motion attempt is rejected by the owner-events grab"
    );
}

#[test]
fn owner_events_scroll_motion_mask_can_reject_fallback_without_leaking() {
    // Xorg rejects foreign natural delivery before applying the grab's XI2
    // mask to fallback (../xserver/dix/events.c:4357-4365,4395-4397,4431-4462).
    const SOURCE: u64 = 0xA7536;
    let mut f = fixture();
    let pointer = pointer_source(&mut f.state, SOURCE, true);
    select_xi2(&mut f, GRABBER, CHILD, XI_MASTER, 1 << 4);
    select_xi2(&mut f, OTHER, CHILD, XI_MASTER, XI_MOTION);
    grab_xi2_with_options(&mut f, WINDOW, true, 1 << 4);

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
        vec![Delivery {
            xi2: true,
            kind: 4,
            window: CHILD,
            child: 0,
        }],
        "the button remains natural while the grab mask rejects fallback Motion"
    );
    assert_eq!(xi2_event_ids(&grabber_bytes), vec![(4, XI_MASTER, pointer)]);
    assert!(
        read_all_capture_available(&mut f.other).is_empty(),
        "B's natural Motion is rejected even though the grab mask excludes Motion"
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
