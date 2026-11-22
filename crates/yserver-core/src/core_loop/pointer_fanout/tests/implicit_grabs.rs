use super::*;

use crate::{
    backend::recording::RecordingBackend,
    core_loop::InputOrigin,
    resources::{ROOT_VISUAL, ROOT_WINDOW},
};
use yserver_protocol::x11::{RequestHeader, ResourceId, SequenceNumber};

const CLIENT_A: u32 = 0xA753;
const CLIENT_B: u32 = 0xA754;
const W: u32 = 0x0010_A753;
const D: u32 = 0x0020_A753;
const P: u32 = 0x0050_A753;
const OUT: u32 = 0x0010_A754;
const HOST_W: u32 = 0xCAFE_A753;
const HOST_OUT: u32 = 0xCAFE_A754;
const XI2_OPCODE: u8 = 137;
const XI_SELECT_EVENTS: u8 = 46;
const XI1_GRAB_DEVICE: u8 = 13;
const XI_GRAB_DEVICE: u8 = 51;
const XI_UNGRAB_DEVICE: u8 = 52;
const XI_MOTION: u32 = 1 << 6;
const XI_BUTTON_PRESS: u32 = 1 << 4;
const XI_BUTTON_RELEASE: u32 = 1 << 5;
const XI_BUTTONS: u32 = XI_BUTTON_PRESS | XI_BUTTON_RELEASE;
const XI_GRAB_MASK: u32 = XI_MOTION | XI_BUTTONS;
const XI1_PRESS: u32 = 1 << crate::xinput::XI_DEVICE_BUTTON_PRESS_OFFSET;
const XI1_RELEASE: u32 = 1 << crate::xinput::XI_DEVICE_BUTTON_RELEASE_OFFSET;
const XI1_MOTION: u32 = 1 << crate::xinput::XI_DEVICE_MOTION_NOTIFY_OFFSET;
const XI_MASTER: u16 = 2;
const XI_XTEST: u16 = 4;
const XI_ALL_DEVICES: u16 = 0;
const CORE_POINTER: u32 = 0x4c;
const CORE_GRAB_POINTER: u8 = 26;
const CORE_UNGRAB_POINTER: u8 = 27;
const CORE_CREATE_PIXMAP: u8 = 53;
const CORE_CREATE_CURSOR: u8 = 93;
const CORE_UNMAP_WINDOW: u8 = 10;
const CORE_UNMAP_SUBWINDOWS: u8 = 11;
const GRAB_PIXMAP_A: u32 = 0x0030_A753;
const GRAB_CURSOR_A: u32 = 0x0040_A753;
const GRAB_PIXMAP_B: u32 = 0x0030_A754;
const GRAB_CURSOR_B: u32 = 0x0040_A754;

