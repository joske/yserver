use super::*;

use std::os::unix::net::UnixStream;

use yserver_core::{backend::Backend, core_loop::process_request, server::ServerState};
use yserver_protocol::x11::ResourceId;

const CLIENT_A: u32 = 0xA753;
const CLIENT_B: u32 = 0xA754;
const WINDOW: u32 = 0x0010_A753;
const CHILD: u32 = 0x0020_A753;
const OUTSIDE: u32 = 0x0010_A754;
const INITIAL_WINDOW: u32 = 0x0010_A755;
const XTEST_OPCODE: u8 = 146;
const XTEST_FAKE_INPUT: u8 = 2;
const XI2_OPCODE: u8 = 137;
const XI_SELECT_EVENTS: u8 = 46;
const XI_SELECT_EXTENSION_EVENT: u8 = 6;
const XI_GRAB_DEVICE: u8 = 51;
const XI_MOTION: u32 = 1 << 6;
const XI_MASTER_POINTER: u16 = 2;
const XI_XTEST_POINTER: u16 = 4;
const CORE_POINTER_MOTION: u32 = 1 << 6;
const CORE_EVENT_MASK: u32 = 1 << 11;
const XI_FIRST_EVENT: u8 = 66;

#[derive(Debug, PartialEq, Eq)]
struct Delivery {
    xi2: bool,
    kind: u16,
    window: u32,
    child: u32,
}

struct Fixture {
    backend: KmsBackend,
    state: ServerState,
    client_a: UnixStream,
    client_b: UnixStream,
}

fn dispatch(
    state: &mut ServerState,
    backend: &mut KmsBackend,
    client: u32,
    sequence: u16,
    opcode: u8,
    data: u8,
    body: &[u8],
) {
    process_request::process_request(
        state,
        backend as &mut dyn Backend,
        yserver_protocol::x11::ClientId(client),
        yserver_protocol::x11::SequenceNumber(sequence),
        yserver_protocol::x11::RequestHeader {
            opcode,
            data,
            length_units: u32::try_from(1 + body.len().div_ceil(4)).unwrap(),
        },
        body,
        None,
    )
    .expect("production request dispatch");
}

