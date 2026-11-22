use super::*;

use std::os::unix::net::UnixStream;

use yserver_core::{backend::Backend, core_loop::process_request, server::ServerState};
use yserver_protocol::x11::ResourceId;

const CLIENT_A: u32 = 0xA753;
const CLIENT_B: u32 = 0xA754;
const WINDOW: u32 = 0x0010_A753;
const CHILD: u32 = 0x0020_A753;
const OUTSIDE: u32 = 0x0010_A754;
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