pub(super) struct Fixture {
    pub(super) state: ServerState,
    pub(super) backend: RecordingBackend,
    pub(super) xid_map: HostXidMap,
    pub(super) client_a: crate::transport::CapturedPeer,
    pub(super) client_b: crate::transport::CapturedPeer,
    pub(super) pointer: (i16, i16),
    pub(super) time: u32,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Packet {
    pub(super) xi2: bool,
    pub(super) kind: u16,
    pub(super) device: u16,
    pub(super) source: u16,
    pub(super) window: u32,
    pub(super) scroll_value: Option<i32>,
    pub(super) child: u32,
    pub(super) event_x: i16,
    pub(super) event_y: i16,
}

// Packet equality covers event routing. Tests that care about FixUpEvent
// geometry assert child and event coordinates explicitly.
impl PartialEq for Packet {
    fn eq(&self, other: &Self) -> bool {
        (
            self.xi2,
            self.kind,
            self.device,
            self.source,
            self.window,
            self.scroll_value,
        ) == (
            other.xi2,
            other.kind,
            other.device,
            other.source,
            other.window,
            other.scroll_value,
        )
    }
}

impl Eq for Packet {}

fn fixture() -> Fixture {
    let mut state = ServerState::new();
    let client_a = install_capture_client(&mut state, CLIENT_A);
    let client_b = install_capture_client(&mut state, CLIENT_B);
    let mut f = Fixture {
        state,
        backend: RecordingBackend::new(),
        xid_map: HostXidMap::new(),
        client_a,
        client_b,
        pointer: (145, 145),
        time: 1000,
    };

    create_window(&mut f, CLIENT_A, W, 0, 100, 100, 300, 300);
    map_window(&mut f, CLIENT_A, W);
    create_window(&mut f, CLIENT_A, D, W, 20, 20, 100, 100);
    map_window(&mut f, CLIENT_A, D);
    create_window(&mut f, CLIENT_B, OUT, 0, 550, 100, 300, 300);
    map_window(&mut f, CLIENT_B, OUT);
    f.xid_map.insert(HOST_W, ResourceId(W));
    f.xid_map.insert(HOST_OUT, ResourceId(OUT));

    let _ = read_all_capture_available(&mut f.client_a);
    let _ = read_all_capture_available(&mut f.client_b);
    f
}

/// Rebuilds W below a mapped top-level parent while keeping the measured
/// root geometry. The ordinary fixture makes W a root child, so the
/// UnmapSubwindows ancestor case needs this request-dispatched variant.
fn fixture_with_parent() -> Fixture {
    let mut f = fixture();
    dispatch(&mut f, CLIENT_A, 4, 0, &W.to_le_bytes()); // DestroyWindow
    create_window(&mut f, CLIENT_A, P, 0, 100, 100, 300, 300);
    map_window(&mut f, CLIENT_A, P);
    create_window(&mut f, CLIENT_A, W, P, 0, 0, 300, 300);
    map_window(&mut f, CLIENT_A, W);
    create_window(&mut f, CLIENT_A, D, W, 20, 20, 100, 100);
    map_window(&mut f, CLIENT_A, D);
    f
}

pub(super) fn dispatch(f: &mut Fixture, client: u32, opcode: u8, data: u8, body: &[u8]) {
    crate::core_loop::process_request::process_request(
        &mut f.state,
        &mut f.backend,
        ClientId(client),
        SequenceNumber(1),
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

pub(super) fn create_window(
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
    body.extend_from_slice(&if parent == 0 { ROOT_WINDOW.0 } else { parent }.to_le_bytes());
    body.extend_from_slice(&x.to_le_bytes());
    body.extend_from_slice(&y.to_le_bytes());
    body.extend_from_slice(&width.to_le_bytes());
    body.extend_from_slice(&height.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes()); // border width
    body.extend_from_slice(&1u16.to_le_bytes()); // InputOutput
    body.extend_from_slice(&ROOT_VISUAL.0.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // no attributes
    dispatch(f, client, 1, 24, &body); // CreateWindow
    assert!(f.state.resources.window(ResourceId(window)).is_some());
}

pub(super) fn map_window(f: &mut Fixture, client: u32, window: u32) {
    dispatch(f, client, 8, 0, &window.to_le_bytes()); // MapWindow
    assert_eq!(
        f.state
            .resources
            .window(ResourceId(window))
            .unwrap()
            .map_state,
        crate::resources::MapState::Viewable
    );
}

pub(super) fn select_xi2(f: &mut Fixture, client: u32, window: u32, device: u16, mask: u32) {
    let mut body = Vec::with_capacity(20);
    body.extend_from_slice(&window.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes());
    body.extend_from_slice(&[0; 2]);
    body.extend_from_slice(&device.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes());
    body.extend_from_slice(&mask.to_le_bytes());
    dispatch(f, client, XI2_OPCODE, XI_SELECT_EVENTS, &body);
    assert_eq!(
        f.state.clients[&client]
            .xi2_masks
            .get(&(ResourceId(window), device)),
        Some(&u64::from(mask)),
        "XISelectEvents must register the test's real selection"
    );
}

pub(super) fn select_core(f: &mut Fixture, client: u32, window: u32, mask: u32) {
    let mut body = Vec::with_capacity(12);
    body.extend_from_slice(&window.to_le_bytes());
    body.extend_from_slice(&(1u32 << 11).to_le_bytes()); // CWEventMask
    body.extend_from_slice(&mask.to_le_bytes());
    dispatch(f, client, 2, 0, &body); // ChangeWindowAttributes
    assert_eq!(
        f.state.clients[&client]
            .event_masks
            .get(&ResourceId(window)),
        Some(&mask),
        "ChangeWindowAttributes must register the test's real selection"
    );
}

fn select_xi1(f: &mut Fixture, client: u32, window: u32, device: u8, mask: u32) {
    let classes: Vec<u32> = [
        crate::xinput::XI_DEVICE_BUTTON_PRESS_OFFSET,
        crate::xinput::XI_DEVICE_BUTTON_RELEASE_OFFSET,
        crate::xinput::XI_DEVICE_MOTION_NOTIFY_OFFSET,
    ]
    .into_iter()
    .filter(|offset| mask & (1 << offset) != 0)
    .map(|offset| (u32::from(device) << 8) | u32::from(crate::server::XI_FIRST_EVENT + offset))
    .collect();
    let mut body = Vec::with_capacity(8 + classes.len() * 4);
    body.extend_from_slice(&window.to_le_bytes());
    body.extend_from_slice(&u16::try_from(classes.len()).unwrap().to_le_bytes());
    body.extend_from_slice(&[0; 2]);
    for class in &classes {
        body.extend_from_slice(&class.to_le_bytes());
    }
    dispatch(f, client, XI2_OPCODE, 6, &body); // XSelectExtensionEvent
    let selected = &f.state.clients[&client].xi1_window_event_classes[&ResourceId(window)];
    assert!(
        classes.iter().all(|class| selected.contains(class)),
        "XSelectExtensionEvent must install the test's real XI1 classes"
    );
}

fn grab_xi2_device(
    f: &mut Fixture,
    client: u32,
    window: u32,
    device: u16,
    owner_events: bool,
    pointer_mode: u8,
    paired_device_mode: u8,
    mask: u32,
) {
    let mut body = Vec::with_capacity(24);
    body.extend_from_slice(&window.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    body.extend_from_slice(&0u32.to_le_bytes()); // no cursor
    body.extend_from_slice(&device.to_le_bytes());
    body.extend_from_slice(&[pointer_mode, paired_device_mode, u8::from(owner_events), 0]);
    body.extend_from_slice(&1u16.to_le_bytes()); // one mask word
    body.extend_from_slice(&mask.to_le_bytes());
    dispatch(f, client, XI2_OPCODE, XI_GRAB_DEVICE, &body);
}

fn grab_reply_status(bytes: &[u8]) -> u8 {
    let mut offset = 0;
    while offset + 32 <= bytes.len() {
        match bytes[offset] {
            1 => return bytes[offset + 8],
            35 => {
                let units = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap());
                offset += 32 + usize::try_from(units).unwrap() * 4;
            }
            _ => offset += 32,
        }
    }
    panic!("grab status reply is missing from the client stream");
}

fn grab_xi1_device(f: &mut Fixture, client: u32, window: u32, device: u8) -> u8 {
    let mut body = Vec::with_capacity(16);
    body.extend_from_slice(&window.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    body.extend_from_slice(&0u16.to_le_bytes()); // no event classes
    body.extend_from_slice(&[1, 1, 0, device]); // async modes, owner_events=false
    body.extend_from_slice(&[0; 2]);
    dispatch(f, client, XI2_OPCODE, XI1_GRAB_DEVICE, &body);
    let peer = if client == CLIENT_A {
        &mut f.client_a
    } else {
        &mut f.client_b
    };
    grab_reply_status(&read_all_capture_available(peer))
}

fn grab_button(f: &mut Fixture, client: u32, window: u32, keyboard_mode: u8) {
    let mut body = Vec::with_capacity(20);
    body.extend_from_slice(&window.to_le_bytes());
    body.extend_from_slice(&0x000cu16.to_le_bytes()); // ButtonPress|ButtonRelease
    body.push(1); // GrabModeAsync pointer
    body.push(keyboard_mode);
    body.extend_from_slice(&0u32.to_le_bytes()); // no confinement
    body.extend_from_slice(&0u32.to_le_bytes()); // no cursor
    body.push(1); // button 1
    body.push(0); // pad
    body.extend_from_slice(&0u16.to_le_bytes()); // no modifiers
    dispatch(f, client, 28, 0, &body); // GrabButton
    assert!(f.state.button_grabs.iter().any(|grab| {
        grab.owner == ClientId(client)
            && grab.grab_window == ResourceId(window)
            && grab.button == 1
            && grab.pointer_mode == 1
            && grab.keyboard_mode == keyboard_mode
    }));
}

fn assert_slave_implicit(f: &Fixture, device: u16, owner: u32, window: u32) {
    let grab = f
        .state
        .xi2_pointer_grabs
        .get(&device)
        .expect("the pressed slave must hold its implicit grab");
    assert_eq!(grab.owner, ClientId(owner));
    assert_eq!(grab.grab_window, ResourceId(window));
    assert!(grab.implicit);
    assert!(grab.via_xi2);
    assert!(!grab.owner_events);
    assert!(!grab.passive);
    assert!(f.state.xi2_detached_masters.is_empty());
}

fn assert_master_implicit(f: &Fixture, owner: u32, window: u32) {
    let grab = f
        .state
        .active_pointer_grab
        .expect("the accepted master press must hold its implicit grab");
    assert_eq!(grab.owner, ClientId(owner));
    assert_eq!(grab.grab_window, ResourceId(window));
    assert!(grab.implicit);
}

pub(super) fn assert_end_state(f: &Fixture) {
    assert_end_state_with_xi1_entries(f, &[]);
}

pub(super) fn assert_end_state_with_xi1_entries(f: &Fixture, expected_devices: &[u16]) {
    assert!(
        f.state.xi2_pointer_grabs.is_empty(),
        "all slave grabs ended"
    );
    assert!(f.state.active_pointer_grab.is_none(), "master grab ended");
    assert!(
        f.state.xi2_detached_masters.is_empty(),
        "no slave was detached"
    );
    assert!(f.state.xi1_active_grabs.is_empty(), "all XI1 grabs ended");
    let actual_devices: std::collections::BTreeSet<_> =
        f.state.xi1_frozen.keys().copied().collect();
    let expected_devices: std::collections::BTreeSet<_> =
        expected_devices.iter().copied().collect();
    assert_eq!(
        actual_devices, expected_devices,
        "XI1 sync entries match the requests used by this scenario"
    );
    assert!(f.state.xi1_frozen.values().all(|freeze| {
        freeze.state == crate::server::Xi1SyncState::Thawed
            && freeze.other.is_none()
            && freeze.stored.is_none()
    }));
}

pub(super) fn send(
    f: &mut Fixture,
    origin: InputOrigin,
    kind: PointerEventKind,
    detail: u8,
    x: i16,
    y: i16,
) {
    let host_xid = if x >= 550 { HOST_OUT } else { HOST_W };
    let time = f.time;
    f.time += 1;
    let event = HostPointerEvent {
        origin,
        kind,
        host_xid,
        detail,
        time,
        root_x: x,
        root_y: y,
        event_x: if x >= 550 { x - 550 } else { x - 100 },
        event_y: y - 100,
        state: 0,
        ..motion_event()
    };
    let dropped =
        pointer_event_fanout_to_state(&mut f.state, &mut f.backend, &f.xid_map, event, true, false);
    assert!(dropped.is_empty(), "test input must not disconnect clients");
    f.pointer = (x, y);
}

pub(super) fn press_over_d(f: &mut Fixture, origin: InputOrigin, button: u8) {
    send(f, origin, PointerEventKind::MotionNotify, 0, 150, 150);
    let _ = read_all_capture_available(&mut f.client_a);
    let _ = read_all_capture_available(&mut f.client_b);
    send(f, origin, PointerEventKind::ButtonPress, button, 150, 150);
}

pub(super) fn move_to_out(f: &mut Fixture, origin: InputOrigin) {
    send(f, origin, PointerEventKind::MotionNotify, 0, 650, 150);
}

pub(super) fn release(f: &mut Fixture, origin: InputOrigin, button: u8) {
    let (x, y) = f.pointer;
    send(f, origin, PointerEventKind::ButtonRelease, button, x, y);
}

pub(super) fn final_motion(f: &mut Fixture, origin: InputOrigin) {
    send(f, origin, PointerEventKind::MotionNotify, 0, 660, 160);
}

fn ungrab_xi2_device(f: &mut Fixture, client: u32, device: u16) {
    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    body.extend_from_slice(&device.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes());
    dispatch(f, client, XI2_OPCODE, XI_UNGRAB_DEVICE, &body);
}

fn assert_xi1_implicit(
    f: &Fixture,
    owner: u32,
    window: u32,
    buttons_down: u16,
    device_mask: u32,
    master_grab: Option<(u32, u32)>,
) {
    let grab = f
        .state
        .xi1_active_grabs
        .get(&XI_XTEST)
        .expect("a selected DeviceButtonPress must create the XI1 implicit grab");
    assert_eq!(grab.owner, ClientId(owner));
    assert_eq!(grab.deviceid, XI_XTEST);
    assert_eq!(grab.grab_window, ResourceId(window));
    assert!(!grab.owner_events);
    assert_eq!(grab.passive_detail, None);
    let debug = format!("{grab:?}");
    assert!(
        debug.contains("implicit: true"),
        "grab must be implicit: {debug}"
    );
    assert!(
        debug.contains(&format!("device_mask: {device_mask}")),
        "grab must snapshot the XI1 classes: {debug}"
    );
    assert!(f.state.xi2_pointer_grabs.is_empty());
    assert!(f.state.xi2_detached_masters.is_empty());
    match (master_grab, f.state.active_pointer_grab) {
        (None, None) => {}
        (Some((master_owner, master_window)), Some(grab)) => {
            assert_eq!(grab.owner, ClientId(master_owner));
            assert_eq!(grab.grab_window, ResourceId(master_window));
            assert!(!grab.via_xi2);
            assert!(grab.implicit);
        }
        (expected, actual) => panic!("master grab mismatch: expected {expected:?}, got {actual:?}"),
    }
    assert_eq!(f.state.buttons_down, buttons_down);
}

fn assert_buttons_released(f: &Fixture) {
    assert_eq!(f.state.buttons_down, 0, "master buttons are all released");
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

fn assert_xi1_end_state(f: &Fixture) {
    assert_end_state(f);
    assert_buttons_released(f);
}

fn grab_core_with_sync_keyboard(
    f: &mut Fixture,
    client: u32,
    window: u32,
) -> (crate::server::ActivePointerGrab, u32) {
    let (pixmap, cursor) = if client == CLIENT_A {
        (GRAB_PIXMAP_A, GRAB_CURSOR_A)
    } else {
        (GRAB_PIXMAP_B, GRAB_CURSOR_B)
    };
    let mut pixmap_body = Vec::with_capacity(12);
    pixmap_body.extend_from_slice(&pixmap.to_le_bytes());
    pixmap_body.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    pixmap_body.extend_from_slice(&(1u32 | (1 << 16)).to_le_bytes()); // 1x1
    dispatch(f, client, CORE_CREATE_PIXMAP, 1, &pixmap_body);

    let mut cursor_body = Vec::with_capacity(24);
    cursor_body.extend_from_slice(&cursor.to_le_bytes());
    cursor_body.extend_from_slice(&pixmap.to_le_bytes());
    cursor_body.extend_from_slice(&[0; 20]); // colors, hotspot, protocol padding
    dispatch(f, client, CORE_CREATE_CURSOR, 0, &cursor_body);
    let cursor_host_xid = f
        .state
        .resources
        .cursor_host_xid(ResourceId(cursor))
        .expect("CreateCursor through process_request installs a host cursor");

    let mut grab_body = Vec::with_capacity(20);
    grab_body.extend_from_slice(&window.to_le_bytes());
    grab_body.extend_from_slice(&0u16.to_le_bytes()); // no core events
    grab_body.extend_from_slice(&[1, 0]); // async pointer, sync keyboard
    grab_body.extend_from_slice(&0u32.to_le_bytes()); // no confinement
    grab_body.extend_from_slice(&cursor.to_le_bytes());
    grab_body.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    dispatch(f, client, CORE_GRAB_POINTER, 0, &grab_body);

    let grab = f
        .state
        .active_pointer_grab
        .expect("GrabPointer through process_request installs the grab");
    assert_eq!(grab.owner, ClientId(client));
    assert_eq!(grab.grab_window, ResourceId(window));
    assert_eq!(grab.cursor, ResourceId(cursor));
    assert!(!grab.implicit);
    assert_eq!(
        f.state.xi1_frozen[&crate::xinput::DEVICEID_MASTER_KEYBOARD].other,
        Some(ClientId(client)),
        "sync keyboard mode holds the paired keyboard"
    );
    (grab, cursor_host_xid)
}

fn ungrab_core_pointer(f: &mut Fixture, client: u32) {
    dispatch(f, client, CORE_UNGRAB_POINTER, 0, &0u32.to_le_bytes());
}

pub(super) fn read_packets(peer: &mut crate::transport::CapturedPeer) -> Vec<Packet> {
    let bytes = read_all_capture_available(peer);
    let mut packets = Vec::new();
    let mut offset = 0;
    while offset + 32 <= bytes.len() {
        if bytes[offset] == 35 {
            let extra = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap());
            let kind = u16::from_le_bytes([bytes[offset + 8], bytes[offset + 9]]);
            if (4..=6).contains(&kind) {
                packets.push(Packet {
                    xi2: true,
                    kind,
                    device: u16::from_le_bytes([bytes[offset + 10], bytes[offset + 11]]) & 0x7fff,
                    source: u16::from_le_bytes(bytes[offset + 52..offset + 54].try_into().unwrap()),
                    window: u32::from_le_bytes(bytes[offset + 24..offset + 28].try_into().unwrap()),
                    scroll_value: (kind == 6 && extra == 28).then(|| {
                        i32::from_le_bytes(bytes[offset + 136..offset + 140].try_into().unwrap())
                    }),
                    child: u32::from_le_bytes(bytes[offset + 28..offset + 32].try_into().unwrap()),
                    event_x: i16::try_from(
                        i32::from_le_bytes(bytes[offset + 40..offset + 44].try_into().unwrap())
                            >> 16,
                    )
                    .expect("XI2 event x coordinate fits i16"),
                    event_y: i16::try_from(
                        i32::from_le_bytes(bytes[offset + 44..offset + 48].try_into().unwrap())
                            >> 16,
                    )
                    .expect("XI2 event y coordinate fits i16"),
                });
            }
            offset += 32 + usize::try_from(extra).unwrap() * 4;
        } else {
            let wire_kind = bytes[offset] & 0x7f;
            let xi1_kind = match wire_kind {
                code if code
                    == crate::server::XI_FIRST_EVENT
                        + crate::xinput::XI_DEVICE_BUTTON_PRESS_OFFSET =>
                {
                    Some(4)
                }
                code if code
                    == crate::server::XI_FIRST_EVENT
                        + crate::xinput::XI_DEVICE_BUTTON_RELEASE_OFFSET =>
                {
                    Some(5)
                }
                code if code
                    == crate::server::XI_FIRST_EVENT
                        + crate::xinput::XI_DEVICE_MOTION_NOTIFY_OFFSET =>
                {
                    Some(6)
                }
                _ => None,
            };
            if let Some(kind) = xi1_kind {
                let device = u16::from(bytes[offset + 31] & 0x7f);
                packets.push(Packet {
                    xi2: false,
                    kind,
                    device,
                    source: device,
                    window: u32::from_le_bytes(bytes[offset + 12..offset + 16].try_into().unwrap()),
                    scroll_value: None,
                    child: u32::from_le_bytes(bytes[offset + 16..offset + 20].try_into().unwrap()),
                    event_x: i16::from_le_bytes([bytes[offset + 24], bytes[offset + 25]]),
                    event_y: i16::from_le_bytes([bytes[offset + 26], bytes[offset + 27]]),
                });
            } else if (4..=6).contains(&u16::from(wire_kind)) {
                let kind = u16::from(wire_kind);
                packets.push(Packet {
                    xi2: false,
                    kind,
                    device: 0,
                    source: 0,
                    window: u32::from_le_bytes(bytes[offset + 12..offset + 16].try_into().unwrap()),
                    scroll_value: None,
                    child: u32::from_le_bytes(bytes[offset + 16..offset + 20].try_into().unwrap()),
                    event_x: i16::from_le_bytes([bytes[offset + 24], bytes[offset + 25]]),
                    event_y: i16::from_le_bytes([bytes[offset + 26], bytes[offset + 27]]),
                });
            }
            offset += 32;
        }
    }
    assert_eq!(offset, bytes.len(), "event stream fully consumed");
    packets
}

pub(super) fn xi(kind: u16, device: u16, source: u16, window: u32) -> Packet {
    Packet {
        xi2: true,
        kind,
        device,
        source,
        window,
        scroll_value: None,
        child: 0,
        event_x: 0,
        event_y: 0,
    }
}

pub(super) fn core(kind: u16, window: u32) -> Packet {
    Packet {
        xi2: false,
        kind,
        device: 0,
        source: 0,
        window,
        scroll_value: None,
        child: 0,
        event_x: 0,
        event_y: 0,
    }
}

fn xi1(kind: u16, device: u16, window: u32) -> Packet {
    Packet {
        xi2: false,
        kind,
        device,
        source: device,
        window,
        scroll_value: None,
        child: 0,
        event_x: 0,
        event_y: 0,
    }
}

#[derive(Clone, Copy)]
enum SlaveGrabEnd {
    Release,
    Ungrab,
    PhysicalRemoval,
}

fn check_slave_implicit_removal_subcase(ending: SlaveGrabEnd) -> Vec<&'static str> {
    const SOURCE: u64 = 0xA75_300;
    let mut f = fixture();
    let (device, origin) = match ending {
        SlaveGrabEnd::PhysicalRemoval => {
            let device = pointer_source(&mut f.state, SOURCE, true);
            (
                device,
                InputOrigin::Physical(crate::xinput::InputSourceId(SOURCE)),
            )
        }
        SlaveGrabEnd::Release | SlaveGrabEnd::Ungrab => (XI_XTEST, InputOrigin::XTest(XI_XTEST)),
    };
    select_xi2(&mut f, CLIENT_A, W, device, XI_GRAB_MASK);
    let (master_before, _) = grab_core_with_sync_keyboard(&mut f, CLIENT_A, OUT);
    let cursor_call_start = f.backend.grab_cursor_calls.len();
    let detached_before = f.state.xi2_detached_masters.clone();

    press_over_d(&mut f, origin, 1);
    assert_slave_implicit(&f, device, CLIENT_A, W);
    assert!(f.state.active_pointer_grab.is_some_and(|grab| {
        grab.owner == ClientId(CLIENT_A) && grab.grab_window == ResourceId(OUT) && !grab.implicit
    }));
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi(4, device, device, W)]
    );
    assert!(read_packets(&mut f.client_b).is_empty());

    match ending {
        SlaveGrabEnd::Release => {
            release(&mut f, origin, 1);
            assert_eq!(
                read_packets(&mut f.client_a),
                vec![xi(5, device, device, W)]
            );
            assert!(read_packets(&mut f.client_b).is_empty());
        }
        SlaveGrabEnd::Ungrab => {
            ungrab_xi2_device(&mut f, CLIENT_A, device);
            move_to_out(&mut f, origin);
            release(&mut f, origin, 1);
            assert!(read_packets(&mut f.client_a).is_empty());
            assert!(read_packets(&mut f.client_b).is_empty());
        }
        SlaveGrabEnd::PhysicalRemoval => {
            let source = crate::xinput::InputSourceId(SOURCE);
            xi_cleanup_source(&mut f.state, &mut f.backend, source);

            // DeviceRemoved drains physical button holds before calling
            // xi_cleanup_source (portable/devices.rs:754-760). This direct
            // cleanup probe deliberately leaves the active implicit grab in
            // place so it can verify cleanup removes it itself. The release
            // below only balances this manually driven fixture after the
            // cleanup boundary has been checked.
            assert!(!f.state.xi2_pointer_grabs.contains_key(&device));
            assert!(f.state.active_pointer_grab.is_some_and(|grab| {
                grab.owner == master_before.owner
                    && grab.grab_window == master_before.grab_window
                    && grab.event_mask == master_before.event_mask
                    && grab.cursor == master_before.cursor
                    && grab.time == master_before.time
                    && grab.owner_events == master_before.owner_events
                    && grab.via_xi2 == master_before.via_xi2
                    && grab.implicit == master_before.implicit
                    && grab.passive == master_before.passive
                    && grab.xi2_mask == master_before.xi2_mask
            }));
            assert_eq!(
                f.state.xi1_frozen[&crate::xinput::DEVICEID_MASTER_KEYBOARD].other,
                Some(ClientId(CLIENT_A))
            );
            assert!(
                !f.backend.grab_cursor_calls[cursor_call_start..].contains(&None),
                "implicit cleanup must preserve the master grab cursor"
            );
            assert_eq!(f.state.xi2_detached_masters, detached_before);
            assert!(
                !f.state.xi1_frozen.contains_key(&device),
                "implicit cleanup must not create slave freeze state"
            );

            release(&mut f, origin, 1);
            assert_eq!(
                read_packets(&mut f.client_a),
                vec![xi(5, device, device, W)]
            );
            assert!(read_packets(&mut f.client_b).is_empty());
            assert!(
                f.state.xi_unregister_facet(device).is_some(),
                "production removes a facet only after xi_cleanup_source"
            );

            // Unregister must not be needed to erase state left by cleanup.
            assert!(!f.state.xi2_pointer_grabs.contains_key(&device));
            assert!(f.state.active_pointer_grab.is_some_and(|grab| {
                grab.owner == master_before.owner
                    && grab.grab_window == master_before.grab_window
                    && grab.event_mask == master_before.event_mask
                    && grab.cursor == master_before.cursor
                    && grab.time == master_before.time
                    && grab.owner_events == master_before.owner_events
                    && grab.via_xi2 == master_before.via_xi2
                    && grab.implicit == master_before.implicit
                    && grab.passive == master_before.passive
                    && grab.xi2_mask == master_before.xi2_mask
            }));
            assert_eq!(
                f.state.xi1_frozen[&crate::xinput::DEVICEID_MASTER_KEYBOARD].other,
                Some(ClientId(CLIENT_A))
            );
            assert!(
                !f.backend.grab_cursor_calls[cursor_call_start..].contains(&None),
                "facet unregister must preserve the master grab cursor"
            );
            assert_eq!(f.state.xi2_detached_masters, detached_before);
            assert!(!f.state.xi1_frozen.contains_key(&device));
        }
    }

    let mut failures = Vec::new();
    if f.state.xi2_pointer_grabs.contains_key(&device) {
        failures.push("slave entry remains after its requested end");
    }
    if !f.state.active_pointer_grab.is_some_and(|grab| {
        grab.owner == ClientId(CLIENT_A) && grab.grab_window == ResourceId(OUT) && !grab.implicit
    }) {
        failures.push("master core grab did not survive");
    }
    if f.state
        .xi1_frozen
        .get(&crate::xinput::DEVICEID_MASTER_KEYBOARD)
        .and_then(|freeze| freeze.other)
        != Some(ClientId(CLIENT_A))
    {
        failures.push("paired keyboard hold did not survive");
    }
    if f.backend.grab_cursor_calls[cursor_call_start..].contains(&None) {
        failures.push("slave teardown cleared the master grab cursor");
    }
    if f.state.xi1_frozen.contains_key(&device) {
        failures.push("slave teardown wrote XI1 freeze state");
    }
    if f.state.xi2_detached_masters != detached_before {
        failures.push("slave teardown changed its attachment");
    }

    // Finish the independent core grab through its normal request path so
    // every fresh-fixture subcase also checks the fully released end state.
    ungrab_core_pointer(&mut f, CLIENT_A);
    if !f.state.xi2_pointer_grabs.is_empty()
        || f.state.active_pointer_grab.is_some()
        || !f.state.xi2_detached_masters.is_empty()
        || f.state
            .xi1_frozen
            .values()
            .any(|freeze| freeze.frozen() || freeze.other.is_some() || freeze.stored.is_some())
    {
        failures.push("subcase did not finish with an empty grab/freeze state");
    }
    failures
}

#[test]
fn motion_only_master_grab_gets_no_wheel_press_during_slave_grab() {
    const SOURCE: u64 = 0xA7566;
    let mut f = fixture();
    let source_device = pointer_source(&mut f.state, SOURCE, true);
    let origin = InputOrigin::Physical(crate::xinput::InputSourceId(SOURCE));
    select_xi2(&mut f, CLIENT_A, W, source_device, XI_BUTTONS | XI_MOTION);

    press_over_d(&mut f, origin, 1);
    assert_slave_implicit(&f, source_device, CLIENT_A, W);
    assert!(
        f.state.active_pointer_grab.is_none(),
        "the slave-only press does not create a master grab"
    );
    assert!(f.state.xi2_detached_masters.is_empty());
    let _ = read_packets(&mut f.client_a);

    grab_xi2_device(&mut f, CLIENT_B, OUT, XI_MASTER, false, 1, 1, XI_MOTION);
    let grab_reply = read_all_capture_available(&mut f.client_b);
    assert_eq!(
        grab_reply_status(&grab_reply),
        0,
        "B's master XIGrabDevice succeeds"
    );
    assert!(f.state.active_pointer_grab.is_some_and(|grab| {
        grab.owner == ClientId(CLIENT_B)
            && grab.grab_window == ResourceId(OUT)
            && !grab.owner_events
            && grab.via_xi2
            && !grab.implicit
            && grab.xi2_mask == u64::from(XI_MOTION)
    }));

    send(&mut f, origin, PointerEventKind::ButtonPress, 5, 150, 150);
    send(&mut f, origin, PointerEventKind::ButtonRelease, 5, 150, 150);

    let b_packets = read_packets(&mut f.client_b);
    assert_eq!(
        b_packets,
        vec![Packet {
            scroll_value: Some(1),
            ..xi(6, XI_MASTER, source_device, OUT)
        }],
        "B gets exactly the master scroll Motion and no emulated ButtonPress"
    );

    release(&mut f, origin, 1);
    assert!(f.state.xi2_pointer_grabs.is_empty());
    assert!(f.state.active_pointer_grab.is_some_and(|grab| {
        grab.owner == ClientId(CLIENT_B)
            && grab.grab_window == ResourceId(OUT)
            && grab.xi2_mask == u64::from(XI_MOTION)
            && !grab.implicit
    }));
    assert!(f.state.xi2_detached_masters.is_empty());
    let live_freeze_devices: std::collections::BTreeSet<_> =
        f.state.xi1_frozen.keys().copied().collect();
    assert_eq!(
        live_freeze_devices,
        [
            crate::xinput::DEVICEID_MASTER_KEYBOARD,
            crate::xinput::DEVICEID_MASTER_POINTER,
        ]
        .into_iter()
        .collect()
    );
    assert!(f.state.xi1_frozen.values().all(|freeze| {
        freeze.state == crate::server::Xi1SyncState::Thawed
            && freeze.other.is_none()
            && freeze.stored.is_none()
    }));
    assert!(read_packets(&mut f.client_b).is_empty());
}

fn run_i1(selection_device: u16) {
    let mut f = fixture();
    for (client, window) in [(CLIENT_A, W), (CLIENT_B, OUT)] {
        if selection_device == XI_ALL_DEVICES {
            select_xi2(&mut f, client, window, XI_ALL_DEVICES, XI_GRAB_MASK);
        } else {
            select_xi2(&mut f, client, window, XI_XTEST, XI_GRAB_MASK);
            select_xi2(&mut f, client, window, XI_MASTER, XI_GRAB_MASK);
        }
    }

    press_over_d(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert_slave_implicit(&f, XI_XTEST, CLIENT_A, W);
    assert_master_implicit(&f, CLIENT_A, W);

    let mut a = read_packets(&mut f.client_a);
    assert_eq!(a, vec![xi(4, 4, 4, W), xi(4, 2, 4, W)]);
    assert!(read_packets(&mut f.client_b).is_empty());
    move_to_out(&mut f, InputOrigin::XTest(XI_XTEST));
    a.extend(read_packets(&mut f.client_a));
    assert!(read_packets(&mut f.client_b).is_empty());
    release(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    a.extend(read_packets(&mut f.client_a));
    assert!(read_packets(&mut f.client_b).is_empty());
    final_motion(&mut f, InputOrigin::XTest(XI_XTEST));
    assert!(
        read_packets(&mut f.client_a).is_empty(),
        "A receives no pointer packets after the natural final motion"
    );
    assert_eq!(
        a,
        vec![
            xi(4, 4, 4, W),
            xi(4, 2, 4, W),
            xi(6, 4, 4, W),
            xi(6, 2, 4, W),
            xi(5, 4, 4, W),
            xi(5, 2, 4, W),
        ]
    );
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(6, 4, 4, OUT), xi(6, 2, 4, OUT)]
    );
    assert_end_state(&f);
}

#[test]
fn i1_slave_and_master_sequences_stay_on_press_window() {
    // I1: each client selects the slave and master forms explicitly.
    run_i1(XI_XTEST);
}

#[test]
fn i1a_xiall_devices_selection_matches_i1() {
    // I1a: same routing with the XIAllDevices selector used by Qt6.
    run_i1(XI_ALL_DEVICES);
}

#[test]
fn i2_slave_only_selector_gets_no_master_events() {
    let mut f = fixture();
    select_xi2(&mut f, CLIENT_A, W, XI_XTEST, XI_GRAB_MASK);
    select_xi2(&mut f, CLIENT_B, OUT, XI_MASTER, XI_GRAB_MASK);

    press_over_d(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert_slave_implicit(&f, XI_XTEST, CLIENT_A, W);
    assert!(f.state.active_pointer_grab.is_none());
    assert!(f.state.xi2_detached_masters.is_empty());
    assert_eq!(read_packets(&mut f.client_a), vec![xi(4, 4, 4, W)]);
    assert!(read_packets(&mut f.client_b).is_empty());

    move_to_out(&mut f, InputOrigin::XTest(XI_XTEST));
    assert_eq!(read_packets(&mut f.client_a), vec![xi(6, 4, 4, W)]);
    assert_eq!(read_packets(&mut f.client_b), vec![xi(6, 2, 4, OUT)]);
    release(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert_eq!(read_packets(&mut f.client_a), vec![xi(5, 4, 4, W)]);
    assert_eq!(read_packets(&mut f.client_b), vec![xi(5, 2, 4, OUT)]);
    final_motion(&mut f, InputOrigin::XTest(XI_XTEST));
    assert!(read_packets(&mut f.client_a).is_empty());
    assert_eq!(read_packets(&mut f.client_b), vec![xi(6, 2, 4, OUT)]);
    assert_end_state(&f);
}

#[test]
fn i3_core_selector_gets_release_during_slave_grab() {
    let mut f = fixture();
    select_xi2(&mut f, CLIENT_A, W, XI_XTEST, XI_GRAB_MASK);
    select_core(&mut f, CLIENT_B, OUT, CORE_POINTER);

    press_over_d(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert_slave_implicit(&f, XI_XTEST, CLIENT_A, W);
    assert!(f.state.active_pointer_grab.is_none());
    assert!(f.state.xi2_detached_masters.is_empty());
    assert_eq!(read_packets(&mut f.client_a), vec![xi(4, 4, 4, W)]);
    assert!(read_packets(&mut f.client_b).is_empty());

    move_to_out(&mut f, InputOrigin::XTest(XI_XTEST));
    assert_eq!(read_packets(&mut f.client_a), vec![xi(6, 4, 4, W)]);
    assert_eq!(read_packets(&mut f.client_b), vec![core(6, OUT)]);
    release(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert_eq!(read_packets(&mut f.client_a), vec![xi(5, 4, 4, W)]);
    assert_eq!(read_packets(&mut f.client_b), vec![core(5, OUT)]);
    final_motion(&mut f, InputOrigin::XTest(XI_XTEST));
    assert!(read_packets(&mut f.client_a).is_empty());
    assert_eq!(read_packets(&mut f.client_b), vec![core(6, OUT)]);
    assert_end_state(&f);
}

#[test]
fn i7_slave_grab_holds_until_last_slave_button() {
    let mut f = fixture();
    select_xi2(&mut f, CLIENT_A, W, XI_XTEST, XI_GRAB_MASK);
    select_xi2(&mut f, CLIENT_B, OUT, XI_XTEST, XI_GRAB_MASK);

    press_over_d(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert_slave_implicit(&f, XI_XTEST, CLIENT_A, W);
    assert!(f.state.active_pointer_grab.is_none());
    let mut a = read_packets(&mut f.client_a);
    assert_eq!(a, vec![xi(4, 4, 4, W)]);
    assert!(read_packets(&mut f.client_b).is_empty());
    move_to_out(&mut f, InputOrigin::XTest(XI_XTEST));
    a.extend(read_packets(&mut f.client_a));
    send(
        &mut f,
        InputOrigin::XTest(XI_XTEST),
        PointerEventKind::ButtonPress,
        3,
        650,
        150,
    );
    a.extend(read_packets(&mut f.client_a));
    release(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    a.extend(read_packets(&mut f.client_a));
    move_to_out(&mut f, InputOrigin::XTest(XI_XTEST));
    a.extend(read_packets(&mut f.client_a));
    release(&mut f, InputOrigin::XTest(XI_XTEST), 3);
    a.extend(read_packets(&mut f.client_a));
    assert!(read_packets(&mut f.client_b).is_empty());
    assert_eq!(
        a,
        vec![
            xi(4, 4, 4, W),
            xi(6, 4, 4, W),
            xi(4, 4, 4, W),
            xi(5, 4, 4, W),
            xi(6, 4, 4, W),
            xi(5, 4, 4, W),
        ]
    );
    assert!(f.state.xi2_pointer_grabs.is_empty());
    assert!(f.state.active_pointer_grab.is_none());
    final_motion(&mut f, InputOrigin::XTest(XI_XTEST));
    assert!(read_packets(&mut f.client_a).is_empty());
    assert_eq!(read_packets(&mut f.client_b), vec![xi(6, 4, 4, OUT)]);
    assert_end_state(&f);
}

#[test]
fn slave_and_master_masks_snapshot_per_device() {
    let mut f = fixture();
    select_xi2(&mut f, CLIENT_A, W, XI_XTEST, XI_BUTTON_PRESS | XI_MOTION);
    select_xi2(&mut f, CLIENT_A, W, XI_MASTER, XI_BUTTONS);
    select_xi2(
        &mut f,
        CLIENT_B,
        OUT,
        XI_XTEST,
        XI_MOTION | XI_BUTTON_RELEASE,
    );
    select_xi2(
        &mut f,
        CLIENT_B,
        OUT,
        XI_MASTER,
        XI_MOTION | XI_BUTTON_RELEASE,
    );

    press_over_d(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert_slave_implicit(&f, XI_XTEST, CLIENT_A, W);
    assert_eq!(
        f.state.xi2_pointer_grabs[&XI_XTEST].xi2_mask,
        u64::from(XI_BUTTON_PRESS | XI_MOTION)
    );
    assert_master_implicit(&f, CLIENT_A, W);
    assert_eq!(
        f.state.active_pointer_grab.unwrap().xi2_mask,
        u64::from(XI_BUTTONS)
    );
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi(4, 4, 4, W), xi(4, 2, 4, W)]
    );
    assert!(read_packets(&mut f.client_b).is_empty());

    move_to_out(&mut f, InputOrigin::XTest(XI_XTEST));
    assert_eq!(read_packets(&mut f.client_a), vec![xi(6, 4, 4, W)]);
    assert!(read_packets(&mut f.client_b).is_empty());
    release(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert_eq!(read_packets(&mut f.client_a), vec![xi(5, 2, 4, W)]);
    assert!(read_packets(&mut f.client_b).is_empty());
    final_motion(&mut f, InputOrigin::XTest(XI_XTEST));
    assert!(read_packets(&mut f.client_a).is_empty());
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(6, 4, 4, OUT), xi(6, 2, 4, OUT)]
    );
    assert_end_state(&f);
}

#[test]
fn two_slaves_end_their_grabs_separately() {
    let mut f = fixture();
    let source_id = crate::xinput::InputSourceId(0xABCD);
    let physical_device = pointer_source(&mut f.state, source_id.0, true);
    assert_ne!(physical_device, XI_XTEST);
    select_xi2(&mut f, CLIENT_A, W, XI_ALL_DEVICES, XI_GRAB_MASK);

    press_over_d(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert_slave_implicit(&f, XI_XTEST, CLIENT_A, W);
    assert_master_implicit(&f, CLIENT_A, W);
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi(4, 4, 4, W), xi(4, 2, 4, W)]
    );

    send(
        &mut f,
        InputOrigin::Physical(source_id),
        PointerEventKind::ButtonPress,
        1,
        150,
        150,
    );
    assert_slave_implicit(&f, physical_device, CLIENT_A, W);
    assert!(f.state.xi2_pointer_grabs.contains_key(&XI_XTEST));
    assert_master_implicit(&f, CLIENT_A, W);
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi(4, physical_device, physical_device, W)]
    );

    release(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert!(!f.state.xi2_pointer_grabs.contains_key(&XI_XTEST));
    assert!(f.state.xi2_pointer_grabs.contains_key(&physical_device));
    assert_master_implicit(&f, CLIENT_A, W);
    assert_eq!(read_packets(&mut f.client_a), vec![xi(5, 4, 4, W)]);

    release(&mut f, InputOrigin::Physical(source_id), 1);
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![
            xi(5, physical_device, physical_device, W),
            xi(5, 2, physical_device, W),
        ]
    );
    assert_end_state(&f);
}

fn core_passive_grab_and_slave_implicit_grab_release(keyboard_mode: u8) {
    let mut f = fixture();
    select_xi2(
        &mut f,
        CLIENT_A,
        W,
        XI_XTEST,
        XI_BUTTON_PRESS | XI_MOTION | XI_BUTTON_RELEASE,
    );
    select_core(&mut f, CLIENT_B, OUT, CORE_POINTER);
    grab_button(&mut f, CLIENT_B, W, keyboard_mode);

    press_over_d(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert_slave_implicit(&f, XI_XTEST, CLIENT_A, W);
    assert!(f.state.active_pointer_grab.is_some_and(|grab| {
        grab.owner == ClientId(CLIENT_B)
            && grab.grab_window == ResourceId(W)
            && grab.passive
            && !grab.implicit
    }));
    assert_eq!(read_packets(&mut f.client_a), vec![xi(4, 4, 4, W)]);
    assert_eq!(read_packets(&mut f.client_b), vec![core(4, W)]);

    let master_keyboard = crate::core_loop::pointer_fanout::xi1_other_input_device(
        &f.state,
        crate::xinput::DEVICEID_MASTER_POINTER,
    )
    .expect("the master pointer has a paired keyboard");
    let pointer_freeze = f
        .state
        .xi1_frozen
        .get(&crate::xinput::DEVICEID_MASTER_POINTER)
        .expect("the core passive grab records pointer sync state");
    let keyboard_freeze = f
        .state
        .xi1_frozen
        .get(&master_keyboard)
        .expect("the core passive grab records paired keyboard state");
    if keyboard_mode == 0 {
        assert_eq!(keyboard_freeze.other, Some(ClientId(CLIENT_B)));
    } else {
        assert_eq!(keyboard_freeze.other, None);
    }
    assert_eq!(pointer_freeze.state, crate::server::Xi1SyncState::Thawed);

    move_to_out(&mut f, InputOrigin::XTest(XI_XTEST));
    assert_eq!(read_packets(&mut f.client_a), vec![xi(6, 4, 4, W)]);
    assert!(read_packets(&mut f.client_b).is_empty());
    release(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert_eq!(read_packets(&mut f.client_a), vec![xi(5, 4, 4, W)]);
    assert_eq!(read_packets(&mut f.client_b), vec![core(5, W)]);
    assert!(f.state.xi2_pointer_grabs.is_empty());
    assert!(f.state.xi2_detached_masters.is_empty());
    assert_eq!(
        f.state.xi1_frozen[&master_keyboard].other, None,
        "the passive grab releases its paired keyboard hold"
    );
    assert_eq!(
        f.state.xi1_frozen[&master_keyboard].state,
        crate::server::Xi1SyncState::Thawed
    );
    assert_eq!(
        f.state.xi1_frozen[&crate::xinput::DEVICEID_MASTER_POINTER].state,
        crate::server::Xi1SyncState::Thawed
    );
    assert!(f.state.active_pointer_grab.is_none());

    final_motion(&mut f, InputOrigin::XTest(XI_XTEST));
    assert!(read_packets(&mut f.client_a).is_empty());
    assert_eq!(read_packets(&mut f.client_b), vec![core(6, OUT)]);
}

#[test]
fn slave_release_also_ends_async_core_passive_grab() {
    core_passive_grab_and_slave_implicit_grab_release(1);
}

#[test]
fn slave_release_clears_sync_keyboard_hold_of_core_passive_grab() {
    core_passive_grab_and_slave_implicit_grab_release(0);
}

#[test]
fn i8_owner_ungrab_ends_slave_implicit_grab() {
    let mut f = fixture();
    select_xi2(&mut f, CLIENT_A, W, XI_XTEST, XI_GRAB_MASK);
    select_xi2(&mut f, CLIENT_B, OUT, XI_XTEST, XI_GRAB_MASK);

    let origin = InputOrigin::XTest(XI_XTEST);
    press_over_d(&mut f, origin, 1);
    assert_slave_implicit(&f, XI_XTEST, CLIENT_A, W);
    assert!(f.state.active_pointer_grab.is_none());
    assert!(f.state.xi2_detached_masters.is_empty());
    assert_eq!(read_packets(&mut f.client_a), vec![xi(4, 4, 4, W)]);
    assert!(read_packets(&mut f.client_b).is_empty());

    move_to_out(&mut f, origin);
    assert_eq!(read_packets(&mut f.client_a), vec![xi(6, 4, 4, W)]);
    assert!(read_packets(&mut f.client_b).is_empty());
    ungrab_xi2_device(&mut f, CLIENT_A, XI_XTEST);
    assert!(!f.state.xi2_pointer_grabs.contains_key(&XI_XTEST));
    assert!(f.state.active_pointer_grab.is_none());

    release(&mut f, origin, 1);
    assert!(read_packets(&mut f.client_a).is_empty());
    assert_eq!(read_packets(&mut f.client_b), vec![xi(5, 4, 4, OUT)]);
    final_motion(&mut f, origin);
    assert!(read_packets(&mut f.client_a).is_empty());
    assert_eq!(read_packets(&mut f.client_b), vec![xi(6, 4, 4, OUT)]);
    assert_end_state(&f);
}

#[test]
fn foreign_ungrab_leaves_slave_implicit_grab() {
    let mut f = fixture();
    select_xi2(&mut f, CLIENT_A, W, XI_XTEST, XI_GRAB_MASK);
    select_xi2(&mut f, CLIENT_B, OUT, XI_XTEST, XI_GRAB_MASK);

    let origin = InputOrigin::XTest(XI_XTEST);
    press_over_d(&mut f, origin, 1);
    assert_slave_implicit(&f, XI_XTEST, CLIENT_A, W);
    assert!(f.state.active_pointer_grab.is_none());
    assert_eq!(read_packets(&mut f.client_a), vec![xi(4, 4, 4, W)]);
    assert!(read_packets(&mut f.client_b).is_empty());

    move_to_out(&mut f, origin);
    assert_eq!(read_packets(&mut f.client_a), vec![xi(6, 4, 4, W)]);
    assert!(read_packets(&mut f.client_b).is_empty());
    ungrab_xi2_device(&mut f, CLIENT_B, XI_XTEST);
    assert_slave_implicit(&f, XI_XTEST, CLIENT_A, W);

    release(&mut f, origin, 1);
    assert_eq!(read_packets(&mut f.client_a), vec![xi(5, 4, 4, W)]);
    assert!(read_packets(&mut f.client_b).is_empty());
    final_motion(&mut f, origin);
    assert!(read_packets(&mut f.client_a).is_empty());
    assert_eq!(read_packets(&mut f.client_b), vec![xi(6, 4, 4, OUT)]);
    assert_end_state(&f);
}

#[test]
fn slave_implicit_removal_keeps_master_grab_state() {
    let cases = [
        (
            "last slave release",
            check_slave_implicit_removal_subcase(SlaveGrabEnd::Release),
        ),
        (
            "owner XIUngrabDevice",
            check_slave_implicit_removal_subcase(SlaveGrabEnd::Ungrab),
        ),
        (
            "physical source cleanup then facet unregister",
            check_slave_implicit_removal_subcase(SlaveGrabEnd::PhysicalRemoval),
        ),
    ];
    let failures: Vec<_> = cases
        .iter()
        .filter(|(_, failures)| !failures.is_empty())
        .collect();
    assert!(
        failures.is_empty(),
        "master-grab preservation failures: {failures:?}"
    );
}

#[test]
fn same_client_holds_both_grabs_ungrab_slave_keeps_master() {
    let mut f = fixture();
    select_xi2(&mut f, CLIENT_A, W, XI_XTEST, XI_GRAB_MASK);
    select_xi2(&mut f, CLIENT_A, W, XI_MASTER, XI_GRAB_MASK);
    let origin = InputOrigin::XTest(XI_XTEST);

    press_over_d(&mut f, origin, 1);
    assert_slave_implicit(&f, XI_XTEST, CLIENT_A, W);
    assert_master_implicit(&f, CLIENT_A, W);
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi(4, 4, 4, W), xi(4, 2, 4, W)]
    );
    assert!(read_packets(&mut f.client_b).is_empty());

    let master_before = f.state.active_pointer_grab.expect("master implicit grab");
    move_to_out(&mut f, origin);
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi(6, 4, 4, W), xi(6, 2, 4, W)]
    );
    assert!(read_packets(&mut f.client_b).is_empty());
    ungrab_xi2_device(&mut f, CLIENT_A, XI_XTEST);
    assert!(!f.state.xi2_pointer_grabs.contains_key(&XI_XTEST));
    assert!(f.state.active_pointer_grab.is_some_and(|grab| {
        grab.owner == master_before.owner
            && grab.grab_window == master_before.grab_window
            && grab.implicit
    }));

    release(&mut f, origin, 1);
    assert_eq!(read_packets(&mut f.client_a), vec![xi(5, 2, 4, W)]);
    assert!(read_packets(&mut f.client_b).is_empty());
    final_motion(&mut f, origin);
    assert!(read_packets(&mut f.client_a).is_empty());
    assert_end_state(&f);
}

#[test]
fn owner_disconnect_keeps_other_clients_hold() {
    let mut f = fixture();
    let _ = grab_core_with_sync_keyboard(&mut f, CLIENT_B, OUT);
    let cursor_call_start = f.backend.grab_cursor_calls.len();
    select_xi2(&mut f, CLIENT_A, W, XI_XTEST, XI_GRAB_MASK);
    select_xi2(&mut f, CLIENT_B, OUT, XI_XTEST, XI_GRAB_MASK);
    let origin = InputOrigin::XTest(XI_XTEST);

    press_over_d(&mut f, origin, 1);
    assert_slave_implicit(&f, XI_XTEST, CLIENT_A, W);
    assert!(f.state.active_pointer_grab.is_some_and(|grab| {
        grab.owner == ClientId(CLIENT_B) && grab.grab_window == ResourceId(OUT) && !grab.implicit
    }));
    assert_eq!(
        f.state.xi1_frozen[&crate::xinput::DEVICEID_MASTER_KEYBOARD].other,
        Some(ClientId(CLIENT_B))
    );
    let detached_before = f.state.xi2_detached_masters.clone();
    assert_eq!(read_packets(&mut f.client_a), vec![xi(4, 4, 4, W)]);
    let _ = read_packets(&mut f.client_b);

    crate::core_loop::process_disconnect::process_disconnect(
        &mut f.state,
        &mut f.backend,
        ClientId(CLIENT_A),
    );
    assert!(!f.state.xi2_pointer_grabs.contains_key(&XI_XTEST));
    assert!(f.state.active_pointer_grab.is_some_and(|grab| {
        grab.owner == ClientId(CLIENT_B) && grab.grab_window == ResourceId(OUT) && !grab.implicit
    }));
    assert_eq!(
        f.state.xi1_frozen[&crate::xinput::DEVICEID_MASTER_KEYBOARD].other,
        Some(ClientId(CLIENT_B)),
        "disconnect must preserve B's paired keyboard hold"
    );
    assert!(!f.backend.grab_cursor_calls[cursor_call_start..].contains(&None));
    assert_eq!(f.state.xi2_detached_masters, detached_before);

    move_to_out(&mut f, origin);
    assert_eq!(read_packets(&mut f.client_b), vec![xi(6, 4, 4, OUT)]);
    release(&mut f, origin, 1);
    assert_eq!(read_packets(&mut f.client_b), vec![xi(5, 4, 4, OUT)]);
    ungrab_core_pointer(&mut f, CLIENT_B);
    assert!(f.state.xi2_pointer_grabs.is_empty());
    assert!(f.state.active_pointer_grab.is_none());
    assert!(f.state.xi2_detached_masters.is_empty());
    assert!(
        f.state.xi1_frozen.values().all(|freeze| {
            !freeze.frozen() && freeze.other.is_none() && freeze.stored.is_none()
        })
    );
}

#[test]
fn destroy_grab_window_ends_slave_implicit_grab() {
    let mut f = fixture();
    select_xi2(&mut f, CLIENT_A, W, XI_XTEST, XI_GRAB_MASK);
    select_xi2(&mut f, CLIENT_B, OUT, XI_XTEST, XI_GRAB_MASK);
    let origin = InputOrigin::XTest(XI_XTEST);

    press_over_d(&mut f, origin, 1);
    assert_slave_implicit(&f, XI_XTEST, CLIENT_A, W);
    assert!(f.state.active_pointer_grab.is_none());
    assert!(f.state.xi2_detached_masters.is_empty());
    assert_eq!(read_packets(&mut f.client_a), vec![xi(4, 4, 4, W)]);
    assert!(read_packets(&mut f.client_b).is_empty());

    dispatch(&mut f, CLIENT_A, 4, 0, &W.to_le_bytes()); // DestroyWindow
    assert!(f.state.resources.window(ResourceId(W)).is_none());
    assert!(!f.state.xi2_pointer_grabs.contains_key(&XI_XTEST));
    move_to_out(&mut f, origin);
    assert_eq!(read_packets(&mut f.client_b), vec![xi(6, 4, 4, OUT)]);
    release(&mut f, origin, 1);
    assert!(read_packets(&mut f.client_a).is_empty());
    assert_eq!(read_packets(&mut f.client_b), vec![xi(5, 4, 4, OUT)]);
    final_motion(&mut f, origin);
    assert_eq!(read_packets(&mut f.client_b), vec![xi(6, 4, 4, OUT)]);
    assert_end_state(&f);
}

#[test]
fn i6_unmap_grab_window_ends_slave_implicit_grab() {
    let mut f = fixture();
    select_xi2(&mut f, CLIENT_A, W, XI_XTEST, XI_GRAB_MASK);
    select_xi2(&mut f, CLIENT_B, OUT, XI_XTEST, XI_GRAB_MASK);
    let origin = InputOrigin::XTest(XI_XTEST);

    press_over_d(&mut f, origin, 1);
    assert_slave_implicit(&f, XI_XTEST, CLIENT_A, W);
    assert!(f.state.active_pointer_grab.is_none());
    assert!(f.state.xi2_detached_masters.is_empty());
    let mut packets_a = read_packets(&mut f.client_a);
    assert_eq!(packets_a, vec![xi(4, XI_XTEST, XI_XTEST, W)]);
    assert!(read_packets(&mut f.client_b).is_empty());

    move_to_out(&mut f, origin);
    let drag = read_packets(&mut f.client_a);
    assert_eq!(drag, vec![xi(6, XI_XTEST, XI_XTEST, W)]);
    packets_a.extend(drag);
    assert!(read_packets(&mut f.client_b).is_empty());

    dispatch(&mut f, CLIENT_A, CORE_UNMAP_WINDOW, 0, &W.to_le_bytes());
    assert_eq!(
        f.state.resources.window(ResourceId(W)).unwrap().map_state,
        crate::resources::MapState::Unmapped
    );
    assert!(
        !f.state.xi2_pointer_grabs.contains_key(&XI_XTEST),
        "unmapping the grab window ends its slave implicit grab"
    );

    final_motion(&mut f, origin);
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(6, XI_XTEST, XI_XTEST, OUT)]
    );
    assert!(read_packets(&mut f.client_a).is_empty());
    release(&mut f, origin, 1);
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(5, XI_XTEST, XI_XTEST, OUT)]
    );
    assert!(read_packets(&mut f.client_a).is_empty());
    assert!(
        packets_a
            .iter()
            .all(|packet| !packet.xi2 || packet.device != XI_MASTER),
        "client A receives no d2 events at any point"
    );
    assert_end_state(&f);
}

