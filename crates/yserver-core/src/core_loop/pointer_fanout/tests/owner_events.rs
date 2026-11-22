use super::*;

use crate::{
    backend::recording::RecordingBackend,
    core_loop::{DeviceInfo, message::LibinputConfigSnapshot},
    resources::{ROOT_VISUAL, ROOT_WINDOW},
};
use yserver_protocol::x11::{RequestHeader, ResourceId, SequenceNumber};

const GRABBER: u32 = 0xA753;
const OTHER: u32 = 0xA754;
const WINDOW: u32 = 0x0010_A753;
const CHILD: u32 = 0x0020_A753;
const OUTSIDE: u32 = 0x0010_A754;
const PARENT: u32 = 0x0050_A753;
const HOST_WINDOW: u32 = 0xCAFE_A753;
const HOST_OUTSIDE: u32 = 0xCAFE_A754;
const XI2_OPCODE: u8 = 137;
const XI_SELECT_EVENTS: u8 = 46;
const XI_GRAB_DEVICE: u8 = 51;
const XI_UNGRAB_DEVICE: u8 = 52;
const XI_ALLOW_EVENTS: u8 = 53;
const XI_PASSIVE_GRAB_DEVICE: u8 = 54;
const CORE_UNMAP_WINDOW: u8 = 10;
const CORE_UNMAP_SUBWINDOWS: u8 = 11;
const XI_SELECT_EXTENSION_EVENT: u8 = 6;
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
    fixture_with_window_owners(child_owner, GRABBER)
}

fn fixture_with_window_owners(child_owner: u32, outside_owner: u32) -> Fixture {
    fixture_with_window_owners_and_parent(child_owner, outside_owner, false)
}

fn fixture_with_parent() -> Fixture {
    fixture_with_window_owners_and_parent(OTHER, GRABBER, true)
}