fn create_window(
    f: &mut Fixture,
    client: u32,
    window: u32,
    parent: u32,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
) {
    let mut body = Vec::with_capacity(28);
    body.extend_from_slice(&window.to_le_bytes());
    body.extend_from_slice(&parent.to_le_bytes());
    body.extend_from_slice(&x.to_le_bytes());
    body.extend_from_slice(&y.to_le_bytes());
    body.extend_from_slice(&width.to_le_bytes());
    body.extend_from_slice(&height.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes()); // border width
    body.extend_from_slice(&1u16.to_le_bytes()); // InputOutput
    body.extend_from_slice(&yserver_core::resources::ROOT_VISUAL.0.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // value mask
    dispatch(&mut f.state, &mut f.backend, client, 1, 1, 24, &body);
}

fn map_window(f: &mut Fixture, client: u32, window: u32) {
    dispatch(
        &mut f.state,
        &mut f.backend,
        client,
        2,
        8,
        0,
        &window.to_le_bytes(),
    );
}

fn fixture() -> Fixture {
    let mut state = ServerState::new();
    let client_a = kbd_map_client_id(&mut state, CLIENT_A);
    let client_b = kbd_map_client_id(&mut state, CLIENT_B);
    let mut f = Fixture {
        backend: KmsBackend::for_tests(),
        state,
        client_a,
        client_b,
    };

    create_window(
        &mut f,
        CLIENT_A,
        WINDOW,
        yserver_core::resources::ROOT_WINDOW.0,
        100,
        100,
        200,
        200,
    );
    map_window(&mut f, CLIENT_A, WINDOW);
    create_window(&mut f, CLIENT_B, CHILD, WINDOW, 20, 20, 100, 100);
    map_window(&mut f, CLIENT_B, CHILD);
    create_window(
        &mut f,
        CLIENT_B,
        OUTSIDE,
        yserver_core::resources::ROOT_WINDOW.0,
        550,
        100,
        300,
        300,
    );
    map_window(&mut f, CLIENT_B, OUTSIDE);
    f
}

fn select_xi2(f: &mut Fixture, client: u32, window: u32, device: u16, mask: u32) {
    let mut body = Vec::with_capacity(20);
    body.extend_from_slice(&window.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes()); // one event mask
    body.extend_from_slice(&[0; 2]);
    body.extend_from_slice(&device.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes()); // one mask unit
    body.extend_from_slice(&mask.to_le_bytes());
    dispatch(
        &mut f.state,
        &mut f.backend,
        client,
        3,
        XI2_OPCODE,
        XI_SELECT_EVENTS,
        &body,
    );
    assert_eq!(
        f.state.clients[&client]
            .xi2_masks
            .get(&(ResourceId(window), device)),
        Some(&u64::from(mask)),
        "XISelectEvents must register the requested selection"
    );
}

fn select_core_motion(f: &mut Fixture, client: u32, window: u32) {
    let mut body = Vec::with_capacity(12);
    body.extend_from_slice(&window.to_le_bytes());
    body.extend_from_slice(&CORE_EVENT_MASK.to_le_bytes());
    body.extend_from_slice(&CORE_POINTER_MOTION.to_le_bytes());
    dispatch(&mut f.state, &mut f.backend, client, 4, 2, 0, &body);
    assert_eq!(
        f.state.clients[&client]
            .event_masks
            .get(&ResourceId(window)),
        Some(&CORE_POINTER_MOTION),
        "ChangeWindowAttributes must register core PointerMotion"
    );
}

fn select_xi1_motion(f: &mut Fixture, client: u32, window: u32, device: u16) {
    let class = (u32::from(device) << 8)
        | u32::from(XI_FIRST_EVENT + yserver_core::xinput::XI_DEVICE_MOTION_NOTIFY_OFFSET);
    let mut body = Vec::with_capacity(12);
    body.extend_from_slice(&window.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes());
    body.extend_from_slice(&[0; 2]);
    body.extend_from_slice(&class.to_le_bytes());
    dispatch(
        &mut f.state,
        &mut f.backend,
        client,
        5,
        XI2_OPCODE,
        XI_SELECT_EXTENSION_EVENT,
        &body,
    );
    assert!(
        f.state.clients[&client]
            .xi1_window_event_classes
            .get(&ResourceId(window))
            .is_some_and(|classes| classes.contains(&class)),
        "XSelectExtensionEvent must register the XI1 Motion selection"
    );
}

fn fake_core_motion(f: &mut Fixture, detail: u8, x: i16, y: i16) {
    let mut body = vec![0u8; 32];
    body[0] = 6; // MotionNotify
    body[1] = detail;
    body[4..8].copy_from_slice(&50u32.to_le_bytes());
    body[20..22].copy_from_slice(&x.to_le_bytes());
    body[22..24].copy_from_slice(&y.to_le_bytes());
    dispatch(
        &mut f.state,
        &mut f.backend,
        CLIENT_A,
        6,
        XTEST_OPCODE,
        XTEST_FAKE_INPUT,
        &body,
    );
}

fn fake_device_motion(f: &mut Fixture, device: u16, detail: u8, first: u8, values: &[i32]) {
    assert!(!values.is_empty() && values.len() <= 6);
    let mut body = vec![0u8; 64];
    body[0] = XI_FIRST_EVENT + yserver_core::xinput::XI_DEVICE_MOTION_NOTIFY_OFFSET;
    body[1] = detail;
    body[4..8].copy_from_slice(&50u32.to_le_bytes());
    body[31] = u8::try_from(device).unwrap() | yserver_core::xinput::XI1_MORE_EVENTS;
    body[32] = XI_FIRST_EVENT + yserver_core::xinput::XI_DEVICE_VALUATOR_OFFSET;
    body[33] = u8::try_from(device).unwrap();
    body[38] = u8::try_from(values.len()).unwrap();
    body[39] = first;
    for (index, value) in values.iter().enumerate() {
        let offset = 40 + index * 4;
        body[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    dispatch(
        &mut f.state,
        &mut f.backend,
        CLIENT_A,
        7,
        XTEST_OPCODE,
        XTEST_FAKE_INPUT,
        &body,
    );
}

fn grab_xi2_device(f: &mut Fixture, owner_events: bool, mask: u32) {
    let mut body = Vec::with_capacity(24);
    body.extend_from_slice(&WINDOW.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    body.extend_from_slice(&0u32.to_le_bytes()); // no cursor
    body.extend_from_slice(&XI_XTEST_POINTER.to_le_bytes());
    body.extend_from_slice(&[1, 1, u8::from(owner_events), 0]); // async modes
    body.extend_from_slice(&1u16.to_le_bytes());
    body.extend_from_slice(&mask.to_le_bytes());
    dispatch(
        &mut f.state,
        &mut f.backend,
        CLIENT_A,
        8,
        XI2_OPCODE,
        XI_GRAB_DEVICE,
        &body,
    );
    let _ = kbd_map_drain(&mut f.client_a); // grab reply and crossings
    let _ = kbd_map_drain(&mut f.client_b);
    assert_eq!(
        f.state
            .xi2_pointer_grabs
            .get(&XI_XTEST_POINTER)
            .map(|grab| grab.owner),
        Some(yserver_protocol::x11::ClientId(CLIENT_A)),
        "XIGrabDevice must install the real slave grab"
    );
}

fn setup_x_case() -> Fixture {
    let mut f = fixture();
    select_xi2(&mut f, CLIENT_A, WINDOW, XI_MASTER_POINTER, XI_MOTION);
    select_core_motion(&mut f, CLIENT_A, WINDOW);
    select_xi2(&mut f, CLIENT_B, OUTSIDE, XI_MASTER_POINTER, XI_MOTION);
    select_core_motion(&mut f, CLIENT_B, OUTSIDE);
    fake_core_motion(&mut f, 0, 150, 150);
    let _ = kbd_map_drain(&mut f.client_a);
    let _ = kbd_map_drain(&mut f.client_b);
    f
}

fn event_packets(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut packets = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        assert!(
            offset + 32 <= bytes.len(),
            "complete X11 packet: {bytes:02x?}"
        );
        let length = if bytes[offset] == 35 {
            let units = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap());
            32 + usize::try_from(units).unwrap() * 4
        } else {
            32
        };
        assert!(
            offset + length <= bytes.len(),
            "complete X11 event: {bytes:02x?}"
        );
        packets.push(bytes[offset..offset + length].to_vec());
        offset += length;
    }
    packets
}

fn xi2_event_ids(bytes: &[u8]) -> Vec<(u16, u16, u16)> {
    event_packets(bytes)
        .into_iter()
        .filter(|packet| packet[0] == 35)
        .map(|packet| {
            (
                u16::from_le_bytes(packet[8..10].try_into().unwrap()),
                u16::from_le_bytes(packet[10..12].try_into().unwrap()),
                u16::from_le_bytes(packet[52..54].try_into().unwrap()),
            )
        })
        .collect()
}

fn xi2_deliveries(bytes: &[u8]) -> Vec<(u16, u32, u32)> {
    event_packets(bytes)
        .into_iter()
        .filter(|packet| packet[0] == 35)
        .map(|packet| {
            (
                u16::from_le_bytes(packet[8..10].try_into().unwrap()),
                u32::from_le_bytes(packet[24..28].try_into().unwrap()),
                u32::from_le_bytes(packet[28..32].try_into().unwrap()),
            )
        })
        .collect()
}

fn decode_deliveries(bytes: &[u8]) -> Vec<Delivery> {
    event_packets(bytes)
        .into_iter()
        .filter_map(|packet| {
            if packet[0] == 35 {
                let kind = u16::from_le_bytes(packet[8..10].try_into().unwrap());
                (4..=6).contains(&kind).then(|| Delivery {
                    xi2: true,
                    kind,
                    window: u32::from_le_bytes(packet[24..28].try_into().unwrap()),
                    child: u32::from_le_bytes(packet[28..32].try_into().unwrap()),
                })
            } else {
                let kind = packet[0] & 0x7f;
                (4..=6).contains(&kind).then(|| Delivery {
                    xi2: false,
                    kind: u16::from(kind),
                    window: u32::from_le_bytes(packet[12..16].try_into().unwrap()),
                    child: u32::from_le_bytes(packet[16..20].try_into().unwrap()),
                })
            }
        })
        .collect()
}

fn xi2_valuator_values(bytes: &[u8]) -> Vec<(u8, i32)> {
    event_packets(bytes)
        .into_iter()
        .filter(|packet| {
            packet[0] == 35 && u16::from_le_bytes(packet[8..10].try_into().unwrap()) == 6
        })
        .flat_map(|packet| {
            let mask = [
                u32::from_le_bytes(packet[112..116].try_into().unwrap()),
                u32::from_le_bytes(packet[116..120].try_into().unwrap()),
            ];
            let mut value_offset = 120;
            (0..64).filter_map(move |axis| {
                if mask[axis / 32] & (1 << (axis % 32)) == 0 {
                    return None;
                }
                let value =
                    i32::from_le_bytes(packet[value_offset..value_offset + 4].try_into().unwrap());
                value_offset += 8; // FP32.32 value; the integer is first.
                Some((u8::try_from(axis).unwrap(), value))
            })
        })
        .collect()
}

fn xi2_geometry(bytes: &[u8]) -> Vec<(u32, u32, i16, i16, i16, i16)> {
    event_packets(bytes)
        .into_iter()
        .filter(|packet| packet[0] == 35)
        .map(|packet| {
            (
                u32::from_le_bytes(packet[24..28].try_into().unwrap()),
                u32::from_le_bytes(packet[28..32].try_into().unwrap()),
                i16::from_le_bytes(packet[34..36].try_into().unwrap()),
                i16::from_le_bytes(packet[38..40].try_into().unwrap()),
                i16::from_le_bytes(packet[42..44].try_into().unwrap()),
                i16::from_le_bytes(packet[46..48].try_into().unwrap()),
            )
        })
        .collect()
}

fn xi1_device_event_ids(bytes: &[u8]) -> Vec<(u8, u8)> {
    event_packets(bytes)
        .into_iter()
        .filter(|packet| {
            (XI_FIRST_EVENT + yserver_core::xinput::XI_DEVICE_BUTTON_PRESS_OFFSET
                ..=XI_FIRST_EVENT + yserver_core::xinput::XI_DEVICE_MOTION_NOTIFY_OFFSET)
                .contains(&(packet[0] & 0x7f))
        })
        .map(|packet| (packet[0] & 0x7f, packet[31] & 0x7f))
        .collect()
}

fn xi1_valuator_values(bytes: &[u8]) -> Vec<(u8, i32)> {
    event_packets(bytes)
        .into_iter()
        .filter(|packet| {
            packet[0] == XI_FIRST_EVENT + yserver_core::xinput::XI_DEVICE_VALUATOR_OFFSET
        })
        .flat_map(|packet| {
            let count = packet[6].min(6);
            let first = packet[7];
            (0..count).map(move |index| {
                let offset = 8 + usize::from(index) * 4;
                (
                    first + index,
                    i32::from_le_bytes(packet[offset..offset + 4].try_into().unwrap()),
                )
            })
        })
        .collect()
}

fn query_root_child(f: &mut Fixture) -> u32 {
    let root = yserver_core::resources::ROOT_WINDOW.0;
    dispatch(
        &mut f.state,
        &mut f.backend,
        CLIENT_A,
        9,
        38,
        0,
        &root.to_le_bytes(),
    );
    let reply = kbd_map_drain(&mut f.client_a);
    assert_eq!(reply.len(), 32, "QueryPointer reply: {reply:02x?}");
    assert_eq!(reply[0], 1, "QueryPointer returned a reply");
    u32::from_le_bytes(reply[12..16].try_into().unwrap())
}

fn assert_button_state_empty(f: &Fixture) {
    assert_eq!(f.state.buttons_down, 0, "all master buttons are released");
    assert!(
        f.state
            .xi_devices
            .devices()
            .iter()
            .all(|device| device.buttons_down == 0),
        "every source's held-button set is empty"
    );
    assert!(
        f.state
            .xi1_device_input_state
            .values()
            .all(|device| device.buttons_down.iter().all(|byte| *byte == 0)),
        "every XI1 device button set is empty"
    );
    assert!(
        f.state
            .unpublished_pointer_buttons_down
            .values()
            .all(|buttons| *buttons == 0),
        "unpublished source held-button sets are empty"
    );
}

fn assert_end_state(f: &Fixture) {
    assert!(f.state.active_pointer_grab.is_none(), "master grab ended");
    assert!(f.state.xi2_pointer_grabs.is_empty(), "slave grabs ended");
    assert!(f.state.xi1_active_grabs.is_empty(), "XI1 grabs ended");
    assert_xi1_end_state(f, &[]);
    assert!(
        f.state.xi2_detached_masters.is_empty(),
        "no slave is detached"
    );
    assert_button_state_empty(f);
}

fn assert_explicit_slave_grab_end_state(f: &Fixture, xi1_entries: &[u16]) {
    assert!(f.state.active_pointer_grab.is_none(), "master grab ended");
    assert_eq!(f.state.xi2_pointer_grabs.len(), 1);
    assert_eq!(
        f.state
            .xi2_pointer_grabs
            .get(&XI_XTEST_POINTER)
            .map(|grab| grab.owner),
        Some(yserver_protocol::x11::ClientId(CLIENT_A)),
        "the explicit XI2 test grab remains active"
    );
    assert!(f.state.xi1_active_grabs.is_empty(), "XI1 grabs ended");
    assert_xi1_end_state(f, xi1_entries);
    assert_eq!(
        f.state.xi2_detached_masters.len(),
        1,
        "the explicit XI2 slave grab is the only detached master"
    );
    assert_eq!(
        f.state.xi2_detached_masters.get(&XI_XTEST_POINTER),
        Some(&XI_MASTER_POINTER),
        "the explicit XI2 test grab keeps its slave detached"
    );
    assert_button_state_empty(f);
}

fn assert_xi1_end_state(f: &Fixture, expected_devices: &[u16]) {
    let actual: std::collections::BTreeSet<_> = f.state.xi1_frozen.keys().copied().collect();
    let expected: std::collections::BTreeSet<_> = expected_devices.iter().copied().collect();
    assert_eq!(actual, expected, "XI1 sync entries match this scenario");
    assert!(f.state.xi1_frozen.values().all(|freeze| {
        freeze.state == yserver_core::server::Xi1SyncState::Thawed
            && freeze.other.is_none()
            && freeze.stored.is_none()
    }));
}

#[test]
fn x1_absolute_device_motion_moves_sprite() {
    let mut f = setup_x_case();
    fake_device_motion(&mut f, XI_XTEST_POINTER, 0, 0, &[650, 150]);

    assert_eq!(f.state.pointer_root, (650, 150));
    assert_eq!(query_root_child(&mut f), OUTSIDE);
    let a = kbd_map_drain(&mut f.client_a);
    let b = kbd_map_drain(&mut f.client_b);
    assert!(a.is_empty(), "A selected W, which the sprite has left");
    assert_eq!(
        xi2_event_ids(&b),
        vec![(6, XI_MASTER_POINTER, XI_XTEST_POINTER)]
    );
    assert_eq!(xi2_deliveries(&b), vec![(6, OUTSIDE, 0)]);
    assert_end_state(&f);
}

#[test]
fn x2_relative_device_motion_moves_sprite() {
    let mut f = setup_x_case();
    fake_device_motion(&mut f, XI_XTEST_POINTER, 1, 0, &[500, 0]);

    assert_eq!(f.state.pointer_root, (650, 150));
    assert_eq!(query_root_child(&mut f), OUTSIDE);
    let a = kbd_map_drain(&mut f.client_a);
    let b = kbd_map_drain(&mut f.client_b);
    assert!(a.is_empty(), "A selected W, which the sprite has left");
    assert_eq!(
        xi2_event_ids(&b),
        vec![(6, XI_MASTER_POINTER, XI_XTEST_POINTER)]
    );
    assert_eq!(xi2_deliveries(&b), vec![(6, OUTSIDE, 0)]);
    assert_eq!(
        xi2_valuator_values(&b),
        vec![(0, 650), (1, 150)],
        "relative XI2 valuators report the completed pointer position"
    );
    assert_end_state(&f);
}

#[test]
fn x3_one_axis_device_motion_follows_xorg_copy() {
    let mut f = setup_x_case();
    fake_device_motion(&mut f, XI_XTEST_POINTER, 0, 1, &[175]);

    assert_eq!(f.state.pointer_root, (150, 0));
    assert_eq!(
        query_root_child(&mut f),
        0,
        "the root has no child at (150,0)"
    );
    assert!(kbd_map_drain(&mut f.client_a).is_empty());
    assert!(kbd_map_drain(&mut f.client_b).is_empty());
    assert_end_state(&f);
}

#[test]
fn xtest_relative_device_motion_clamps_to_screen() {
    let mut f = setup_x_case();
    fake_device_motion(&mut f, XI_XTEST_POINTER, 1, 0, &[5000, 0]);

    let (screen_width, _) = Backend::fb_dimensions(&f.backend);
    assert_eq!(
        f.state.pointer_root,
        (i16::try_from(screen_width - 1).unwrap(), 150)
    );
    let b = kbd_map_drain(&mut f.client_b);
    assert_eq!(
        xi2_valuator_values(&b),
        vec![
            (0, i32::from(i16::try_from(screen_width - 1).unwrap())),
            (1, 150),
        ],
        "clamped XI2 valuators report the completed pointer position"
    );
    assert_end_state(&f);
}

#[test]
fn xtest_device_motion_owner_events_and_exclusive_grabs_route_xi2() {
    let mut owner_events = fixture();
    fake_core_motion(&mut owner_events, 0, 150, 150);
    let _ = kbd_map_drain(&mut owner_events.client_a);
    let _ = kbd_map_drain(&mut owner_events.client_b);
    select_xi1_motion(&mut owner_events, CLIENT_A, WINDOW, XI_XTEST_POINTER);
    select_xi2(
        &mut owner_events,
        CLIENT_A,
        WINDOW,
        XI_XTEST_POINTER,
        XI_MOTION,
    );
    grab_xi2_device(&mut owner_events, true, XI_MOTION);
    fake_device_motion(&mut owner_events, XI_XTEST_POINTER, 0, 0, &[150, 150]);

    let owner = kbd_map_drain(&mut owner_events.client_a);
    assert_eq!(
        xi2_event_ids(&owner),
        vec![(6, XI_XTEST_POINTER, XI_XTEST_POINTER)]
    );
    assert_eq!(
        decode_deliveries(&owner),
        vec![Delivery {
            xi2: true,
            kind: 6,
            window: WINDOW,
            child: CHILD,
        }],
        "C16: all core and XI2 deliveries contain only the natural XI_Motion"
    );
    assert_eq!(
        xi2_geometry(&owner),
        vec![(WINDOW, CHILD, 150, 150, 50, 50)],
        "C16: the event uses the request's absolute X/Y valuators"
    );
    assert!(xi1_device_event_ids(&owner).is_empty());
    assert!(kbd_map_drain(&mut owner_events.client_b).is_empty());
    assert_explicit_slave_grab_end_state(&owner_events, &[XI_XTEST_POINTER]);

    let mut exclusive = fixture();
    fake_core_motion(&mut exclusive, 0, 150, 150);
    let _ = kbd_map_drain(&mut exclusive.client_a);
    let _ = kbd_map_drain(&mut exclusive.client_b);
    select_xi1_motion(&mut exclusive, CLIENT_A, WINDOW, XI_XTEST_POINTER);
    grab_xi2_device(&mut exclusive, false, XI_MOTION);
    fake_device_motion(&mut exclusive, XI_XTEST_POINTER, 0, 0, &[150, 150]);

    let owner = kbd_map_drain(&mut exclusive.client_a);
    assert_eq!(
        xi2_event_ids(&owner),
        vec![(6, XI_XTEST_POINTER, XI_XTEST_POINTER)]
    );
    assert_eq!(
        decode_deliveries(&owner),
        vec![Delivery {
            xi2: true,
            kind: 6,
            window: WINDOW,
            child: CHILD,
        }],
        "C17: all core and XI2 deliveries contain only the grabbed XI_Motion"
    );
    assert_eq!(
        xi2_geometry(&owner),
        vec![(WINDOW, CHILD, 150, 150, 50, 50)]
    );
    assert!(xi1_device_event_ids(&owner).is_empty());
    assert!(kbd_map_drain(&mut exclusive.client_b).is_empty());
    assert_explicit_slave_grab_end_state(&exclusive, &[XI_XTEST_POINTER]);

    let mut xi1_only = fixture();
    fake_core_motion(&mut xi1_only, 0, 150, 150);
    let _ = kbd_map_drain(&mut xi1_only.client_a);
    let _ = kbd_map_drain(&mut xi1_only.client_b);
    select_xi1_motion(&mut xi1_only, CLIENT_A, WINDOW, XI_XTEST_POINTER);
    grab_xi2_device(&mut xi1_only, true, XI_MOTION);
    fake_device_motion(&mut xi1_only, XI_XTEST_POINTER, 0, 0, &[150, 150]);

    let owner = kbd_map_drain(&mut xi1_only.client_a);
    assert!(xi2_event_ids(&owner).is_empty());
    assert_eq!(
        xi1_device_event_ids(&owner),
        vec![(
            XI_FIRST_EVENT + yserver_core::xinput::XI_DEVICE_MOTION_NOTIFY_OFFSET,
            XI_XTEST_POINTER as u8,
        )],
        "C10: XI1 still wins when the grab owner has no XI2 selection"
    );
    assert_eq!(owner[31], XI_XTEST_POINTER as u8 | 0x80, "C10 MORE_EVENTS");
    assert_eq!(
        xi1_valuator_values(&owner),
        vec![(0, 150), (1, 150)],
        "C10: direct FakeInput keeps both explicit device valuators"
    );
    assert!(kbd_map_drain(&mut xi1_only.client_b).is_empty());
    assert_explicit_slave_grab_end_state(&xi1_only, &[XI_XTEST_POINTER]);
}

#[test]
fn xtest_device_motion_preserves_partial_valuator_range() {
    let mut f = fixture();
    fake_core_motion(&mut f, 0, 150, 150);
    let _ = kbd_map_drain(&mut f.client_a);
    let _ = kbd_map_drain(&mut f.client_b);
    select_xi1_motion(&mut f, CLIENT_A, WINDOW, XI_XTEST_POINTER);
    select_xi2(&mut f, CLIENT_A, WINDOW, XI_XTEST_POINTER, XI_MOTION);
    grab_xi2_device(&mut f, true, XI_MOTION);
    fake_device_motion(&mut f, XI_XTEST_POINTER, 0, 1, &[175]);

    let owner = kbd_map_drain(&mut f.client_a);
    assert_eq!(
        xi2_event_ids(&owner),
        vec![(6, XI_XTEST_POINTER, XI_XTEST_POINTER)]
    );
    assert_eq!(
        u32::from_le_bytes(owner[112..116].try_into().unwrap()),
        0x2,
        "C18: XI_Motion carries only the requested Y valuator bit"
    );
    assert_eq!(
        u32::from_le_bytes(owner[116..120].try_into().unwrap()),
        0,
        "C18: second valuator mask word is empty"
    );
    assert_eq!(
        i32::from_le_bytes(owner[120..124].try_into().unwrap()),
        0,
        "C18: Xvfb 21.1.24 emits Y=0 for this nonzero first-valuator range"
    );
    assert_eq!(
        u32::from_le_bytes(owner[124..128].try_into().unwrap()),
        0,
        "C18: Y fraction is zero"
    );
    assert!(xi1_device_event_ids(&owner).is_empty());
    assert!(kbd_map_drain(&mut f.client_b).is_empty());
    assert_explicit_slave_grab_end_state(&f, &[XI_XTEST_POINTER]);
}

// ---- XTS XAllowDeviceEvents (spec invariant 8, per-device freeze) ----
//
// XTS `XI/AllowDeviceEvents.m`: `grabstartup` selects DeviceMotionNotify,
// DeviceButtonPress and DeviceButtonRelease for the button device on a
// window and warps the pointer to (1,1) in it; `ispfrozen` simulates an
// absolute one-axis DeviceMotion and reports "frozen" when the selecting
// client does not receive DeviceMotionNotify. On Xorg a frozen master
// pointer (core GrabPointer) does not freeze the slave (4): only the
// master's event is enqueued (../xserver mi/mieq.c:502-508, dix/events.c
// ComputeFreezes), so the slave's XI1 event arrives.

const XI_ALLOW_DEVICE_EVENTS: u8 = 19;
const ASYNC_THIS_DEVICE: u8 = 0;
const SYNC_THIS_DEVICE: u8 = 1;
const REPLAY_THIS_DEVICE: u8 = 2;

fn ade_fixture() -> Fixture {
    let mut state = ServerState::new();
    let client_a = kbd_map_client_id(&mut state, CLIENT_A);
    let client_b = kbd_map_client_id(&mut state, CLIENT_B);
    let mut f = Fixture {
        backend: KmsBackend::for_tests(),
        state,
        client_a,
        client_b,
    };
    // The XTS grab window is away from the root's left edge; the one-axis
    // X=0 probe moves the sprite out of it (Xorg still delivers the slave's
    // motion, see `slave_pass_target`).
    create_window(
        &mut f,
        CLIENT_A,
        WINDOW,
        yserver_core::resources::ROOT_WINDOW.0,
        80,
        285,
        200,
        200,
    );
    map_window(&mut f, CLIENT_A, WINDOW);
    f
}

/// XTS `grabstartup`: XSelectExtensionEvent of the three device classes on
/// the grab window, then the pointer warped to (1,1) in it.
fn ade_grabstartup() -> Fixture {
    let mut f = ade_fixture();
    let offsets = [
        yserver_core::xinput::XI_DEVICE_MOTION_NOTIFY_OFFSET,
        yserver_core::xinput::XI_DEVICE_BUTTON_PRESS_OFFSET,
        yserver_core::xinput::XI_DEVICE_BUTTON_RELEASE_OFFSET,
    ];
    let mut body = Vec::new();
    body.extend_from_slice(&WINDOW.to_le_bytes());
    body.extend_from_slice(&u16::try_from(offsets.len()).unwrap().to_le_bytes());
    body.extend_from_slice(&[0; 2]);
    for offset in offsets {
        let class = (u32::from(XI_XTEST_POINTER) << 8) | u32::from(XI_FIRST_EVENT + offset);
        body.extend_from_slice(&class.to_le_bytes());
    }
    dispatch(
        &mut f.state,
        &mut f.backend,
        CLIENT_A,
        5,
        XI2_OPCODE,
        XI_SELECT_EXTENSION_EVENT,
        &body,
    );
    assert_eq!(
        f.state.clients[&CLIENT_A].xi1_window_event_classes[&ResourceId(WINDOW)].len(),
        3,
        "XSelectExtensionEvent must register the three XI1 classes"
    );
    fake_core_motion(&mut f, 0, 81, 286);
    let _ = kbd_map_drain(&mut f.client_a);
    f
}

/// XTS `ispfrozen`: the device counts as frozen when the simulated
/// absolute DeviceMotion does not reach the selecting client.
fn ade_ispfrozen(f: &mut Fixture) -> bool {
    let _ = kbd_map_drain(&mut f.client_a);
    fake_device_motion(f, XI_XTEST_POINTER, 0, 0, &[0]);
    let received = kbd_map_drain(&mut f.client_a);
    let motion = XI_FIRST_EVENT + yserver_core::xinput::XI_DEVICE_MOTION_NOTIFY_OFFSET;
    !xi1_device_event_ids(&received)
        .iter()
        .any(|(kind, device)| *kind == motion && u16::from(*device) == XI_XTEST_POINTER)
}

fn allow_device_events(f: &mut Fixture, device: u16, mode: u8) {
    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    body.extend_from_slice(&[mode, u8::try_from(device).unwrap(), 0, 0]);
    dispatch(
        &mut f.state,
        &mut f.backend,
        CLIENT_A,
        10,
        XI2_OPCODE,
        XI_ALLOW_DEVICE_EVENTS,
        &body,
    );
}

fn grab_core_pointer_sync_sync(f: &mut Fixture, client: u32) {
    let mut body = Vec::with_capacity(20);
    body.extend_from_slice(&WINDOW.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes()); // NoEventMask
    body.extend_from_slice(&[0, 0]); // GrabModeSync, GrabModeSync
    body.extend_from_slice(&0u32.to_le_bytes()); // no confinement
    body.extend_from_slice(&0u32.to_le_bytes()); // no cursor
    body.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    dispatch(&mut f.state, &mut f.backend, client, 11, 26, 0, &body); // GrabPointer
    assert!(
        f.state
            .active_pointer_grab
            .is_some_and(|grab| grab.owner == yserver_protocol::x11::ClientId(client)),
        "GrabPointer installs the core grab"
    );
}

fn grab_core_keyboard_sync_sync(f: &mut Fixture) {
    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&WINDOW.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    body.extend_from_slice(&[0, 0, 0, 0]); // sync, sync, pad
    dispatch(&mut f.state, &mut f.backend, CLIENT_A, 12, 31, 0, &body); // GrabKeyboard
    assert!(
        f.state.active_keyboard_grab.is_some(),
        "GrabKeyboard installs the core grab"
    );
}

fn ungrab_core(f: &mut Fixture) {
    dispatch(
        &mut f.state,
        &mut f.backend,
        CLIENT_A,
        13,
        27,
        0,
        &0u32.to_le_bytes(),
    ); // UngrabPointer
    dispatch(
        &mut f.state,
        &mut f.backend,
        CLIENT_A,
        14,
        32,
        0,
        &0u32.to_le_bytes(),
    ); // UngrabKeyboard
}

fn assert_ade_end_state(f: &mut Fixture, xi1_entries: &[u16]) {
    assert!(
        f.state.sync_pending.is_empty(),
        "no input event stays queued"
    );
    assert!(
        f.state.active_keyboard_grab.is_none(),
        "keyboard grab ended"
    );
    let _ = kbd_map_drain(&mut f.client_a);
    assert!(f.state.active_pointer_grab.is_none(), "master grab ended");
    assert!(f.state.xi2_pointer_grabs.is_empty(), "slave grabs ended");
    assert!(f.state.xi1_active_grabs.is_empty(), "XI1 grabs ended");
    assert_xi1_end_state(f, xi1_entries);
    assert!(
        f.state.xi2_detached_masters.is_empty(),
        "no slave is detached"
    );
    assert_button_state_empty(f);
}

#[test]
fn xts_ade3_double_grab_async_this_device_thaws() {
    let mut f = ade_grabstartup();
    grab_core_pointer_sync_sync(&mut f, CLIENT_A);
    grab_core_keyboard_sync_sync(&mut f);
    let _ = kbd_map_drain(&mut f.client_a);

    allow_device_events(&mut f, XI_XTEST_POINTER, ASYNC_THIS_DEVICE);
    assert!(
        !ade_ispfrozen(&mut f),
        "DeviceMotionNotify of the unfrozen slave reaches the client"
    );

    ungrab_core(&mut f);
    assert_ade_end_state(&mut f, &[XI_MASTER_POINTER, 3]);
}

#[test]
fn xts_ade4_not_frozen_async_this_device() {
    let mut f = ade_grabstartup();
    allow_device_events(&mut f, XI_XTEST_POINTER, ASYNC_THIS_DEVICE);
    assert!(!ade_ispfrozen(&mut f), "no device was frozen");
    assert_ade_end_state(&mut f, &[]);
}

#[test]
fn xts_ade6_sync_this_device_no_freeze() {
    let mut f = ade_grabstartup();
    allow_device_events(&mut f, XI_XTEST_POINTER, SYNC_THIS_DEVICE);
    assert!(!ade_ispfrozen(&mut f), "SyncThisDevice froze nothing");
    assert_ade_end_state(&mut f, &[]);
}

#[test]
fn xts_ade9_replay_this_device_not_frozen() {
    let mut f = ade_grabstartup();
    fake_core_motion(&mut f, 0, 81, 286);
    allow_device_events(&mut f, XI_XTEST_POINTER, REPLAY_THIS_DEVICE);
    assert!(!ade_ispfrozen(&mut f), "ReplayThisDevice froze nothing");
    assert_ade_end_state(&mut f, &[]);
}

/// XTS purposes 4, 6 and 9 say "the device is not frozen by the client, so
/// the call has no effect". Here another client's synchronous core grab
/// freezes the master pointer; the call must not thaw it, and the slave's
/// DeviceMotionNotify must still reach the selecting client.
fn ade_no_effect_while_other_client_freezes_master(mode: u8) {
    let mut f = ade_grabstartup();
    grab_core_pointer_sync_sync(&mut f, CLIENT_B);
    let _ = kbd_map_drain(&mut f.client_b);

    allow_device_events(&mut f, XI_XTEST_POINTER, mode);
    assert!(
        f.state
            .xi1_frozen
            .get(&XI_MASTER_POINTER)
            .is_some_and(yserver_core::server::Xi1Freeze::frozen),
        "A's AllowDeviceEvents does not thaw B's frozen master"
    );
    assert!(
        !ade_ispfrozen(&mut f),
        "the slave's DeviceMotionNotify reaches A while B freezes the master"
    );

    dispatch(
        &mut f.state,
        &mut f.backend,
        CLIENT_B,
        15,
        27,
        0,
        &0u32.to_le_bytes(),
    ); // UngrabPointer
    let _ = kbd_map_drain(&mut f.client_b);
    assert_ade_end_state(&mut f, &[XI_MASTER_POINTER, 3]);
}

#[test]
fn xts_ade4_async_this_device_while_other_client_freezes_master() {
    ade_no_effect_while_other_client_freezes_master(ASYNC_THIS_DEVICE);
}

#[test]
fn xts_ade6_sync_this_device_while_other_client_freezes_master() {
    ade_no_effect_while_other_client_freezes_master(SYNC_THIS_DEVICE);
}

#[test]
fn xts_ade9_replay_this_device_while_other_client_freezes_master() {
    ade_no_effect_while_other_client_freezes_master(REPLAY_THIS_DEVICE);
}

fn fake_device_button(f: &mut Fixture, device: u16, detail: u8, press: bool) {
    let offset = if press {
        yserver_core::xinput::XI_DEVICE_BUTTON_PRESS_OFFSET
    } else {
        yserver_core::xinput::XI_DEVICE_BUTTON_RELEASE_OFFSET
    };
    let mut body = vec![0u8; 32];
    body[0] = XI_FIRST_EVENT + offset;
    body[1] = detail;
    body[31] = u8::try_from(device).unwrap();
    dispatch(
        &mut f.state,
        &mut f.backend,
        CLIENT_A,
        16,
        XTEST_OPCODE,
        XTEST_FAKE_INPUT,
        &body,
    );
}

/// XTS purpose 14: AllowDeviceEvents(SyncAll) with no grab has no effect;
/// the following DeviceButtonPress selects an XI1 implicit grab on the grab
/// window (Task 4) and the probe motion still reaches the client.
#[test]
fn xts_ade14_sync_all_without_grabs_then_button_press_not_frozen() {
    let mut f = ade_grabstartup();
    allow_device_events(&mut f, XI_XTEST_POINTER, 5); // SyncAll
    fake_device_button(&mut f, XI_XTEST_POINTER, 1, true);
    assert!(
        !f.state.xi1_active_grabs.is_empty(),
        "the delivered DeviceButtonPress creates the XI1 implicit grab"
    );
    assert!(
        !ade_ispfrozen(&mut f),
        "no grab froze the device after SyncAll"
    );
    fake_device_button(&mut f, XI_XTEST_POINTER, 1, false);
    let _ = kbd_map_drain(&mut f.client_a);
    assert_ade_end_state(&mut f, &[]);
}

/// Xorg runs the slave's pass before the master's and only the master's pass
/// moves the sprite (`../xserver/mi/mieq.c:504-508`): a slave selector gets
/// the motion leaving its window, a master selector the motion entering its
/// own (measured on Xvfb 21.1.24; same shape as XTS AllowDeviceEvents'
/// X=0 probe).
#[test]
fn xtest_slave_motion_uses_sprite_before_move_master_uses_after() {
    let mut f = setup_x_case();
    select_xi1_motion(&mut f, CLIENT_A, WINDOW, XI_XTEST_POINTER);
    let _ = kbd_map_drain(&mut f.client_a);
    let _ = kbd_map_drain(&mut f.client_b);

    // Sprite in W -> OUTSIDE: A's XI1 slave selection on W fires, B's master
    // selection on OUTSIDE fires.
    fake_device_motion(&mut f, XI_XTEST_POINTER, 0, 0, &[650, 150]);
    assert_eq!(
        xi1_device_event_ids(&kbd_map_drain(&mut f.client_a)),
        vec![(
            XI_FIRST_EVENT + yserver_core::xinput::XI_DEVICE_MOTION_NOTIFY_OFFSET,
            XI_XTEST_POINTER as u8,
        )]
    );
    assert_eq!(
        xi2_event_ids(&kbd_map_drain(&mut f.client_b)),
        vec![(6, XI_MASTER_POINTER, XI_XTEST_POINTER)]
    );

    // Sprite OUTSIDE -> W: A's slave form follows the old window (OUTSIDE,
    // not selected by A); A's master selection (from `setup_x_case`) and the
    // new window W give the master form instead.
    fake_device_motion(&mut f, XI_XTEST_POINTER, 0, 0, &[150, 150]);
    let a = kbd_map_drain(&mut f.client_a);
    assert!(
        xi1_device_event_ids(&a).is_empty(),
        "no slave form from OUTSIDE"
    );
    assert_eq!(
        xi2_event_ids(&a),
        vec![(6, XI_MASTER_POINTER, XI_XTEST_POINTER)]
    );
    assert!(kbd_map_drain(&mut f.client_b).is_empty());
    assert_end_state(&f);
}

/// KMS starts its cursor at the primary-output centre. The first motion after
/// startup, with no preparatory warp, routes the slave form from that actual
/// sprite window (`../xserver/Xi/exevents.c:1854-1855,1950`).
#[test]
fn first_slave_motion_uses_kms_initial_sprite_position_without_warp() {
    let mut f = fixture();
    let (initial_x, initial_y) = f.backend.initial_pointer_position();
    f.state.seed_master_sprite_position(initial_x, initial_y);
    let left = i16::try_from(initial_x - 100).unwrap();
    let top = i16::try_from(initial_y - 100).unwrap();
    create_window(
        &mut f,
        CLIENT_A,
        INITIAL_WINDOW,
        yserver_core::resources::ROOT_WINDOW.0,
        left,
        top,
        200,
        200,
    );
    map_window(&mut f, CLIENT_A, INITIAL_WINDOW);
    select_xi2(
        &mut f,
        CLIENT_A,
        INITIAL_WINDOW,
        XI_XTEST_POINTER,
        XI_MOTION,
    );

    let outside_x = initial_x + 150;
    fake_device_motion(&mut f, XI_XTEST_POINTER, 0, 0, &[outside_x, initial_y]);
    let deliveries = decode_deliveries(&kbd_map_drain(&mut f.client_a));
    assert_eq!(deliveries.len(), 1, "the first motion gets one slave form");
    assert_eq!(deliveries[0].kind, 6);
    assert!(deliveries[0].xi2);
    assert_eq!(deliveries[0].window, INITIAL_WINDOW);
    assert_eq!(f.state.pointer_root, (outside_x as i16, initial_y as i16));
    assert_end_state(&f);
}