#[test]
fn unmap_subwindows_of_ancestor_ends_slave_implicit_grab() {
    let mut f = fixture_with_parent();
    select_xi2(&mut f, CLIENT_A, W, XI_XTEST, XI_GRAB_MASK);
    select_xi2(&mut f, CLIENT_B, OUT, XI_XTEST, XI_GRAB_MASK);
    let origin = InputOrigin::XTest(XI_XTEST);

    press_over_d(&mut f, origin, 1);
    assert_slave_implicit(&f, XI_XTEST, CLIENT_A, W);
    assert!(f.state.active_pointer_grab.is_none());
    assert!(f.state.xi2_detached_masters.is_empty());
    let mut packets_a = read_packets(&mut f.client_a);
    assert_eq!(packets_a, vec![xi(4, XI_XTEST, XI_XTEST, W)]);
    assert!(read_packets(&mut f.client_b).is_empty());

    move_to_out(&mut f, origin);
    let drag = read_packets(&mut f.client_a);
    assert_eq!(drag, vec![xi(6, XI_XTEST, XI_XTEST, W)]);
    packets_a.extend(drag);
    assert!(read_packets(&mut f.client_b).is_empty());

    dispatch(&mut f, CLIENT_A, CORE_UNMAP_SUBWINDOWS, 0, &P.to_le_bytes());
    assert_eq!(
        f.state.resources.window(ResourceId(W)).unwrap().map_state,
        crate::resources::MapState::Unmapped
    );
    assert!(
        !f.state.xi2_pointer_grabs.contains_key(&XI_XTEST),
        "unmapping an ancestor's subwindows ends the descendant grab"
    );

    final_motion(&mut f, origin);
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(6, XI_XTEST, XI_XTEST, OUT)]
    );
    assert!(read_packets(&mut f.client_a).is_empty());
    release(&mut f, origin, 1);
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(5, XI_XTEST, XI_XTEST, OUT)]
    );
    assert!(read_packets(&mut f.client_a).is_empty());
    assert!(
        packets_a
            .iter()
            .all(|packet| !packet.xi2 || packet.device != XI_MASTER),
        "client A receives no d2 events at any point"
    );
    assert_end_state(&f);
}