fn fixture_with_window_owners_and_parent(
    child_owner: u32,
    outside_owner: u32,
    parented: bool,
) -> Fixture {
    let mut state = ServerState::new();
    let grabber = install_capture_client(&mut state, GRABBER);
    let other = install_capture_client(&mut state, OTHER);
    let backend = RecordingBackend::new();
    if parented {
        state.resources.create_window(
            ClientId(GRABBER),
            yserver_protocol::x11::CreateWindowRequest {
                depth: 24,
                window: ResourceId(PARENT),
                parent: ROOT_WINDOW,
                x: 100,
                y: 100,
                width: 200,
                height: 200,
                border_width: 0,
                class: 1,
                visual: ROOT_VISUAL,
                ..Default::default()
            },
        );
        let _ = state.resources.map_window(ResourceId(PARENT));
    }
    let window_parent = if parented {
        ResourceId(PARENT)
    } else {
        ROOT_WINDOW
    };
    let window_x = if parented { 0 } else { 100 };
    let window_y = if parented { 0 } else { 100 };
    for (owner, window, parent, x, y, width, height) in [
        (GRABBER, WINDOW, window_parent, window_x, window_y, 200, 200),
        (child_owner, CHILD, ResourceId(WINDOW), 20, 20, 100, 100),
        (outside_owner, OUTSIDE, ROOT_WINDOW, 550, 100, 300, 300),
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

fn register_test_pointer_source(
    f: &mut Fixture,
    source_id: u64,
) -> (crate::xinput::InputSourceId, u16) {
    let source = crate::xinput::InputSourceId(source_id);
    let info = DeviceInfo {
        source_id: source,
        enabled: true,
        resume_key: None,
        capabilities: crate::xinput::InputCapabilities {
            keyboard: false,
            pointer: true,
            touch: false,
        },
        name: "unmap queue test pointer".to_owned(),
        device_node: "/dev/input/unmap-queue-test".to_owned(),
        sysname: "unmap-queue-test".to_owned(),
        vendor_id: 0,
        product_id: 0,
        is_touchpad: false,
        config: LibinputConfigSnapshot::default(),
    };
    let devices = f.state.xi_register_source(&info);
    assert_eq!(devices.len(), 1, "test source registers one pointer facet");
    (source, devices[0])
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

fn grab_xi2_passive_button_sync(
    f: &mut Fixture,
    client: u32,
    window: u32,
    device: u16,
    button: u32,
    event_mask: u32,
) {
    let mut body = Vec::with_capacity(36);
    body.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    body.extend_from_slice(&window.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // no cursor
    body.extend_from_slice(&button.to_le_bytes());
    body.extend_from_slice(&device.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes()); // AnyModifier
    body.extend_from_slice(&1u16.to_le_bytes()); // one XI2 event-mask word
    body.extend_from_slice(&[0, 0, 1, 0]); // button, sync pointer, async paired, owner_events=false
    body.extend_from_slice(&[0; 2]); // pad
    body.extend_from_slice(&event_mask.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // AnyModifier
    dispatch(f, client, 2, XI2_OPCODE, XI_PASSIVE_GRAB_DEVICE, &body);
    let _ = read_all_capture_available(if client == GRABBER {
        &mut f.grabber
    } else {
        &mut f.other
    }); // reply
    let _ = read_all_capture_available(&mut f.other);
}

fn allow_xi2_replay_device(f: &mut Fixture, device: u16) {
    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    body.extend_from_slice(&device.to_le_bytes());
    body.extend_from_slice(&[2, 0]); // XIReplayDevice
    dispatch(f, GRABBER, 4, XI2_OPCODE, XI_ALLOW_EVENTS, &body);
    let _ = read_all_capture_available(&mut f.grabber);
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
    let detail = if matches!(
        kind,
        PointerEventKind::ButtonPress | PointerEventKind::ButtonRelease
    ) {
        1
    } else {
        0
    };
    inject_from_origin_with_detail(f, origin, kind, detail, x, y, host_xid);
}

fn inject_from_origin_with_detail(
    f: &mut Fixture,
    origin: crate::core_loop::InputOrigin,
    kind: PointerEventKind,
    detail: u8,
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
            detail,
            time: 50,
            root_x: x,
            root_y: y,
            event_x: x - host_x,
            event_y: y - host_y,
            state: if kind == PointerEventKind::ButtonRelease {
                1u16 << (7 + detail)
            } else {
                0
            },
            crossing_mode: 0,
            child: 0,
            raw_dx: 0,
            raw_dy: 0,
            tree_change: false,
            device_axes: None,
        },
        true,
        false,
    );
}

fn inject_physical_wheel(f: &mut Fixture, source: u64, time: u32) {
    inject_physical_button(f, source, PointerEventKind::ButtonPress, 5, time);
}

fn inject_physical_button(
    f: &mut Fixture,
    source: u64,
    kind: PointerEventKind,
    detail: u8,
    time: u32,
) {
    let dropped = pointer_event_fanout_to_state(
        &mut f.state,
        &mut f.backend,
        &f.xid_map,
        HostPointerEvent {
            origin: crate::core_loop::InputOrigin::Physical(crate::xinput::InputSourceId(source)),
            kind,
            host_xid: HOST_WINDOW,
            detail,
            time,
            root_x: 150,
            root_y: 150,
            event_x: 50,
            event_y: 50,
            state: if kind == PointerEventKind::ButtonRelease && detail <= 8 {
                1 << (7 + detail)
            } else {
                0
            },
            ..motion_event()
        },
        true,
        false,
    );
    assert!(dropped.is_empty());
}

fn xi2_motion_windows(bytes: &[u8]) -> Vec<(u32, u32)> {
    decode_deliveries(bytes)
        .into_iter()
        .filter(|event| event.xi2 && event.kind == 6)
        .map(|event| (event.window, event.child))
        .collect()
}

fn xi2_motion_forms(bytes: &[u8]) -> Vec<(u16, u16)> {
    xi2_event_ids(bytes)
        .into_iter()
        .filter_map(|(kind, device, source)| (kind == 6).then_some((device, source)))
        .collect()
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
    // The oracle prepositions the sprite at (145,145) inside W before the case
    // (PREPOSITION XTestFakeMotionEvent); the slave form routes from that
    // pre-move window (Xvfb 21.1.24 measurement, spec invariant 8).
    inject(
        &mut f,
        PointerEventKind::MotionNotify,
        145,
        145,
        HOST_WINDOW,
    );
    let _ = read_all_capture_available(&mut f.grabber);
    let _ = read_all_capture_available(&mut f.other);
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
fn exclusive_slave_grab_physical_motion_reports_child_under_pointer() {
    // C17: Xorg FixUpEventFromWindow reports child D for exclusive XI2 grab
    // fallback, independent of owner_events (../xserver/dix/events.c:4389).
    const SOURCE: u64 = 0xA7565;
    let mut f = fixture();
    let pointer = pointer_source(&mut f.state, SOURCE, true);
    grab_xi2_device_with_options(&mut f, WINDOW, pointer, false, XI_MOTION);

    inject_from_origin(
        &mut f,
        crate::core_loop::InputOrigin::Physical(crate::xinput::InputSourceId(SOURCE)),
        PointerEventKind::MotionNotify,
        150,
        150,
        HOST_WINDOW,
    );

    let owner = read_all_capture_available(&mut f.grabber);
    assert_eq!(
        decode_deliveries(&owner),
        vec![Delivery {
            xi2: true,
            kind: 6,
            window: WINDOW,
            child: CHILD,
        }],
        "the exclusive slave-form grab reports the child under the pointer"
    );
    assert_eq!(
        xi2_geometry(&owner),
        vec![(WINDOW, CHILD, 150, 150, 50, 50)]
    );
    assert!(read_all_capture_available(&mut f.other).is_empty());
    assert_eq!(xi2_event_ids(&owner), vec![(6, pointer, pointer)]);
}

#[test]
fn exclusive_slave_grab_scroll_motion_reaches_only_grab_owner() {
    // C17 measured child D for exclusive fallback; Xorg always calls
    // FixUpEventFromWindow on the grab window (../xserver/dix/events.c:4389).
    // Xorg creates smooth Motion before the wheel button edge and sends
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
            child: CHILD,
        }]
    );
    assert_eq!(
        xi2_geometry(&owner_bytes),
        vec![(WINDOW, CHILD, 150, 150, 50, 50)],
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
    // The oracle prepositions the sprite at (145,145) inside W before the case
    // (PREPOSITION XTestFakeMotionEvent); the slave form routes from that
    // pre-move window (Xvfb 21.1.24 measurement, spec invariant 8).
    inject(
        &mut f,
        PointerEventKind::MotionNotify,
        145,
        145,
        HOST_WINDOW,
    );
    let _ = read_all_capture_available(&mut f.grabber);
    let _ = read_all_capture_available(&mut f.other);
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
    // The oracle prepositions the sprite at (145,145) inside W before the case
    // (PREPOSITION XTestFakeMotionEvent); the slave form routes from that
    // pre-move window (Xvfb 21.1.24 measurement, spec invariant 8).
    inject(
        &mut f,
        PointerEventKind::MotionNotify,
        145,
        145,
        HOST_WINDOW,
    );
    let _ = read_all_capture_available(&mut f.grabber);
    let _ = read_all_capture_available(&mut f.other);
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
fn exclusive_master_grab_physical_motion_reports_child_under_pointer() {
    // C17: Xorg FixUpEventFromWindow reports child D for exclusive XI2 grab
    // fallback, independent of owner_events (../xserver/dix/events.c:4389).
    const SOURCE: u64 = 0xA7532;
    let mut f = fixture();
    let pointer = pointer_source(&mut f.state, SOURCE, true);
    grab_xi2_with_options(&mut f, WINDOW, false, XI_MOTION);

    inject_from_origin(
        &mut f,
        crate::core_loop::InputOrigin::Physical(crate::xinput::InputSourceId(SOURCE)),
        PointerEventKind::MotionNotify,
        150,
        150,
        HOST_WINDOW,
    );

    let owner = read_all_capture_available(&mut f.grabber);
    assert_eq!(
        decode_deliveries(&owner),
        vec![Delivery {
            xi2: true,
            kind: 6,
            window: WINDOW,
            child: CHILD,
        }],
        "the exclusive master-form grab reports the child under the pointer"
    );
    assert_eq!(
        xi2_geometry(&owner),
        vec![(WINDOW, CHILD, 150, 150, 50, 50)]
    );
    assert!(read_all_capture_available(&mut f.other).is_empty());
    assert_eq!(xi2_event_ids(&owner), vec![(6, XI_MASTER, pointer)]);
}

#[test]
fn exclusive_master_grab_scroll_motion_reaches_only_grab_owner() {
    // C17 measured child D for exclusive fallback; Xorg always calls
    // FixUpEventFromWindow on the grab window (../xserver/dix/events.c:4389).
    // ../xserver/dix/events.c:4431-4462 filters a grabbed event through
    // the grab mask, and
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
                child: CHILD
            },
            Delivery {
                xi2: true,
                kind: 4,
                window: WINDOW,
                child: CHILD
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

#[test]
fn xi_sync_slave_passive_grab_delivers_activating_press_before_replay() {
    // C19 measured on Xvfb 21.1.24: the synchronous XI2 passive-grab owner
    // gets XI_ButtonPress/4 on W/D as device/source 4/4 before ReplayDevice;
    // B then gets a balanced slave press/release on D.
    let mut f = fixture();
    select_xi2(&mut f, OTHER, CHILD, XI_XTEST, XI_BUTTONS);
    grab_xi2_passive_button_sync(&mut f, GRABBER, WINDOW, XI_XTEST, 1, XI_BUTTONS);

    inject(&mut f, PointerEventKind::ButtonPress, 145, 145, HOST_WINDOW);
    let activating = read_all_capture_available(&mut f.grabber);
    assert_eq!(
        decode_deliveries(&activating),
        vec![Delivery {
            xi2: true,
            kind: 4,
            window: WINDOW,
            child: CHILD,
        }],
        "the passive-grab owner gets the activating XI2 press on the grab window"
    );
    assert_eq!(xi2_event_ids(&activating), vec![(4, XI_XTEST, XI_XTEST)]);
    assert_eq!(
        xi2_geometry(&activating),
        vec![(WINDOW, CHILD, 145, 145, 45, 45)]
    );
    assert_eq!(
        u32::from_le_bytes(activating[16..20].try_into().unwrap()),
        1,
        "the activating event is button 1"
    );
    assert!(read_all_capture_available(&mut f.other).is_empty());
    assert!(f.state.xi1_frozen[&XI_XTEST].frozen());

    inject(
        &mut f,
        PointerEventKind::ButtonRelease,
        145,
        145,
        HOST_WINDOW,
    );
    assert!(read_all_capture_available(&mut f.grabber).is_empty());
    assert!(read_all_capture_available(&mut f.other).is_empty());
    assert!(f.state.sync_pending.iter().any(|pending| {
        pending.device == XI_XTEST
            && matches!(
                pending.event,
                crate::server::QueuedInputEvent::HostPointer(HostPointerEvent {
                    kind: PointerEventKind::ButtonRelease,
                    ..
                })
            )
    }));

    allow_xi2_replay_device(&mut f, XI_XTEST);
    assert!(read_all_capture_available(&mut f.grabber).is_empty());
    let replayed = read_all_capture_available(&mut f.other);
    assert_eq!(
        decode_deliveries(&replayed),
        vec![
            Delivery {
                xi2: true,
                kind: 4,
                window: CHILD,
                child: 0,
            },
            Delivery {
                xi2: true,
                kind: 5,
                window: CHILD,
                child: 0,
            },
        ],
        "ReplayDevice sends one balanced natural press/release pair: {:?}",
        xi2_event_ids(&replayed)
    );
    assert_eq!(
        xi2_event_ids(&replayed),
        vec![(4, XI_XTEST, XI_XTEST), (5, XI_XTEST, XI_XTEST)]
    );
    assert_eq!(
        xi2_geometry(&replayed),
        vec![(CHILD, 0, 145, 145, 25, 25); 2]
    );
    assert!(f.state.sync_pending.is_empty());
    assert!(!f.state.xi1_frozen[&XI_XTEST].frozen());
    assert!(f.state.xi2_pointer_grabs.is_empty());
    assert_eq!(f.state.buttons_down, 0);
}

fn assert_passive_sync_unmap_replays_only_released_device(f: &mut Fixture, unmap_subwindows: bool) {
    let (unrelated_source, unrelated_device) = register_test_pointer_source(f, 0xD15);
    assert_ne!(unrelated_device, XI_XTEST);
    select_xi2(f, OTHER, OUTSIDE, XI_XTEST, XI_GRAB_MASK);
    select_xi2(f, OTHER, OUTSIDE, unrelated_device, XI_GRAB_MASK);

    grab_xi2_passive_button_sync(f, GRABBER, WINDOW, XI_XTEST, 1, XI_GRAB_MASK);
    inject(f, PointerEventKind::ButtonPress, 145, 145, HOST_WINDOW);
    let activating = read_all_capture_available(&mut f.grabber);
    assert_eq!(
        decode_deliveries(&activating)
            .iter()
            .map(|event| (event.xi2, event.kind, event.window))
            .collect::<Vec<_>>(),
        vec![(true, 4, WINDOW)],
        "the activating press goes to the target passive-grab owner once"
    );
    assert_eq!(xi2_event_ids(&activating), vec![(4, XI_XTEST, XI_XTEST)]);
    assert!(f.state.xi1_frozen[&XI_XTEST].frozen());
    assert!(f.state.xi1_frozen[&XI_XTEST].stored.is_some());
    assert!(f.state.xi2_pointer_grabs[&XI_XTEST].passive);

    inject(f, PointerEventKind::MotionNotify, 650, 150, HOST_OUTSIDE);
    inject(f, PointerEventKind::ButtonRelease, 650, 150, HOST_OUTSIDE);
    let target_pending: Vec<_> = f
        .state
        .sync_pending
        .iter()
        .filter(|pending| pending.device == XI_XTEST)
        .collect();
    assert_eq!(
        target_pending.len(),
        3,
        "pending events: {:?}",
        target_pending
    );
    assert!(target_pending.iter().any(|pending| matches!(
        pending.event,
        crate::server::QueuedInputEvent::HostPointer(HostPointerEvent {
            kind: PointerEventKind::MotionNotify,
            ..
        })
    )));
    assert!(target_pending.iter().any(|pending| matches!(
        pending.event,
        crate::server::QueuedInputEvent::HostPointer(HostPointerEvent {
            kind: PointerEventKind::ButtonRelease,
            ..
        })
    )));
    assert!(read_all_capture_available(&mut f.other).is_empty());

    // Keep a second client's sync hold alive on an independent physical
    // pointer so the post-unmap computation must leave that queue untouched.
    grab_xi2_passive_button_sync(f, OTHER, OUTSIDE, unrelated_device, 1, XI_GRAB_MASK);
    inject_from_origin_with_detail(
        f,
        crate::core_loop::InputOrigin::Physical(unrelated_source),
        PointerEventKind::ButtonPress,
        1,
        650,
        150,
        HOST_OUTSIDE,
    );
    let unrelated_activation = read_all_capture_available(&mut f.other);
    assert_eq!(
        decode_deliveries(&unrelated_activation),
        vec![Delivery {
            xi2: true,
            kind: 4,
            window: OUTSIDE,
            child: 0,
        }],
        "the other client's independent passive grab activates on OUT"
    );
    assert_eq!(
        xi2_event_ids(&unrelated_activation),
        vec![(4, unrelated_device, unrelated_device)]
    );
    assert!(f.state.xi1_frozen[&unrelated_device].frozen());
    assert!(f.state.xi1_frozen[&unrelated_device].stored.is_some());

    inject_from_origin(
        f,
        crate::core_loop::InputOrigin::Physical(unrelated_source),
        PointerEventKind::MotionNotify,
        650,
        150,
        HOST_OUTSIDE,
    );
    inject_from_origin_with_detail(
        f,
        crate::core_loop::InputOrigin::Physical(unrelated_source),
        PointerEventKind::ButtonRelease,
        1,
        650,
        150,
        HOST_OUTSIDE,
    );
    assert_eq!(f.state.sync_pending.len(), 6);
    let unrelated_pending: Vec<_> = f
        .state
        .sync_pending
        .iter()
        .filter(|pending| pending.device == unrelated_device)
        .collect();
    assert_eq!(unrelated_pending.len(), 3);
    assert!(unrelated_pending.iter().any(|pending| matches!(
        pending.event,
        crate::server::QueuedInputEvent::HostPointer(HostPointerEvent {
            kind: PointerEventKind::MotionNotify,
            ..
        })
    )));
    assert!(unrelated_pending.iter().any(|pending| matches!(
        pending.event,
        crate::server::QueuedInputEvent::HostPointer(HostPointerEvent {
            kind: PointerEventKind::ButtonRelease,
            ..
        })
    )));
    assert_eq!(
        f.state
            .sync_pending
            .iter()
            .filter(|pending| pending.device == XI_XTEST)
            .count(),
        3
    );
    assert_eq!(
        f.state
            .sync_pending
            .iter()
            .filter(|pending| pending.device == unrelated_device)
            .count(),
        3
    );
    assert!(read_all_capture_available(&mut f.grabber).is_empty());
    assert!(read_all_capture_available(&mut f.other).is_empty());

    if unmap_subwindows {
        dispatch(
            f,
            GRABBER,
            3,
            CORE_UNMAP_SUBWINDOWS,
            0,
            &PARENT.to_le_bytes(),
        );
    } else {
        dispatch(f, GRABBER, 3, CORE_UNMAP_WINDOW, 0, &WINDOW.to_le_bytes());
    }

    let replayed = read_all_capture_available(&mut f.other);
    assert_eq!(
        decode_deliveries(&replayed),
        vec![
            Delivery {
                xi2: true,
                kind: 6,
                window: OUTSIDE,
                child: 0,
            },
            Delivery {
                xi2: true,
                kind: 5,
                window: OUTSIDE,
                child: 0,
            },
        ],
        "unmapping delivers only the released device's queued motion/release; the activating press is discarded"
    );
    assert_eq!(
        xi2_event_ids(&replayed),
        vec![(6, XI_XTEST, XI_XTEST), (5, XI_XTEST, XI_XTEST)]
    );
    assert!(
        read_deliveries(&mut f.grabber).is_empty(),
        "unmap must not replay the discarded activating press to its grab owner"
    );
    assert_eq!(f.state.sync_pending.len(), 3);
    assert!(
        f.state
            .sync_pending
            .iter()
            .all(|pending| pending.device == unrelated_device)
    );

    assert!(!f.state.xi2_pointer_grabs.contains_key(&XI_XTEST));
    let surviving_grab = &f.state.xi2_pointer_grabs[&unrelated_device];
    assert_eq!(surviving_grab.owner, ClientId(OTHER));
    assert!(!surviving_grab.implicit);
    assert!(surviving_grab.passive);
    assert!(f.state.active_pointer_grab.is_none());
    assert_eq!(
        f.state.xi_devices.device(XI_XTEST).unwrap().attached_master,
        Some(XI_MASTER),
        "the unmap restores the released device's live attachment"
    );
    assert!(!f.state.xi2_detached_masters.contains_key(&XI_XTEST));
    assert!(f.state.xi2_detached_masters.contains_key(&unrelated_device));
    let released_freeze = &f.state.xi1_frozen[&XI_XTEST];
    assert_eq!(released_freeze.state, crate::server::Xi1SyncState::Thawed);
    assert_eq!(released_freeze.other, None);
    assert!(released_freeze.stored.is_none());
    let surviving_freeze = &f.state.xi1_frozen[&unrelated_device];
    assert!(
        surviving_freeze.frozen(),
        "the unrelated hold survives unmap"
    );
    assert!(surviving_freeze.stored.is_some());
}

#[test]
fn implicit_grabs_passive_sync_unmap_window_replays_queued_input() {
    assert_passive_sync_unmap_replays_only_released_device(&mut fixture(), false);
}

#[test]
fn implicit_grabs_passive_sync_unmap_subwindows_replays_queued_input() {
    assert_passive_sync_unmap_replays_only_released_device(&mut fixture_with_parent(), true);
}

#[test]
fn sibling_slave_press_and_master_fallback_keep_separate_forms() {
    // C20 measured on Xvfb 21.1.24: A gets the master fallback on W while
    // B independently gets the slave form on sibling OUT, for both edges.
    let mut f = fixture_with_window_owners(GRABBER, OTHER);
    select_xi2(&mut f, OTHER, OUTSIDE, XI_XTEST, XI_BUTTONS);
    grab_xi2_with_options(&mut f, WINDOW, true, XI_BUTTONS);
    assert_eq!(
        f.state.root_pointer_target_at(650, 150).map(|hit| hit.0),
        Some(ResourceId(OUTSIDE))
    );

    inject_from_origin(
        &mut f,
        crate::core_loop::InputOrigin::XTest(XI_XTEST),
        PointerEventKind::ButtonPress,
        650,
        150,
        HOST_OUTSIDE,
    );
    inject_from_origin(
        &mut f,
        crate::core_loop::InputOrigin::XTest(XI_XTEST),
        PointerEventKind::ButtonRelease,
        650,
        150,
        HOST_OUTSIDE,
    );

    let owner = read_all_capture_available(&mut f.grabber);
    let natural = read_all_capture_available(&mut f.other);
    assert_eq!(
        decode_deliveries(&owner),
        vec![
            Delivery {
                xi2: true,
                kind: 4,
                window: WINDOW,
                child: 0,
            },
            Delivery {
                xi2: true,
                kind: 5,
                window: WINDOW,
                child: 0,
            },
        ]
    );
    assert_eq!(
        xi2_event_ids(&owner),
        vec![(4, XI_MASTER, XI_XTEST), (5, XI_MASTER, XI_XTEST)]
    );
    assert_eq!(
        decode_deliveries(&natural),
        vec![
            Delivery {
                xi2: true,
                kind: 4,
                window: OUTSIDE,
                child: 0,
            },
            Delivery {
                xi2: true,
                kind: 5,
                window: OUTSIDE,
                child: 0,
            },
        ]
    );
    assert_eq!(
        xi2_event_ids(&natural),
        vec![(4, XI_XTEST, XI_XTEST), (5, XI_XTEST, XI_XTEST)]
    );
    assert_eq!(f.state.buttons_down, 0);
}

#[test]
fn core_owner_events_grab_rejects_foreign_master_scroll_motion() {
    // Source-backed scroll contract: ../xserver/dix/events.c:2071-2075,
    // 4431-4462 routes XI_Motion through the master grab's owner-events
    // attempt; eventconvert.c:113-122 keeps wheel-only Motion core_mask=0.
    const SOURCE: u64 = 0xA75C1;
    let mut f = fixture();
    let pointer = pointer_source(&mut f.state, SOURCE, true);
    select_xi2(&mut f, OTHER, CHILD, XI_MASTER, XI_MOTION);
    grab_core_with_options(&mut f, WINDOW, true, CORE_BUTTONS);

    inject_physical_wheel(&mut f, SOURCE, 101);
    let owner = read_all_capture_available(&mut f.grabber);
    let foreign = read_all_capture_available(&mut f.other);
    assert!(xi2_motion_forms(&owner).is_empty());
    assert!(
        foreign.is_empty(),
        "the core owner-events grab blocks B's foreign master Motion without inventing a core Motion"
    );
    assert_ne!(pointer, 0);
}

#[test]
fn slave_owner_events_scroll_grab_uses_its_mask_and_blocks_foreign_motion() {
    // Source-backed per-form scroll delivery: ../xserver/mi/mieq.c:484-508,
    // ../xserver/dix/events.c:2071-2075,4431-4462. With Motion in the grab
    // mask A gets W fallback; without it neither A nor B gets this form.
    const SOURCE: u64 = 0xA75C2;
    for (mask, expected_owner_motion) in [(XI_MOTION | XI_BUTTONS, true), (XI_BUTTONS, false)] {
        let mut f = fixture();
        let pointer = pointer_source(
            &mut f.state,
            SOURCE + u64::from(expected_owner_motion),
            true,
        );
        select_xi2(&mut f, OTHER, CHILD, pointer, XI_MOTION);
        grab_xi2_device_with_options(&mut f, WINDOW, pointer, true, mask);

        inject_physical_wheel(&mut f, SOURCE + u64::from(expected_owner_motion), 102);
        let owner = read_all_capture_available(&mut f.grabber);
        let foreign = read_all_capture_available(&mut f.other);
        assert_eq!(
            xi2_motion_forms(&owner),
            if expected_owner_motion {
                vec![(pointer, pointer)]
            } else {
                Vec::new()
            }
        );
        assert_eq!(
            xi2_motion_windows(&owner),
            if expected_owner_motion {
                vec![(WINDOW, CHILD)]
            } else {
                Vec::new()
            }
        );
        assert!(
            foreign.is_empty(),
            "the foreign slave selector is rejected for both grab-mask variants"
        );
    }
}

#[test]
fn exclusive_master_scroll_grab_preserves_ungrabbed_slave_motion() {
    // C17 plus FixUpEventFromWindow at ../xserver/dix/events.c:4389 establish
    // child D on exclusive grab fallback. Form independence comes from
    // ../xserver/mi/mieq.c:484-508 and
    // ../xserver/dix/events.c:4431-4462. The exclusive master grab owns its
    // master Motion while the ungrabbed slave form keeps B's natural target.
    const SOURCE: u64 = 0xA75C4;
    let mut f = fixture();
    let pointer = pointer_source(&mut f.state, SOURCE, true);
    select_xi2(&mut f, OTHER, CHILD, pointer, XI_MOTION);
    grab_xi2_with_options(&mut f, WINDOW, false, XI_MOTION | XI_BUTTONS);

    inject_physical_wheel(&mut f, SOURCE, 103);
    let owner = read_all_capture_available(&mut f.grabber);
    let natural = read_all_capture_available(&mut f.other);
    assert_eq!(xi2_motion_forms(&owner), vec![(XI_MASTER, pointer)]);
    assert_eq!(xi2_motion_windows(&owner), vec![(WINDOW, CHILD)]);
    assert_eq!(xi2_motion_forms(&natural), vec![(pointer, pointer)]);
    assert_eq!(xi2_motion_windows(&natural), vec![(CHILD, 0)]);
}

#[test]
fn ungrabbed_parent_scroll_update_and_stop_share_child_geometry() {
    // Xorg walks per-window targets for ordinary Motion, computes child D,
    // and fixes event coordinates relative to W via FixUpEventFromWindow
    // (../xserver/dix/events.c:2623-2637, 2897-2931).
    const SOURCE: u64 = 0xA75C6;
    let mut f = fixture();
    let pointer = pointer_source(&mut f.state, SOURCE, true);
    select_xi2(&mut f, GRABBER, WINDOW, XI_MASTER, XI_MOTION);

    inject_physical_wheel(&mut f, SOURCE, 106);
    let update = read_all_capture_available(&mut f.grabber);
    assert_eq!(xi2_motion_forms(&update), vec![(XI_MASTER, pointer)]);
    let update_geometry = xi2_geometry(&update);
    assert_eq!(update_geometry, vec![(WINDOW, CHILD, 150, 150, 50, 50)]);

    super::super::emit_scroll_stop_to_state(
        &mut f.state,
        &f.xid_map,
        crate::core_loop::InputOrigin::Physical(crate::xinput::InputSourceId(SOURCE)),
        HOST_WINDOW,
        150,
        150,
        0,
        107,
    );

    let stop = read_all_capture_available(&mut f.grabber);
    assert_eq!(
        xi2_geometry(&stop),
        vec![update_geometry[0]; 2],
        "both stop axes preserve the update's window, child, event and root coordinates"
    );
    assert!(read_all_capture_available(&mut f.other).is_empty());
}

#[test]
fn owner_events_scroll_update_and_stop_share_per_form_routing() {
    // Source-backed stop behavior: ../xserver/Xi/exevents.c:1942-1951 and
    // ../xserver/dix/events.c:4431-4462 apply grab routing to Motion even
    // when the stop carries unchanged zero-delta scroll valuators.
    const SOURCE: u64 = 0xA75C5;
    let mut f = fixture();
    let pointer = pointer_source(&mut f.state, SOURCE, true);
    select_xi2(&mut f, GRABBER, CHILD, pointer, XI_MOTION);
    select_xi2(&mut f, OTHER, CHILD, XI_MASTER, XI_MOTION);
    grab_xi2_with_options(&mut f, WINDOW, true, XI_MOTION | XI_BUTTONS);

    inject_physical_wheel(&mut f, SOURCE, 104);
    inject_physical_button(&mut f, SOURCE, PointerEventKind::ButtonRelease, 5, 105);
    let update = read_all_capture_available(&mut f.grabber);
    let rejected_update = read_all_capture_available(&mut f.other);
    assert_eq!(
        xi2_motion_forms(&update),
        vec![(pointer, pointer), (XI_MASTER, pointer)]
    );
    assert_eq!(
        xi2_motion_windows(&update),
        vec![(CHILD, 0), (WINDOW, CHILD)]
    );
    assert!(rejected_update.is_empty());

    let pointer_before = f.state.pointer_root;
    let buttons_before = (
        f.state.buttons_down,
        f.state.xi_devices.device(pointer).unwrap().buttons_down,
    );
    let values_before = (
        f.state.scroll_axis_value,
        f.state
            .xi_devices
            .device(pointer)
            .unwrap()
            .scroll_axis_values,
    );
    super::super::emit_scroll_stop_to_state(
        &mut f.state,
        &f.xid_map,
        crate::core_loop::InputOrigin::Physical(crate::xinput::InputSourceId(SOURCE)),
        HOST_WINDOW,
        150,
        150,
        0,
        105,
    );

    let stop = read_all_capture_available(&mut f.grabber);
    let rejected_stop = read_all_capture_available(&mut f.other);
    assert_eq!(
        xi2_motion_forms(&stop),
        vec![
            (pointer, pointer),
            (pointer, pointer),
            (XI_MASTER, pointer),
            (XI_MASTER, pointer),
        ],
        "the stop reaches both A-owned forms in slave-before-master order"
    );
    assert_eq!(
        xi2_motion_windows(&stop),
        vec![(CHILD, 0), (CHILD, 0), (WINDOW, CHILD), (WINDOW, CHILD)]
    );
    assert_eq!(
        xi2_scroll_values(&stop),
        vec![
            (pointer, pointer, 1),
            (pointer, pointer, 0),
            (XI_MASTER, pointer, 1),
            (XI_MASTER, pointer, 0),
        ],
        "the stop preserves the current vertical and horizontal valuators"
    );
    assert!(rejected_stop.is_empty());
    assert_eq!(
        f.state.pointer_root, pointer_before,
        "stop has no position change"
    );
    assert_eq!(
        (
            f.state.buttons_down,
            f.state.xi_devices.device(pointer).unwrap().buttons_down
        ),
        buttons_before,
        "stop performs no button transition"
    );
    assert_eq!(
        (
            f.state.scroll_axis_value,
            f.state
                .xi_devices
                .device(pointer)
                .unwrap()
                .scroll_axis_values
        ),
        values_before,
        "stop leaves the accumulated scroll valuators unchanged"
    );
}

#[test]
fn xi_owner_events_fallback_preserves_other_recipients_root_geometry() {
    use crate::{core_loop::InputOrigin, resources::ROOT_VISUAL};
    use yserver_protocol::x11::{RequestHeader, SequenceNumber};

    const GRABBER: u32 = 0xA745;
    const SELECTOR: u32 = 0xA746;
    const SOURCE: u64 = 0xA7451;
    const HOST_XID: u32 = 0xCAFE_A745;
    const CHILD: u32 = 0x0010_A745;
    const XI_MOTION: u32 = 1 << 6;

    fn request(
        state: &mut ServerState,
        backend: &mut RecordingBackend,
        client: u32,
        sequence: u16,
        minor: u8,
        body: &[u8],
    ) {
        crate::core_loop::process_request::process_request(
            state,
            backend,
            ClientId(client),
            SequenceNumber(sequence),
            RequestHeader {
                opcode: 137,
                data: minor,
                length_units: u32::try_from((body.len() + 4) / 4).unwrap(),
            },
            body,
            None,
        )
        .expect("production XI2 request dispatch");
    }

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    let mut grabber = install_capture_client(&mut state, GRABBER);
    let mut selector = install_capture_client(&mut state, SELECTOR);
    let pointer = pointer_source(&mut state, SOURCE, true);

    state.resources.create_window(
        ClientId(SELECTOR),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(CHILD),
            parent: ROOT_WINDOW,
            x: 20,
            y: 30,
            width: 100,
            height: 100,
            border_width: 0,
            class: 1,
            visual: ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(ResourceId(CHILD));
    let mut xid_map = HostXidMap::new();
    xid_map.insert(HOST_XID, ResourceId(CHILD));

    for client in [GRABBER, SELECTOR] {
        request(&mut state, &mut backend, client, 1, 47, &[2, 0, 4, 0]);
    }
    let _ = read_all_capture_available(&mut grabber);
    let _ = read_all_capture_available(&mut selector);

    // C1 measures that a foreign master selector is suppressed during the
    // owner's XIGrabDevice routing. C5 measures that an attached-slave
    // selector still receives its independent slave form. Keep both forms
    // selected here so this regression continues to cover root geometry on
    // the surviving slave event.
    let mut select = Vec::new();
    select.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    select.extend_from_slice(&2u16.to_le_bytes());
    select.extend_from_slice(&[0; 2]);
    select.extend_from_slice(&2u16.to_le_bytes());
    select.extend_from_slice(&1u16.to_le_bytes());
    select.extend_from_slice(&XI_MOTION.to_le_bytes());
    select.extend_from_slice(&pointer.to_le_bytes());
    select.extend_from_slice(&1u16.to_le_bytes());
    select.extend_from_slice(&XI_MOTION.to_le_bytes());
    request(&mut state, &mut backend, SELECTOR, 2, 46, &select);
    let _ = read_all_capture_available(&mut selector);

    let mut grab = Vec::new();
    grab.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    grab.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    grab.extend_from_slice(&0u32.to_le_bytes()); // no cursor
    grab.extend_from_slice(&2u16.to_le_bytes()); // master pointer
    grab.extend_from_slice(&[1, 1, 1, 0]); // async, async paired, owner_events=true
    grab.extend_from_slice(&1u16.to_le_bytes());
    grab.extend_from_slice(&XI_MOTION.to_le_bytes());
    request(&mut state, &mut backend, GRABBER, 2, 51, &grab);
    let _ = read_all_capture_available(&mut grabber);
    assert!(
        state
            .active_pointer_grab
            .is_some_and(|active| { active.owner == ClientId(GRABBER) && active.owner_events })
    );

    let dropped = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &xid_map,
        HostPointerEvent {
            origin: InputOrigin::Physical(crate::xinput::InputSourceId(SOURCE)),
            kind: PointerEventKind::MotionNotify,
            host_xid: HOST_XID,
            root_x: 45,
            root_y: 65,
            event_x: 25,
            event_y: 35,
            time: 50,
            ..motion_event()
        },
        true,
        false,
    );
    assert!(dropped.is_empty());

    let selector_bytes = read_all_capture_available(&mut selector);
    let grabber_bytes = read_all_capture_available(&mut grabber);
    assert_eq!(selector_bytes[0], 35, "selector receives XI_Motion");
    assert_eq!(
        xi2_event_ids(&selector_bytes),
        vec![(6, pointer, pointer)],
        "only the selected slave form is delivered"
    );
    assert_eq!(
        u16::from_le_bytes([selector_bytes[8], selector_bytes[9]]),
        6
    );
    assert_eq!(
        u16::from_le_bytes([selector_bytes[10], selector_bytes[11]]),
        pointer,
        "the surviving event is the selected slave form"
    );
    assert_eq!(
        u32::from_le_bytes(selector_bytes[24..28].try_into().unwrap()),
        ROOT_WINDOW.0,
        "other recipients keep the root event window under owner-events fallback"
    );
    assert_eq!(
        u32::from_le_bytes(selector_bytes[28..32].try_into().unwrap()),
        CHILD,
        "the selected root event retains its child path"
    );
    assert_eq!(
        i16::from_le_bytes(selector_bytes[42..44].try_into().unwrap()),
        45,
        "root selection receives root-local event_x"
    );
    assert_eq!(
        i16::from_le_bytes(selector_bytes[46..48].try_into().unwrap()),
        65,
        "root selection receives root-local event_y"
    );
    assert_eq!(
        grabber_bytes[0], 35,
        "fallback grab owner receives XI_Motion"
    );
    assert_eq!(
        u32::from_le_bytes(grabber_bytes[24..28].try_into().unwrap()),
        ROOT_WINDOW.0,
        "grab owner gets its grab window"
    );
    assert_eq!(
        u32::from_le_bytes(grabber_bytes[28..32].try_into().unwrap()),
        CHILD,
        "C2: Xorg fallback reports the child toward the pointer"
    );
    assert_eq!(
        i16::from_le_bytes(grabber_bytes[42..44].try_into().unwrap()),
        45
    );
    assert_eq!(
        i16::from_le_bytes(grabber_bytes[46..48].try_into().unwrap()),
        65
    );
    assert_eq!(state.pointer_root, (45, 65));
    assert_eq!(
        state.xi_devices.device(pointer).unwrap().attached_master,
        Some(2)
    );
    assert!(state.sync_pending.is_empty());
    assert_eq!(state.buttons_down, 0);
    assert_eq!(state.xi_devices.device(pointer).unwrap().buttons_down, 0);
    assert_eq!(state.active_pointer_grab.unwrap().owner, ClientId(GRABBER));
}

/// Regression (HW-confirmed 2026-07-10, Telegram/Qt on silence):
/// a client selecting `XIAllDevices(0)` receives every pointer event
/// in BOTH slave- and master-stamped forms, and the SLAVE form MUST be
/// emitted first — matching Xorg's `mieqProcessDeviceEvent`
/// ("process slave first, then master", mi/mieq.c). Qt compresses
/// consecutive XI_Motion keeping only the last (qxcbconnection.cpp) and
/// drops the slave-deviceid copy (qxcbconnection_xi2.cpp:689). With the
/// old master-first order every master copy was trailed by its slave
/// copy → compressed away → and the surviving slave copy dropped, so
/// ZERO motion (and thus zero smooth-scroll, which rides XI_Motion)
/// reached the widget: hover-scrollbars + wheel dead while clicks
/// (not motion-compressed) worked. Slave-first leaves the master copy
/// — the one Qt keeps — trailing, so it survives compression.
/// Issue #94 follow-up: an XI2 client holding an active `XIGrabDevice`
/// (`owner_events=true`) must receive the grabbed `ButtonRelease` even
/// when the pointer is over a window it never `XISelectEvents`'d on —
/// Xorg `DeliverGrabbedEvent` falls back to grab-window delivery using
/// the grab mask. CEF grabs the pointer on mousedown and holds it to
/// mouseup; the release fires over a window it holds only via the grab,
/// so without the fallback the mouse-up reaches nobody and clicks never
/// complete (Steam nav/menus/close all dead post-crash-fix).
#[test]
fn xi2_grabbed_button_release_reaches_grab_owner_via_grab_window() {
    use crate::server::ActivePointerGrab;
    use yserver_protocol::x11::{CreateWindowRequest, ResourceId};
    const GC: u32 = 2; // grab owner (CEF-like)
    let grab_win = ResourceId(0x0020_0001); // GC's window == grab window
    let hit_win = ResourceId(0x0020_0002); // child GC owns but did NOT select on

    let mut state = ServerState::new();
    let mut backend = crate::backend::recording::RecordingBackend::default();
    let mut gc_peer = install_client(&mut state, GC);

    state.resources.create_window(
        ClientId(GC),
        CreateWindowRequest {
            depth: 24,
            window: grab_win,
            parent: crate::resources::ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 100,
            height: 100,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    state.resources.create_window(
        ClientId(GC),
        CreateWindowRequest {
            depth: 24,
            window: hit_win,
            parent: grab_win,
            x: 10,
            y: 10,
            width: 40,
            height: 40,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(grab_win);
    let _ = state.resources.map_window(hit_win);

    // Active XI2 grab held by GC (matching CEF's XIGrabDevice device=2,
    // async, owner_events=true). GC has NO XISelectEvents mask anywhere
    // — it holds the pointer purely via the grab.
    state.active_pointer_grab = Some(ActivePointerGrab {
        owner: ClientId(GC),
        grab_window: grab_win,
        event_mask: 0xFFFF,
        cursor: ResourceId(0),
        time: 0,
        owner_events: true,
        via_xi2: true,
        implicit: false,
        passive: false,
        xi2_mask: u64::MAX,
    });

    let mut xid_map = HostXidMap::new();
    xid_map.insert(0xCAFE_u32, grab_win);

    let mut rel = motion_event();
    rel.kind = PointerEventKind::ButtonRelease;
    rel.host_xid = 0xCAFE;
    rel.detail = 1;
    rel.root_x = 20;
    rel.root_y = 20;
    rel.event_x = 20;
    rel.event_y = 20;

    let mut press = rel;
    press.kind = PointerEventKind::ButtonPress;
    press.state = 0;
    let _ = pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, press, true, false);
    let _ = read_all_available(&mut gc_peer);

    let _ = pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, rel, true, false);

    let bytes = read_all_available(&mut gc_peer);
    let mut found = None;
    let mut off = 0usize;
    while off + 32 <= bytes.len() {
        if bytes[off] == 35 && u16::from_le_bytes([bytes[off + 8], bytes[off + 9]]) == 5 {
            found = Some(u32::from_le_bytes(
                bytes[off + 24..off + 28].try_into().unwrap(),
            ));
            break;
        }
        let length = u32::from_le_bytes(bytes[off + 4..off + 8].try_into().unwrap()) as usize;
        off += 32 + length * 4;
    }
    assert!(
        found.is_some(),
        "grab owner must receive the grabbed XI2 ButtonRelease (Xorg grab-window fallback); got none"
    );
    assert_eq!(
        found.unwrap(),
        grab_win.0,
        "grabbed release must be reported on the grab window"
    );
}

/// Core grabs use the same grab-window fallback as XI2 grabs: when
/// owner-events delivery finds no selected natural target, the grab
/// owner still receives the event on the grab window.
#[test]
fn core_grabbed_button_release_reaches_grab_owner_via_grab_window() {
    use crate::server::ActivePointerGrab;
    use yserver_protocol::x11::{CreateWindowRequest, ResourceId};

    const OWNER: u32 = 2;
    let grab_win = ResourceId(0x0020_0001);
    let hit_win = ResourceId(0x0020_0002);

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::default();
    let mut owner_peer = install_client(&mut state, OWNER);

    for (window, parent, x, y, width, height) in [
        (grab_win, ROOT_WINDOW, 0, 0, 100, 100),
        (hit_win, grab_win, 10, 10, 40, 40),
    ] {
        state.resources.create_window(
            ClientId(OWNER),
            CreateWindowRequest {
                depth: 24,
                window,
                parent,
                x,
                y,
                width,
                height,
                border_width: 0,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
    }
    let _ = state.resources.map_window(grab_win);
    let _ = state.resources.map_window(hit_win);

    // No core event mask is selected on either window. Delivery must
    // therefore fall back to the grab's event mask and grab window.
    state.active_pointer_grab = Some(ActivePointerGrab {
        owner: ClientId(OWNER),
        grab_window: grab_win,
        event_mask: 0xFFFF,
        cursor: ResourceId(0),
        time: 0,
        owner_events: true,
        via_xi2: false,
        implicit: false,
        passive: false,
        xi2_mask: 0,
    });

    let mut xid_map = HostXidMap::new();
    xid_map.insert(0xCAFE, hit_win);

    let mut release = motion_event();
    release.kind = PointerEventKind::ButtonRelease;
    release.host_xid = 0xCAFE;
    release.detail = 1;
    release.root_x = 20;
    release.root_y = 20;
    release.event_x = 10;
    release.event_y = 10;

    let mut press = release;
    press.kind = PointerEventKind::ButtonPress;
    press.state = 0;
    let _ = pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, press, true, false);
    let _ = read_all_available(&mut owner_peer);

    let _ = pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, release, true, false);

    // Core ButtonRelease is event code 5 and its event window occupies
    // bytes 12..16 of the fixed 32-byte event.
    let bytes = read_all_available(&mut owner_peer);
    let event_window = bytes.chunks_exact(32).find_map(|event| {
        (event[0] & 0x7F == 5)
            .then(|| u32::from_le_bytes(event[12..16].try_into().expect("event window bytes")))
    });
    assert_eq!(
        event_window,
        Some(grab_win.0),
        "grabbed core release must be reported on the grab window",
    );
}