#[test]
fn unmap_ends_explicit_slave_grab_with_full_teardown() {
    let mut f = fixture();
    grab_xi2_device(&mut f, CLIENT_A, W, XI_XTEST, false, 0, 1, XI_GRAB_MASK);
    assert_eq!(
        grab_reply_status(&read_all_capture_available(&mut f.client_a)),
        0
    );
    let grab = f
        .state
        .xi2_pointer_grabs
        .get(&XI_XTEST)
        .expect("XIGrabDevice installs the explicit slave grab");
    assert_eq!(grab.owner, ClientId(CLIENT_A));
    assert_eq!(grab.grab_window, ResourceId(W));
    assert!(!grab.owner_events);
    assert!(!grab.implicit);
    assert!(f.state.xi2_detached_masters.contains_key(&XI_XTEST));
    assert_eq!(
        f.state.xi1_frozen[&XI_XTEST].state,
        crate::server::Xi1SyncState::FrozenNoEvent
    );

    dispatch(&mut f, CLIENT_A, CORE_UNMAP_WINDOW, 0, &W.to_le_bytes());
    assert!(
        !f.state.xi2_pointer_grabs.contains_key(&XI_XTEST),
        "unmapping the explicit grab window ends its slave grab"
    );
    assert!(f.state.active_pointer_grab.is_none());
    assert!(f.state.xi2_detached_masters.is_empty());
    assert_eq!(f.state.xi1_frozen.len(), 1);
    let freeze = &f.state.xi1_frozen[&XI_XTEST];
    assert_eq!(freeze.state, crate::server::Xi1SyncState::Thawed);
    assert_eq!(freeze.other, None);
    assert!(freeze.stored.is_none());
}

fn assert_unmap_replays_explicit_slave_input(f: &mut Fixture, unmap_subwindows: bool) {
    select_xi2(f, CLIENT_B, OUT, XI_XTEST, XI_GRAB_MASK);
    let original_master = f.state.xi_devices.device(XI_XTEST).unwrap().attached_master;
    assert!(original_master.is_some());

    grab_xi2_device(f, CLIENT_A, W, XI_XTEST, false, 0, 1, XI_GRAB_MASK);
    assert_eq!(
        grab_reply_status(&read_all_capture_available(&mut f.client_a)),
        0
    );
    assert!(f.state.xi2_detached_masters.contains_key(&XI_XTEST));
    assert_eq!(
        f.state.xi1_frozen[&XI_XTEST].state,
        crate::server::Xi1SyncState::FrozenNoEvent
    );

    let origin = InputOrigin::XTest(XI_XTEST);
    send(f, origin, PointerEventKind::ButtonPress, 1, 650, 150);
    send(f, origin, PointerEventKind::ButtonRelease, 1, 650, 150);
    assert_eq!(f.state.sync_pending.len(), 2);
    assert!(read_packets(&mut f.client_a).is_empty());
    assert!(read_packets(&mut f.client_b).is_empty());

    if unmap_subwindows {
        dispatch(f, CLIENT_A, CORE_UNMAP_SUBWINDOWS, 0, &P.to_le_bytes());
    } else {
        dispatch(f, CLIENT_A, CORE_UNMAP_WINDOW, 0, &W.to_le_bytes());
    }

    assert_eq!(
        read_packets(&mut f.client_b),
        vec![
            xi(4, XI_XTEST, XI_XTEST, OUT),
            xi(5, XI_XTEST, XI_XTEST, OUT),
        ],
        "unmapping releases and immediately delivers the queued slave events"
    );
    assert!(read_packets(&mut f.client_a).is_empty());
    assert!(f.state.sync_pending.is_empty());
    assert!(!f.state.xi2_pointer_grabs.contains_key(&XI_XTEST));
    assert!(f.state.active_pointer_grab.is_none());
    assert!(f.state.xi2_detached_masters.is_empty());
    assert_eq!(
        f.state.xi_devices.device(XI_XTEST).unwrap().attached_master,
        original_master,
        "unmapping reattaches device 4 before replaying input"
    );
    let freeze = &f.state.xi1_frozen[&XI_XTEST];
    assert_eq!(freeze.state, crate::server::Xi1SyncState::Thawed);
    assert_eq!(freeze.other, None);
    assert!(freeze.stored.is_none());
}

#[test]
fn unmap_explicit_slave_grab_replays_queued_input() {
    let mut f = fixture();
    assert_unmap_replays_explicit_slave_input(&mut f, false);
}

#[test]
fn unmap_subwindows_explicit_slave_grab_replays_queued_input() {
    let mut f = fixture_with_parent();
    assert_unmap_replays_explicit_slave_input(&mut f, true);
}

#[test]
fn i4_grab_statuses_during_slave_implicit_grab() {
    let mut f = fixture();
    select_xi2(&mut f, CLIENT_A, W, XI_XTEST, XI_GRAB_MASK);
    select_xi2(&mut f, CLIENT_B, OUT, XI_XTEST, XI_GRAB_MASK);
    let origin = InputOrigin::XTest(XI_XTEST);

    press_over_d(&mut f, origin, 1);
    assert_slave_implicit(&f, XI_XTEST, CLIENT_A, W);
    assert!(f.state.active_pointer_grab.is_none());
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi(4, XI_XTEST, XI_XTEST, W)]
    );
    assert!(read_packets(&mut f.client_b).is_empty());

    move_to_out(&mut f, origin);
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi(6, XI_XTEST, XI_XTEST, W)]
    );
    assert!(read_packets(&mut f.client_b).is_empty());

    grab_xi2_device(&mut f, CLIENT_B, OUT, XI_XTEST, false, 1, 1, XI_BUTTONS);
    assert_eq!(
        grab_reply_status(&read_all_capture_available(&mut f.client_b)),
        1,
        "another client cannot grab a slave held by an implicit grab"
    );
    assert_slave_implicit(&f, XI_XTEST, CLIENT_A, W);

    grab_xi2_device(&mut f, CLIENT_B, OUT, XI_MASTER, false, 1, 1, XI_BUTTONS);
    assert_eq!(
        grab_reply_status(&read_all_capture_available(&mut f.client_b)),
        0,
        "the master pointer remains independently grabbable"
    );
    assert!(f.state.active_pointer_grab.is_some_and(|grab| {
        grab.owner == ClientId(CLIENT_B) && grab.grab_window == ResourceId(OUT) && !grab.implicit
    }));

    release(&mut f, origin, 1);
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi(5, XI_XTEST, XI_XTEST, W)]
    );
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(5, XI_MASTER, XI_XTEST, OUT)]
    );
    ungrab_xi2_device(&mut f, CLIENT_B, XI_MASTER);
    assert_end_state_with_xi1_entries(
        &f,
        &[
            crate::xinput::DEVICEID_MASTER_POINTER,
            crate::xinput::DEVICEID_MASTER_KEYBOARD,
        ],
    );
}

#[test]
fn same_client_xigrabdevice_replaces_implicit_with_explicit() {
    let mut f = fixture();
    select_xi2(&mut f, CLIENT_A, W, XI_XTEST, XI_BUTTONS);
    let origin = InputOrigin::XTest(XI_XTEST);
    let original_master = f.state.xi_devices.device(XI_XTEST).unwrap().attached_master;
    assert!(original_master.is_some());

    press_over_d(&mut f, origin, 1);
    assert_slave_implicit(&f, XI_XTEST, CLIENT_A, W);
    assert!(f.state.active_pointer_grab.is_none());
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi(4, XI_XTEST, XI_XTEST, W)]
    );
    assert!(read_packets(&mut f.client_b).is_empty());

    grab_xi2_device(&mut f, CLIENT_A, W, XI_XTEST, false, 1, 1, XI_BUTTONS);
    assert_eq!(
        grab_reply_status(&read_all_capture_available(&mut f.client_a)),
        0,
        "the implicit grab owner can replace it with an explicit grab"
    );
    let grab = f.state.xi2_pointer_grabs.get(&XI_XTEST).unwrap();
    assert_eq!(grab.owner, ClientId(CLIENT_A));
    assert_eq!(grab.grab_window, ResourceId(W));
    assert!(!grab.implicit);
    assert!(f.state.xi2_detached_masters.contains_key(&XI_XTEST));
    assert_eq!(
        f.state.xi_devices.device(XI_XTEST).unwrap().attached_master,
        None,
        "the successful explicit slave grab leaves device 4 floating"
    );

    release(&mut f, origin, 1);
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi(5, XI_XTEST, XI_XTEST, W)]
    );
    assert!(f.state.xi2_pointer_grabs.contains_key(&XI_XTEST));
    assert!(f.state.xi2_detached_masters.contains_key(&XI_XTEST));
    assert!(f.state.active_pointer_grab.is_none());
    assert!(read_packets(&mut f.client_b).is_empty());

    ungrab_xi2_device(&mut f, CLIENT_A, XI_XTEST);
    assert_eq!(
        f.state.xi_devices.device(XI_XTEST).unwrap().attached_master,
        original_master,
        "XIUngrabDevice restores device 4's original master attachment"
    );
    assert_end_state_with_xi1_entries(&f, &[XI_XTEST]);
}

#[test]
fn xi1_grab_device_over_xi2_implicit_is_already_grabbed() {
    let mut f = fixture();
    select_xi2(&mut f, CLIENT_A, W, XI_XTEST, XI_BUTTONS);
    let origin = InputOrigin::XTest(XI_XTEST);

    press_over_d(&mut f, origin, 1);
    assert_slave_implicit(&f, XI_XTEST, CLIENT_A, W);
    assert!(f.state.active_pointer_grab.is_none());
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi(4, XI_XTEST, XI_XTEST, W)]
    );
    assert!(read_packets(&mut f.client_b).is_empty());

    assert_eq!(
        grab_xi1_device(&mut f, CLIENT_A, W, XI_XTEST as u8),
        1,
        "XI1 and XI2 are separate protocols, so the XI2 grab is occupied even by its owner"
    );
    assert!(f.state.xi1_active_grabs.is_empty());
    assert_slave_implicit(&f, XI_XTEST, CLIENT_A, W);

    release(&mut f, origin, 1);
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi(5, XI_XTEST, XI_XTEST, W)]
    );
    assert!(read_packets(&mut f.client_b).is_empty());
    assert_end_state(&f);
}

#[test]
fn i5_xi1_drag_and_release_stay_with_press_client() {
    let mut f = fixture();
    let origin = InputOrigin::XTest(XI_XTEST);
    let all = XI1_PRESS | XI1_RELEASE | XI1_MOTION;
    select_xi1(&mut f, CLIENT_A, W, XI_XTEST as u8, all);
    select_xi1(&mut f, CLIENT_B, OUT, XI_XTEST as u8, all);

    press_over_d(&mut f, origin, 1);
    assert_xi1_implicit(&f, CLIENT_A, W, 1, all, None);
    assert_eq!(read_packets(&mut f.client_a), vec![xi1(4, XI_XTEST, W)]);
    assert!(read_packets(&mut f.client_b).is_empty());

    move_to_out(&mut f, origin);
    release(&mut f, origin, 1);
    final_motion(&mut f, origin);
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi1(6, XI_XTEST, W), xi1(5, XI_XTEST, W)]
    );
    assert_eq!(read_packets(&mut f.client_b), vec![xi1(6, XI_XTEST, OUT)]);
    assert_xi1_end_state(&f);
}

#[test]
fn xi1_implicit_grab_mask_is_merged_snapshot() {
    let mut f = fixture();
    let origin = InputOrigin::XTest(XI_XTEST);
    let snapshot = XI1_PRESS | XI1_RELEASE | XI1_MOTION;
    select_xi1(&mut f, CLIENT_A, W, XI_XTEST as u8, XI1_PRESS | XI1_MOTION);
    select_xi1(&mut f, CLIENT_B, W, XI_XTEST as u8, XI1_RELEASE);
    select_xi1(&mut f, CLIENT_B, OUT, XI_XTEST as u8, XI1_RELEASE);

    press_over_d(&mut f, origin, 1);
    assert_xi1_implicit(&f, CLIENT_A, W, 1, snapshot, None);
    assert_eq!(read_packets(&mut f.client_a), vec![xi1(4, XI_XTEST, W)]);
    assert!(read_packets(&mut f.client_b).is_empty());

    select_xi1(&mut f, CLIENT_A, W, XI_XTEST as u8, XI1_PRESS);
    move_to_out(&mut f, origin);
    assert_eq!(read_packets(&mut f.client_a), vec![xi1(6, XI_XTEST, W)]);
    assert!(read_packets(&mut f.client_b).is_empty());
    release(&mut f, origin, 1);
    assert_eq!(read_packets(&mut f.client_a), vec![xi1(5, XI_XTEST, W)]);
    assert!(read_packets(&mut f.client_b).is_empty());
    assert_xi1_end_state(&f);
}

#[test]
fn xi1_unselected_release_still_ends_grab() {
    let mut f = fixture();
    let origin = InputOrigin::XTest(XI_XTEST);
    let mask = XI1_PRESS | XI1_MOTION;
    select_xi1(&mut f, CLIENT_A, W, XI_XTEST as u8, mask);

    press_over_d(&mut f, origin, 1);
    assert_xi1_implicit(&f, CLIENT_A, W, 1, mask, None);
    assert_eq!(read_packets(&mut f.client_a), vec![xi1(4, XI_XTEST, W)]);
    assert!(read_packets(&mut f.client_b).is_empty());

    move_to_out(&mut f, origin);
    assert_eq!(read_packets(&mut f.client_a), vec![xi1(6, XI_XTEST, W)]);
    release(&mut f, origin, 1);
    assert!(f.state.xi1_active_grabs.is_empty());
    assert!(read_packets(&mut f.client_a).is_empty());
    assert!(read_packets(&mut f.client_b).is_empty());
    assert_xi1_end_state(&f);
}

#[test]
fn xi1_grab_holds_until_last_button() {
    let mut f = fixture();
    let origin = InputOrigin::XTest(XI_XTEST);
    let all = XI1_PRESS | XI1_RELEASE | XI1_MOTION;
    select_xi1(&mut f, CLIENT_A, W, XI_XTEST as u8, all);
    select_xi1(&mut f, CLIENT_B, OUT, XI_XTEST as u8, all);

    press_over_d(&mut f, origin, 1);
    assert_xi1_implicit(&f, CLIENT_A, W, 1, all, None);
    send(&mut f, origin, PointerEventKind::ButtonPress, 3, 150, 150);
    assert_eq!(f.state.buttons_down, 0b101);
    assert!(f.state.xi1_active_grabs.contains_key(&XI_XTEST));
    move_to_out(&mut f, origin);
    release(&mut f, origin, 1);
    assert_eq!(f.state.buttons_down, 0b100);
    assert!(f.state.xi1_active_grabs.contains_key(&XI_XTEST));
    send(&mut f, origin, PointerEventKind::MotionNotify, 0, 655, 155);
    release(&mut f, origin, 3);
    assert!(f.state.xi1_active_grabs.is_empty());
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![
            xi1(4, XI_XTEST, W),
            xi1(4, XI_XTEST, W),
            xi1(6, XI_XTEST, W),
            xi1(5, XI_XTEST, W),
            xi1(6, XI_XTEST, W),
            xi1(5, XI_XTEST, W),
        ]
    );
    assert!(read_packets(&mut f.client_b).is_empty());
    assert_xi1_end_state(&f);
}

#[test]
fn xi2_slave_selection_on_deeper_window_beats_xi1() {
    let mut f = fixture();
    let origin = InputOrigin::XTest(XI_XTEST);
    let xi2_mask = XI_GRAB_MASK;
    select_xi2(&mut f, CLIENT_B, D, XI_XTEST, xi2_mask);
    select_xi1(
        &mut f,
        CLIENT_A,
        W,
        XI_XTEST as u8,
        XI1_PRESS | XI1_RELEASE | XI1_MOTION,
    );

    press_over_d(&mut f, origin, 1);
    assert_slave_implicit(&f, XI_XTEST, CLIENT_B, D);
    assert_eq!(f.state.buttons_down, 1);
    assert!(f.state.xi1_active_grabs.is_empty());
    assert!(f.state.active_pointer_grab.is_none());
    assert!(read_packets(&mut f.client_a).is_empty());
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(4, XI_XTEST, XI_XTEST, D)]
    );
    release(&mut f, origin, 1);
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(5, XI_XTEST, XI_XTEST, D)]
    );
    assert!(read_packets(&mut f.client_a).is_empty());
    assert_xi1_end_state(&f);
}

#[test]
fn xi1_on_deeper_window_beats_xi2_on_ancestor() {
    let mut f = fixture();
    let origin = InputOrigin::XTest(XI_XTEST);
    let all = XI1_PRESS | XI1_RELEASE | XI1_MOTION;
    select_xi2(&mut f, CLIENT_A, W, XI_XTEST, XI_GRAB_MASK);
    select_xi1(&mut f, CLIENT_B, D, XI_XTEST as u8, all);

    press_over_d(&mut f, origin, 1);
    assert_xi1_implicit(&f, CLIENT_B, D, 1, all, None);
    assert_eq!(read_packets(&mut f.client_b), vec![xi1(4, XI_XTEST, D)]);
    assert!(read_packets(&mut f.client_a).is_empty());
    move_to_out(&mut f, origin);
    release(&mut f, origin, 1);
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi1(6, XI_XTEST, D), xi1(5, XI_XTEST, D)]
    );
    assert!(read_packets(&mut f.client_a).is_empty());
    assert_xi1_end_state(&f);
}

#[test]
fn xi2_beats_xi1_on_same_window() {
    let mut f = fixture();
    let origin = InputOrigin::XTest(XI_XTEST);
    select_xi2(&mut f, CLIENT_B, D, XI_XTEST, XI_BUTTONS);
    select_xi1(&mut f, CLIENT_B, D, XI_XTEST as u8, XI1_PRESS | XI1_RELEASE);

    press_over_d(&mut f, origin, 1);
    assert_slave_implicit(&f, XI_XTEST, CLIENT_B, D);
    assert_eq!(f.state.buttons_down, 1);
    assert!(f.state.xi1_active_grabs.is_empty());
    assert!(f.state.active_pointer_grab.is_none());
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(4, XI_XTEST, XI_XTEST, D)]
    );
    assert!(read_packets(&mut f.client_a).is_empty());
    release(&mut f, origin, 1);
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(5, XI_XTEST, XI_XTEST, D)]
    );
    assert_xi1_end_state(&f);
}

#[test]
fn xi1_slave_grab_and_master_grab_coexist() {
    let mut f = fixture();
    let origin = InputOrigin::XTest(XI_XTEST);
    let all = XI1_PRESS | XI1_RELEASE | XI1_MOTION;
    select_xi1(&mut f, CLIENT_A, W, XI_XTEST as u8, all);
    select_core(&mut f, CLIENT_B, W, CORE_POINTER);

    press_over_d(&mut f, origin, 1);
    assert_xi1_implicit(&f, CLIENT_A, W, 1, all, Some((CLIENT_B, W)));
    assert!(f.state.active_pointer_grab.is_some_and(|grab| {
        grab.owner == ClientId(CLIENT_B) && grab.grab_window == ResourceId(W) && grab.implicit
    }));
    assert_eq!(read_packets(&mut f.client_a), vec![xi1(4, XI_XTEST, W)]);
    assert_eq!(read_packets(&mut f.client_b), vec![core(4, W)]);

    move_to_out(&mut f, origin);
    release(&mut f, origin, 1);
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi1(6, XI_XTEST, W), xi1(5, XI_XTEST, W)]
    );
    assert_eq!(read_packets(&mut f.client_b), vec![core(6, W), core(5, W)]);
    assert_xi1_end_state(&f);
}

#[test]
fn xi_grab_device_refused_during_xi1_implicit_grab() {
    let mut f = fixture();
    let origin = InputOrigin::XTest(XI_XTEST);
    let all = XI1_PRESS | XI1_RELEASE | XI1_MOTION;
    select_xi1(&mut f, CLIENT_A, W, XI_XTEST as u8, all);
    press_over_d(&mut f, origin, 1);
    assert_xi1_implicit(&f, CLIENT_A, W, 1, all, None);
    let _ = read_packets(&mut f.client_a);
    let _ = read_packets(&mut f.client_b);

    grab_xi2_device(&mut f, CLIENT_B, W, XI_XTEST, false, 1, 1, XI_BUTTONS);
    assert_eq!(
        grab_reply_status(&read_all_capture_available(&mut f.client_b)),
        1
    );
    grab_xi2_device(&mut f, CLIENT_A, W, XI_XTEST, false, 1, 1, XI_BUTTONS);
    assert_eq!(
        grab_reply_status(&read_all_capture_available(&mut f.client_a)),
        1
    );
    assert!(f.state.xi2_pointer_grabs.is_empty());
    assert!(f.state.xi1_active_grabs.contains_key(&XI_XTEST));

    release(&mut f, origin, 1);
    assert_eq!(read_packets(&mut f.client_a), vec![xi1(5, XI_XTEST, W)]);
    assert!(read_packets(&mut f.client_b).is_empty());
    assert_xi1_end_state(&f);
}

#[test]
fn xi1_implicit_grab_ends_on_unmap() {
    let mut f = fixture();
    let origin = InputOrigin::XTest(XI_XTEST);
    let all = XI1_PRESS | XI1_RELEASE | XI1_MOTION;
    select_xi1(&mut f, CLIENT_A, W, XI_XTEST as u8, all);
    select_xi1(&mut f, CLIENT_B, OUT, XI_XTEST as u8, all);

    press_over_d(&mut f, origin, 1);
    assert_xi1_implicit(&f, CLIENT_A, W, 1, all, None);
    let _ = read_packets(&mut f.client_a);
    move_to_out(&mut f, origin);
    assert_eq!(read_packets(&mut f.client_a), vec![xi1(6, XI_XTEST, W)]);
    dispatch(&mut f, CLIENT_A, CORE_UNMAP_WINDOW, 0, &W.to_le_bytes());
    assert!(!f.state.xi1_active_grabs.contains_key(&XI_XTEST));

    release(&mut f, origin, 1);
    assert!(read_packets(&mut f.client_a).is_empty());
    assert_eq!(read_packets(&mut f.client_b), vec![xi1(5, XI_XTEST, OUT)]);
    assert_xi1_end_state(&f);
}

#[test]
fn xi1_implicit_release_keeps_master_grab_keyboard_hold() {
    let mut f = fixture();
    let origin = InputOrigin::XTest(XI_XTEST);
    let all = XI1_PRESS | XI1_RELEASE;
    select_xi1(&mut f, CLIENT_A, W, XI_XTEST as u8, all);
    let (master_grab, _) = grab_core_with_sync_keyboard(&mut f, CLIENT_A, OUT);

    press_over_d(&mut f, origin, 1);
    let xi1_grab = f.state.xi1_active_grabs.get(&XI_XTEST).unwrap();
    assert_eq!(xi1_grab.owner, ClientId(CLIENT_A));
    assert_eq!(xi1_grab.grab_window, ResourceId(W));
    assert!(xi1_grab.implicit);
    assert!(f.state.active_pointer_grab.is_some_and(|grab| {
        grab.owner == master_grab.owner
            && grab.grab_window == master_grab.grab_window
            && !grab.implicit
    }));
    assert_eq!(f.state.buttons_down, 1);
    assert_eq!(read_packets(&mut f.client_a), vec![xi1(4, XI_XTEST, W)]);

    release(&mut f, origin, 1);
    assert_eq!(read_packets(&mut f.client_a), vec![xi1(5, XI_XTEST, W)]);
    assert!(!f.state.xi1_active_grabs.contains_key(&XI_XTEST));
    assert!(f.state.active_pointer_grab.is_some_and(|grab| {
        grab.owner == master_grab.owner
            && grab.grab_window == master_grab.grab_window
            && !grab.implicit
    }));
    assert_eq!(
        f.state.xi1_frozen[&crate::xinput::DEVICEID_MASTER_KEYBOARD].other,
        Some(ClientId(CLIENT_A)),
        "XI1 slave-grab teardown must preserve the independent core grab's keyboard hold"
    );

    ungrab_core_pointer(&mut f, CLIENT_A);
    assert_eq!(
        f.state.xi1_frozen[&crate::xinput::DEVICEID_MASTER_KEYBOARD].other,
        None,
        "the master grab releases its own keyboard hold"
    );
    assert_buttons_released(&f);
    assert_end_state_with_xi1_entries(
        &f,
        &[
            crate::xinput::DEVICEID_MASTER_POINTER,
            crate::xinput::DEVICEID_MASTER_KEYBOARD,
        ],
    );
}

#[test]
fn xi1_implicit_unmap_keeps_master_grab_keyboard_hold() {
    let mut f = fixture();
    let origin = InputOrigin::XTest(XI_XTEST);
    let all = XI1_PRESS | XI1_RELEASE;
    select_xi1(&mut f, CLIENT_A, W, XI_XTEST as u8, all);
    let (master_grab, _) = grab_core_with_sync_keyboard(&mut f, CLIENT_A, OUT);

    press_over_d(&mut f, origin, 1);
    assert!(f.state.active_pointer_grab.is_some_and(|grab| {
        grab.owner == master_grab.owner
            && grab.grab_window == master_grab.grab_window
            && !grab.implicit
    }));
    assert!(f.state.xi1_active_grabs[&XI_XTEST].implicit);
    assert_eq!(f.state.buttons_down, 1);
    let _ = read_packets(&mut f.client_a);

    dispatch(&mut f, CLIENT_A, CORE_UNMAP_WINDOW, 0, &W.to_le_bytes());
    assert!(!f.state.xi1_active_grabs.contains_key(&XI_XTEST));
    assert!(f.state.active_pointer_grab.is_some_and(|grab| {
        grab.owner == master_grab.owner
            && grab.grab_window == master_grab.grab_window
            && !grab.implicit
    }));
    assert_eq!(
        f.state.xi1_frozen[&crate::xinput::DEVICEID_MASTER_KEYBOARD].other,
        Some(ClientId(CLIENT_A)),
        "unmapping the XI1 grab window must preserve the independent core grab's keyboard hold"
    );

    release(&mut f, origin, 1);
    ungrab_core_pointer(&mut f, CLIENT_A);
    assert_eq!(
        f.state.xi1_frozen[&crate::xinput::DEVICEID_MASTER_KEYBOARD].other,
        None
    );
    assert_buttons_released(&f);
    assert_end_state_with_xi1_entries(
        &f,
        &[
            crate::xinput::DEVICEID_MASTER_POINTER,
            crate::xinput::DEVICEID_MASTER_KEYBOARD,
        ],
    );
}

#[test]
fn xi1_implicit_grab_owner_disconnect_frees_device() {
    let mut f = fixture();
    let origin = InputOrigin::XTest(XI_XTEST);
    let all = XI1_PRESS | XI1_RELEASE | XI1_MOTION;
    select_xi1(&mut f, CLIENT_A, W, XI_XTEST as u8, all);
    select_xi1(&mut f, CLIENT_B, OUT, XI_XTEST as u8, all);

    press_over_d(&mut f, origin, 1);
    assert_xi1_implicit(&f, CLIENT_A, W, 1, all, None);
    let _ = read_packets(&mut f.client_a);
    move_to_out(&mut f, origin);
    assert_eq!(read_packets(&mut f.client_a), vec![xi1(6, XI_XTEST, W)]);
    crate::core_loop::process_disconnect::process_disconnect(
        &mut f.state,
        &mut f.backend,
        ClientId(CLIENT_A),
    );
    assert!(!f.state.xi1_active_grabs.contains_key(&XI_XTEST));

    release(&mut f, origin, 1);
    assert_eq!(read_packets(&mut f.client_b), vec![xi1(5, XI_XTEST, OUT)]);
    assert_xi1_end_state(&f);
}
