//! Where a passive button grab's activating press is reported (GH #277).

use super::{
    implicit_grabs::{
        Fixture, assert_end_state, assert_end_state_with_xi1_entries, core, create_window,
        dispatch, map_window, press_over_d, read_packets, release, select_xi2, send, xi,
    },
    *,
};
use crate::{
    backend::recording::RecordingBackend, core_loop::InputOrigin, host_x11::HostXidMap,
    server::ServerState,
};
use yserver_protocol::x11::{ClientId, ResourceId};

const CLIENT_A: u32 = 0xA753;
const CLIENT_B: u32 = 0xA754;
const W: u32 = 0x0010_A753;
const D: u32 = 0x0020_A753;
const OUT: u32 = 0x0010_A754;
const HOST_W: u32 = 0xCAFE_A753;
const HOST_OUT: u32 = 0xCAFE_A754;
const XI2_OPCODE: u8 = 137;
const XI_XI_ALLOW_EVENTS: u8 = 53;
const XI_PASSIVE_GRAB_DEVICE: u8 = 54;
#[allow(dead_code)] // shared request helper for the following passive-grab task
const XI_PASSIVE_UNGRAB_DEVICE: u8 = 55;
const RECORD_CREATE_CONTEXT: u8 = yserver_protocol::x11::record::CREATE_CONTEXT;
const RECORD_ENABLE_CONTEXT: u8 = yserver_protocol::x11::record::ENABLE_CONTEXT;
const RECORD_CLIENT: u32 = 0xA755;
const RECORD_CONTEXT: u32 = 0x0300_0001;
const XI_MASTER: u16 = 2;
const XI_MASTER_KEYBOARD: u16 = 3;
const XI_XTEST: u16 = 4;
const XI_ALL_DEVICES: u16 = 0;
const XI_MOTION: u32 = 1 << 6;
const XI_BUTTON_PRESS: u32 = 1 << 4;
const XI_BUTTON_RELEASE: u32 = 1 << 5;
const XI_BUTTONS: u32 = XI_BUTTON_PRESS | XI_BUTTON_RELEASE;
const XI_GRAB_MASK: u32 = XI_MOTION | XI_BUTTONS;
const ANY_BUTTON: u8 = 0;
const ANY_MODIFIER: u16 = 0x8000;
const BUTTON_1: u16 = 1;
const BUTTON_3: u16 = 1 << 2;

/// Uses the shared request-dispatched windows and capture clients, with D
/// owned by B as in the passive-grab measurements.
fn p_fixture() -> Fixture {
    let mut state = ServerState::new();
    let client_a = super::install_capture_client(&mut state, CLIENT_A);
    let client_b = super::install_capture_client(&mut state, CLIENT_B);
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
    create_window(&mut f, CLIENT_B, D, W, 20, 20, 100, 100);
    map_window(&mut f, CLIENT_B, D);
    create_window(&mut f, CLIENT_B, OUT, 0, 550, 100, 300, 300);
    map_window(&mut f, CLIENT_B, OUT);
    f.xid_map.insert(HOST_W, ResourceId(W));
    f.xid_map.insert(HOST_OUT, ResourceId(OUT));
    f
}

fn grab_button_core(
    f: &mut Fixture,
    client: u32,
    window: u32,
    button: u8,
    modifiers: u16,
    pointer_mode: u8,
    owner_events: bool,
) {
    let mut body = Vec::with_capacity(20);
    body.extend_from_slice(&window.to_le_bytes());
    body.extend_from_slice(&0x000cu16.to_le_bytes()); // ButtonPress|ButtonRelease
    body.extend_from_slice(&[pointer_mode, 1]); // pointer mode, async keyboard mode
    body.extend_from_slice(&0u32.to_le_bytes()); // no confinement
    body.extend_from_slice(&0u32.to_le_bytes()); // no cursor
    body.extend_from_slice(&[button, 0]);
    body.extend_from_slice(&modifiers.to_le_bytes());
    dispatch(f, client, 28, u8::from(owner_events), &body);
    assert!(f.state.button_grabs.iter().any(|grab| {
        grab.owner == ClientId(client)
            && grab.grab_window == ResourceId(window)
            && grab.button == button
            && grab.modifiers == modifiers
            && !grab.via_xi2
    }));
}

#[allow(dead_code)] // shared request helper for the following passive-grab task
pub(super) fn ungrab_button_core(
    f: &mut Fixture,
    client: u32,
    window: u32,
    button: u8,
    modifiers: u16,
) {
    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&window.to_le_bytes());
    body.extend_from_slice(&modifiers.to_le_bytes());
    body.extend_from_slice(&[0; 2]); // request padding
    dispatch(f, client, 29, button, &body);
}

fn xi_grab_button(
    f: &mut Fixture,
    client: u32,
    device: u16,
    window: u32,
    button: u8,
    mode: u8,
    owner_events: bool,
    mask: u32,
) {
    let mut body = Vec::with_capacity(36);
    body.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    body.extend_from_slice(&window.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // no cursor
    body.extend_from_slice(&u32::from(button).to_le_bytes());
    body.extend_from_slice(&device.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes()); // one modifier
    body.extend_from_slice(&1u16.to_le_bytes()); // one mask word
    body.extend_from_slice(&[0, mode, 1, u8::from(owner_events)]); // Button, mode, async paired, owner_events
    body.extend_from_slice(&0u16.to_le_bytes()); // pad
    body.extend_from_slice(&mask.to_le_bytes());
    body.extend_from_slice(&0x8000_0000u32.to_le_bytes()); // XIAnyModifier
    dispatch(f, client, XI2_OPCODE, XI_PASSIVE_GRAB_DEVICE, &body);
    assert!(f.state.button_grabs.iter().any(|grab| {
        grab.owner == ClientId(client)
            && grab.grab_window == ResourceId(window)
            && grab.device_id == device
            && grab.button == button
            && grab.modifiers == ANY_MODIFIER
            && grab.via_xi2
    }));
}

#[allow(dead_code)] // shared request helper for the following passive-grab task
pub(super) fn xi_ungrab_button(f: &mut Fixture, client: u32, device: u16, window: u32, button: u8) {
    let mut body = Vec::with_capacity(20);
    body.extend_from_slice(&window.to_le_bytes());
    body.extend_from_slice(&u32::from(button).to_le_bytes());
    body.extend_from_slice(&device.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes()); // one modifier
    body.push(0); // Button grab type
    body.extend_from_slice(&[0; 3]); // pad
    body.extend_from_slice(&0x8000_0000u32.to_le_bytes()); // XIAnyModifier
    dispatch(f, client, XI2_OPCODE, XI_PASSIVE_UNGRAB_DEVICE, &body);
}

fn allow_events_core(f: &mut Fixture, client: u32, mode: u8) {
    dispatch(f, client, 35, mode, &0u32.to_le_bytes()); // CurrentTime
}

fn xi_allow_events(f: &mut Fixture, client: u32, device: u16, mode: u8) {
    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    body.extend_from_slice(&device.to_le_bytes());
    body.extend_from_slice(&[mode, 0]);
    dispatch(f, client, XI2_OPCODE, XI_XI_ALLOW_EVENTS, &body);
}

fn enable_record_button_events(f: &mut Fixture, peer: &mut crate::transport::CapturedPeer) {
    use yserver_protocol::x11::{ClientByteOrder, record};

    let mut body = Vec::with_capacity(record::REGISTER_FIXED_BODY + 4 + record::RANGE_SIZE);
    body.extend_from_slice(&RECORD_CONTEXT.to_le_bytes());
    body.extend_from_slice(&[0; 4]); // no element header
    body.extend_from_slice(&1u32.to_le_bytes()); // one client spec
    body.extend_from_slice(&1u32.to_le_bytes()); // one range
    body.extend_from_slice(&record::ALL_CLIENTS.to_le_bytes());
    record::Range {
        device_events: (4, 5), // ButtonPress through ButtonRelease
        ..record::Range::default()
    }
    .encode(ClientByteOrder::LittleEndian, &mut body);
    dispatch(
        f,
        RECORD_CLIENT,
        crate::nested::RECORD_MAJOR_OPCODE,
        RECORD_CREATE_CONTEXT,
        &body,
    );
    dispatch(
        f,
        RECORD_CLIENT,
        crate::nested::RECORD_MAJOR_OPCODE,
        RECORD_ENABLE_CONTEXT,
        &RECORD_CONTEXT.to_le_bytes(),
    );
    let start_marker = read_all_capture_available(peer);
    assert_eq!(
        start_marker.len(),
        32,
        "EnableContext sends its start marker"
    );
    assert_eq!(start_marker[0], 1, "start marker is an X reply");
}

/// Adds Task 1's held-button invariant to the shared grab/freeze end check.
fn assert_task1_end_state(f: &Fixture, xi1_entries: &[u16]) {
    if xi1_entries.is_empty() {
        assert_end_state(f);
    } else {
        assert_end_state_with_xi1_entries(f, xi1_entries);
    }
    assert_eq!(f.state.buttons_down, 0, "master has no held buttons");
    for device in f.state.xi_devices.devices().iter().filter(|device| {
        device.id == XI_XTEST || device.facet == Some(crate::xinput::XiFacetKind::PointerTouch)
    }) {
        assert_eq!(
            device.buttons_down, 0,
            "pointer device {} has no held buttons",
            device.id
        );
    }
    assert!(
        f.state.unpublished_pointer_buttons_down.is_empty(),
        "unpublished pointer sources have no held buttons"
    );
}

/// GH #277 (probe V9, measured on Xorg in
/// `tools/vng-scenarios/key-grab-activation`): Xorg `ActivatePassiveGrab`
/// (dix/events.c) delivers the activating press with
/// `FixUpEventFromWindow(grab->window)` + `TryClientEvents` — no
/// `owner_events` walk. The grab client selects ButtonPress on the root
/// and on `bar` but not on the grab window `top`; the press is still
/// reported on `top`, child `bar`. The release, under the active grab,
/// takes the owner_events walk to `bar`.
#[test]
fn passive_grab_activating_press_skips_owner_events_walk() {
    use yserver_protocol::x11::ResourceId;

    let mut state = ServerState::new();
    let top = ResourceId(0x0020_0001);
    let bar = ResourceId(0x0020_0002);
    let mut peer = install_client(&mut state, 1);
    for (win, parent, x, y, w, h) in [
        (top, crate::resources::ROOT_WINDOW, 100, 100, 400, 300),
        (bar, top, 0, 250, 400, 50),
    ] {
        state.resources.create_window(
            ClientId(1),
            yserver_protocol::x11::CreateWindowRequest {
                depth: 24,
                window: win,
                parent,
                x,
                y,
                width: w,
                height: h,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
        let _ = state.resources.map_window(win);
    }
    let masks = &mut state.clients.get_mut(&1).unwrap().event_masks;
    masks.insert(crate::resources::ROOT_WINDOW, 0x0000_000C);
    masks.insert(bar, 0x0000_000C);
    state.button_grabs.push(crate::server::PassiveButtonGrab {
        device_id: 0,
        owner: ClientId(1),
        grab_window: top,
        button: 1,
        modifiers: 0x8000,
        owner_events: true,
        event_mask: 0x0000_000C,
        pointer_mode: 1,
        keyboard_mode: 1,
        confine_to: ResourceId(0),
        via_xi2: false,
    });
    let mut xid_map = HostXidMap::new();
    xid_map.insert(0xCAFE_u32, crate::resources::ROOT_WINDOW);
    let mut backend = crate::backend::recording::RecordingBackend::default();
    let mut press = HostPointerEvent {
        origin: crate::core_loop::message::InputOrigin::XTest(4),
        kind: PointerEventKind::ButtonPress,
        host_xid: 0xCAFE,
        detail: 1,
        time: 0,
        root_x: 300,
        root_y: 375,
        event_x: 300,
        event_y: 375,
        state: 0,
        crossing_mode: 0,
        child: 0,
        raw_dx: 0,
        raw_dy: 0,
        tree_change: false,
    };
    let fields = |bytes: &[u8], code: u8| -> Vec<(u32, u32, i16, i16)> {
        bytes
            .chunks(32)
            .filter(|e| e[0] & 0x7f == code)
            .map(|e| {
                let u32_at = |at: usize| u32::from_le_bytes(e[at..at + 4].try_into().unwrap());
                let i16_at = |at: usize| i16::from_le_bytes([e[at], e[at + 1]]);
                (u32_at(12), u32_at(16), i16_at(24), i16_at(26))
            })
            .collect()
    };
    let _ = pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, press, true, false);
    assert_eq!(
        fields(&read_all_available(&mut peer), 4),
        [(top.0, bar.0, 200, 275)],
        "activating press: event=top child=bar ev=200,275 (Xorg probe V9)",
    );
    press.kind = PointerEventKind::ButtonRelease;
    press.state = 0x100;
    let _ = pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, press, true, false);
    assert_eq!(
        fields(&read_all_available(&mut peer), 5),
        [(bar.0, 0, 200, 25)],
        "release under the grab walks to bar (Xorg probe V9)",
    );
}

#[test]
fn p1_async_slave_grab_owner_events_no_master_press() {
    let mut f = p_fixture();
    assert_eq!(f.state.buttons_down, 0);
    select_xi2(&mut f, CLIENT_B, D, XI_XTEST, XI_GRAB_MASK);
    select_xi2(&mut f, CLIENT_B, D, XI_MASTER, XI_GRAB_MASK);
    xi_grab_button(&mut f, CLIENT_A, XI_XTEST, W, 1, 1, true, XI_GRAB_MASK);

    press_over_d(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    let grab = f.state.xi2_pointer_grabs.get(&XI_XTEST).copied();
    assert!(grab.is_some_and(|grab| {
        grab.owner == ClientId(CLIENT_A)
            && grab.grab_window == ResourceId(W)
            && grab.via_xi2
            && grab.passive
            && !grab.implicit
    }));
    assert_eq!(
        f.state.xi2_detached_masters.get(&XI_XTEST),
        Some(&XI_MASTER)
    );
    let press_a = read_packets(&mut f.client_a);
    let press_b = read_packets(&mut f.client_b);
    assert_eq!(press_a, vec![xi(4, XI_XTEST, XI_XTEST, W)]);
    assert!(
        press_b.is_empty() && f.state.buttons_down == 0,
        "slave activation has no master press or transition: packets={press_b:?}, buttons_down=0x{:x}",
        f.state.buttons_down
    );

    send(
        &mut f,
        InputOrigin::XTest(XI_XTEST),
        PointerEventKind::MotionNotify,
        0,
        155,
        155,
    );
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi(6, XI_XTEST, XI_XTEST, W)]
    );
    assert!(read_packets(&mut f.client_b).is_empty());
    release(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi(5, XI_XTEST, XI_XTEST, W)]
    );
    assert!(read_packets(&mut f.client_b).is_empty());
    assert_task1_end_state(&f, &[XI_XTEST]);
}

#[test]
fn p1_slave_activation_records_no_master_press() {
    let mut f = p_fixture();
    assert_eq!(f.state.buttons_down, 0);
    let mut record_peer = super::install_capture_client(&mut f.state, RECORD_CLIENT);
    {
        let client = f.state.clients.get_mut(&RECORD_CLIENT).unwrap();
        client.resource_id_base = 0x0300_0000;
        client.resource_id_mask = 0x00ff_ffff;
    }
    enable_record_button_events(&mut f, &mut record_peer);

    select_xi2(&mut f, CLIENT_B, D, XI_XTEST, XI_BUTTONS);
    select_xi2(&mut f, CLIENT_B, D, XI_MASTER, XI_BUTTONS);
    xi_grab_button(&mut f, CLIENT_A, XI_XTEST, W, 1, 1, true, XI_GRAB_MASK);

    press_over_d(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert!(
        f.state
            .xi2_pointer_grabs
            .get(&XI_XTEST)
            .is_some_and(|grab| {
                grab.owner == ClientId(CLIENT_A)
                    && grab.grab_window == ResourceId(W)
                    && grab.via_xi2
                    && grab.passive
                    && !grab.implicit
            })
    );
    assert_eq!(
        f.state.xi2_detached_masters.get(&XI_XTEST),
        Some(&XI_MASTER)
    );
    assert_eq!(f.state.buttons_down, 0);
    release(&mut f, InputOrigin::XTest(XI_XTEST), 1);

    let recorded = read_all_capture_available(&mut record_peer);
    assert!(
        recorded.is_empty(),
        "RECORD must receive no core ButtonPress or ButtonRelease for a slave activation; got {recorded:02x?}"
    );
    assert_task1_end_state(&f, &[XI_XTEST]);
}

#[test]
fn p1m_master_xi2_grab_delivers_only_xi2() {
    let mut f = p_fixture();
    assert_eq!(f.state.buttons_down, 0);
    select_xi2(&mut f, CLIENT_B, D, XI_XTEST, XI_GRAB_MASK);
    select_xi2(&mut f, CLIENT_B, D, XI_MASTER, XI_GRAB_MASK);
    xi_grab_button(&mut f, CLIENT_A, XI_MASTER, W, 1, 1, true, XI_GRAB_MASK);

    press_over_d(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    let grab = f.state.active_pointer_grab;
    assert!(grab.is_some_and(|grab| {
        grab.owner == ClientId(CLIENT_A)
            && grab.grab_window == ResourceId(W)
            && grab.via_xi2
            && grab.passive
            && !grab.implicit
    }));
    let slave_grab = f.state.xi2_pointer_grabs.get(&XI_XTEST);
    assert!(slave_grab.is_some_and(|grab| {
        grab.owner == ClientId(CLIENT_B)
            && grab.grab_window == ResourceId(D)
            && grab.via_xi2
            && grab.implicit
            && !grab.passive
    }));
    assert_eq!(f.state.buttons_down, BUTTON_1);
    let press_a = read_packets(&mut f.client_a);
    assert_eq!(press_a, vec![xi(4, XI_MASTER, XI_XTEST, W)]);
    assert!(press_a[0].xi2, "the activation has no core ButtonPress");
    assert_eq!(press_a[0].child, D);
    assert_eq!((press_a[0].event_x, press_a[0].event_y), (50, 50));
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(4, XI_XTEST, XI_XTEST, D)]
    );

    send(
        &mut f,
        InputOrigin::XTest(XI_XTEST),
        PointerEventKind::MotionNotify,
        0,
        155,
        155,
    );
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi(6, XI_MASTER, XI_XTEST, W)]
    );
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(6, XI_XTEST, XI_XTEST, D)]
    );
    release(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi(5, XI_MASTER, XI_XTEST, W)]
    );
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(5, XI_XTEST, XI_XTEST, D)]
    );
    assert_task1_end_state(&f, &[XI_MASTER, XI_MASTER_KEYBOARD]);
}

#[test]
fn p2_sync_slave_grab_release_only_mask_gets_press() {
    let mut f = p_fixture();
    assert_eq!(f.state.buttons_down, 0);
    select_xi2(&mut f, CLIENT_B, D, XI_XTEST, XI_BUTTONS);
    select_xi2(&mut f, CLIENT_B, D, XI_MASTER, XI_BUTTONS);
    xi_grab_button(
        &mut f,
        CLIENT_A,
        XI_XTEST,
        W,
        1,
        0,
        false,
        XI_BUTTON_RELEASE,
    );

    press_over_d(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    let grab = f.state.xi2_pointer_grabs.get(&XI_XTEST).copied();
    assert!(grab.is_some_and(|grab| {
        grab.owner == ClientId(CLIENT_A)
            && grab.grab_window == ResourceId(W)
            && grab.via_xi2
            && grab.passive
            && !grab.implicit
    }));
    assert_eq!(
        f.state.xi2_detached_masters.get(&XI_XTEST),
        Some(&XI_MASTER)
    );
    assert_eq!(f.state.buttons_down, 0);
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi(4, XI_XTEST, XI_XTEST, W)]
    );
    assert!(read_packets(&mut f.client_b).is_empty());

    xi_allow_events(&mut f, CLIENT_A, XI_XTEST, 0); // AsyncDevice
    release(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi(5, XI_XTEST, XI_XTEST, W)]
    );
    assert!(read_packets(&mut f.client_b).is_empty());
    assert_task1_end_state(&f, &[XI_XTEST]);
}

#[test]
fn p2m_master_sync_release_only_mask_single_press() {
    let mut f = p_fixture();
    assert_eq!(f.state.buttons_down, 0);
    select_xi2(&mut f, CLIENT_B, D, XI_XTEST, XI_BUTTONS);
    select_xi2(&mut f, CLIENT_B, D, XI_MASTER, XI_BUTTONS);
    xi_grab_button(
        &mut f,
        CLIENT_A,
        XI_MASTER,
        W,
        1,
        0,
        false,
        XI_BUTTON_RELEASE,
    );

    press_over_d(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    let grab = f.state.active_pointer_grab;
    assert!(grab.is_some_and(|grab| {
        grab.owner == ClientId(CLIENT_A)
            && grab.grab_window == ResourceId(W)
            && grab.via_xi2
            && grab.passive
            && !grab.implicit
    }));
    let slave_grab = f.state.xi2_pointer_grabs.get(&XI_XTEST);
    assert!(slave_grab.is_some_and(|grab| {
        grab.owner == ClientId(CLIENT_B)
            && grab.grab_window == ResourceId(D)
            && grab.via_xi2
            && grab.implicit
            && !grab.passive
    }));
    assert_eq!(f.state.buttons_down, BUTTON_1);
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi(4, XI_MASTER, XI_XTEST, W)]
    );
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(4, XI_XTEST, XI_XTEST, D)]
    );

    xi_allow_events(&mut f, CLIENT_A, XI_MASTER, 0); // AsyncDevice
    release(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi(5, XI_MASTER, XI_XTEST, W)]
    );
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(5, XI_XTEST, XI_XTEST, D)]
    );
    assert_task1_end_state(&f, &[XI_MASTER, XI_MASTER_KEYBOARD]);
}

#[test]
fn p4_xfwm4_core_then_xi2_grab_single_xi2_press() {
    let mut f = p_fixture();
    assert_eq!(f.state.buttons_down, 0);
    select_xi2(&mut f, CLIENT_B, D, XI_MASTER, XI_BUTTONS);
    grab_button_core(&mut f, CLIENT_A, D, ANY_BUTTON, ANY_MODIFIER, 0, false);
    xi_grab_button(
        &mut f, CLIENT_A, XI_MASTER, D, ANY_BUTTON, 0, false, XI_BUTTONS,
    );

    press_over_d(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    let grab = f.state.active_pointer_grab;
    assert!(grab.is_some_and(|grab| {
        grab.owner == ClientId(CLIENT_A)
            && grab.grab_window == ResourceId(D)
            && grab.via_xi2
            && grab.passive
            && !grab.implicit
    }));
    assert!(f.state.xi2_pointer_grabs.is_empty());
    assert_eq!(f.state.buttons_down, BUTTON_1);
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi(4, XI_MASTER, XI_XTEST, D)]
    );
    assert!(read_packets(&mut f.client_b).is_empty());

    allow_events_core(&mut f, CLIENT_A, 2); // ReplayPointer
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(4, XI_MASTER, XI_XTEST, D)]
    );
    assert!(read_packets(&mut f.client_a).is_empty());
    release(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(5, XI_MASTER, XI_XTEST, D)]
    );
    assert!(read_packets(&mut f.client_a).is_empty());
    assert_task1_end_state(&f, &[XI_MASTER, XI_MASTER_KEYBOARD]);
}

#[test]
fn held_button_blocks_core_grab_on_master_pass() {
    let mut f = p_fixture();
    assert_eq!(f.state.buttons_down, 0);
    grab_button_core(&mut f, CLIENT_A, W, 1, ANY_MODIFIER, 1, false);
    let source = crate::xinput::InputSourceId(0xABCD);
    let _physical_device = super::pointer_source(&mut f.state, source.0, true);

    send(
        &mut f,
        InputOrigin::Physical(source),
        PointerEventKind::ButtonPress,
        3,
        150,
        150,
    );
    assert_eq!(f.state.buttons_down, BUTTON_3);
    press_over_d(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert!(f.state.active_pointer_grab.is_none());
    assert_eq!(f.state.buttons_down, BUTTON_1 | BUTTON_3);
    assert!(read_packets(&mut f.client_a).is_empty());
    assert!(read_packets(&mut f.client_b).is_empty());

    release(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert_eq!(f.state.buttons_down, BUTTON_3);
    release(&mut f, InputOrigin::Physical(source), 3);
    assert_task1_end_state(&f, &[]);
}

#[test]
fn held_master_button_survives_slave_grab() {
    let mut f = p_fixture();
    assert_eq!(f.state.buttons_down, 0);
    let source = crate::xinput::InputSourceId(0xABCD);
    let physical_device = super::pointer_source(&mut f.state, source.0, true);
    assert_ne!(physical_device, XI_XTEST);
    xi_grab_button(&mut f, CLIENT_A, XI_XTEST, W, 1, 1, false, XI_BUTTONS);

    send(
        &mut f,
        InputOrigin::Physical(source),
        PointerEventKind::ButtonPress,
        3,
        150,
        150,
    );
    assert_eq!(f.state.buttons_down, BUTTON_3);
    assert!(read_packets(&mut f.client_b).is_empty());

    press_over_d(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    let grab = f.state.xi2_pointer_grabs.get(&XI_XTEST).copied();
    assert!(
        grab.is_some_and(|grab| {
            grab.owner == ClientId(CLIENT_A)
                && grab.grab_window == ResourceId(W)
                && grab.via_xi2
                && grab.passive
                && !grab.implicit
        }),
        "expected slave passive activation, got grab={grab:?}, buttons_down=0x{:x}",
        f.state.buttons_down
    );
    assert_eq!(
        f.state.xi2_detached_masters.get(&XI_XTEST),
        Some(&XI_MASTER)
    );
    assert_eq!(
        f.state.buttons_down, BUTTON_3,
        "slave activation preserves the other source's held master button"
    );
    assert!(read_packets(&mut f.client_b).is_empty());

    release(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert_eq!(f.state.buttons_down, BUTTON_3);
    assert!(read_packets(&mut f.client_b).is_empty());
    release(&mut f, InputOrigin::Physical(source), 3);
    assert!(read_packets(&mut f.client_b).is_empty());
    assert_task1_end_state(&f, &[XI_XTEST]);
}

#[test]
fn p4c_core_requests_keep_xi2_grab() {
    let mut f = p_fixture();
    select_xi2(&mut f, CLIENT_B, D, XI_MASTER, XI_BUTTONS);
    xi_grab_button(
        &mut f, CLIENT_A, XI_MASTER, D, ANY_BUTTON, 0, false, XI_BUTTONS,
    );
    grab_button_core(&mut f, CLIENT_A, D, ANY_BUTTON, ANY_MODIFIER, 0, false);

    let same_window_grabs: Vec<_> = f
        .state
        .button_grabs
        .iter()
        .filter(|grab| grab.owner == ClientId(CLIENT_A) && grab.grab_window == ResourceId(D))
        .collect();
    assert_eq!(same_window_grabs.len(), 2, "core and XI2 grabs coexist");

    ungrab_button_core(&mut f, CLIENT_A, D, ANY_BUTTON, ANY_MODIFIER);
    let remaining: Vec<_> = f
        .state
        .button_grabs
        .iter()
        .filter(|grab| grab.owner == ClientId(CLIENT_A) && grab.grab_window == ResourceId(D))
        .collect();
    assert_eq!(
        remaining.len(),
        1,
        "UngrabButton removes only the core grab"
    );
    assert!(remaining[0].via_xi2 && remaining[0].device_id == XI_MASTER);

    press_over_d(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert!(f.state.active_pointer_grab.is_some_and(|grab| {
        grab.owner == ClientId(CLIENT_A)
            && grab.grab_window == ResourceId(D)
            && grab.via_xi2
            && grab.passive
            && !grab.implicit
    }));
    assert!(f.state.xi2_pointer_grabs.is_empty());
    assert_eq!(f.state.buttons_down, BUTTON_1);
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi(4, XI_MASTER, XI_XTEST, D)]
    );
    assert!(read_packets(&mut f.client_b).is_empty());

    xi_allow_events(&mut f, CLIENT_A, XI_MASTER, 2); // ReplayDevice
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(4, XI_MASTER, XI_XTEST, D)]
    );
    assert!(read_packets(&mut f.client_a).is_empty());
    release(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(5, XI_MASTER, XI_XTEST, D)]
    );
    assert!(read_packets(&mut f.client_a).is_empty());
    assert_task1_end_state(&f, &[XI_MASTER, XI_MASTER_KEYBOARD]);
}

#[test]
fn p4b_older_xi2_newer_core_core_wins() {
    let mut f = p_fixture();
    select_xi2(&mut f, CLIENT_B, D, XI_MASTER, XI_BUTTONS);
    xi_grab_button(
        &mut f, CLIENT_A, XI_MASTER, D, ANY_BUTTON, 0, false, XI_BUTTONS,
    );
    grab_button_core(&mut f, CLIENT_A, D, ANY_BUTTON, ANY_MODIFIER, 0, false);
    assert_eq!(
        f.state
            .button_grabs
            .iter()
            .filter(|grab| grab.owner == ClientId(CLIENT_A) && grab.grab_window == ResourceId(D))
            .count(),
        2,
        "both registrations exist before the press"
    );

    press_over_d(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert!(f.state.active_pointer_grab.is_some_and(|grab| {
        grab.owner == ClientId(CLIENT_A)
            && grab.grab_window == ResourceId(D)
            && !grab.via_xi2
            && grab.passive
            && !grab.implicit
    }));
    assert!(f.state.xi2_pointer_grabs.is_empty());
    assert_eq!(f.state.buttons_down, BUTTON_1);
    assert_eq!(read_packets(&mut f.client_a), vec![core(4, D)]);
    assert!(read_packets(&mut f.client_b).is_empty());

    allow_events_core(&mut f, CLIENT_A, 2); // ReplayPointer
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(4, XI_MASTER, XI_XTEST, D)]
    );
    assert!(read_packets(&mut f.client_a).is_empty());
    release(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(5, XI_MASTER, XI_XTEST, D)]
    );
    assert!(read_packets(&mut f.client_a).is_empty());
    assert_task1_end_state(&f, &[XI_MASTER, XI_MASTER_KEYBOARD]);
}

#[test]
fn p5_slave_grab_on_descendant_beats_core_grab_on_ancestor() {
    let mut f = p_fixture();
    select_xi2(&mut f, CLIENT_B, D, XI_MASTER, XI_BUTTONS);
    grab_button_core(&mut f, CLIENT_A, W, ANY_BUTTON, ANY_MODIFIER, 1, false);
    xi_grab_button(&mut f, CLIENT_B, XI_XTEST, D, 1, 1, false, XI_BUTTONS);

    press_over_d(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert!(
        f.state
            .xi2_pointer_grabs
            .get(&XI_XTEST)
            .is_some_and(|grab| {
                grab.owner == ClientId(CLIENT_B)
                    && grab.grab_window == ResourceId(D)
                    && grab.via_xi2
                    && grab.passive
                    && !grab.implicit
            })
    );
    assert!(f.state.active_pointer_grab.is_none());
    assert_eq!(f.state.buttons_down, 0);
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(4, XI_XTEST, XI_XTEST, D)]
    );
    assert!(read_packets(&mut f.client_a).is_empty());

    release(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(5, XI_XTEST, XI_XTEST, D)]
    );
    assert!(read_packets(&mut f.client_a).is_empty());
    assert_task1_end_state(&f, &[XI_XTEST]);
}

#[test]
fn xiall_devices_grab_activates_on_slave_pass() {
    let mut f = p_fixture();
    xi_grab_button(&mut f, CLIENT_A, XI_ALL_DEVICES, W, 1, 1, false, XI_BUTTONS);

    press_over_d(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert!(
        f.state
            .xi2_pointer_grabs
            .get(&XI_XTEST)
            .is_some_and(|grab| {
                grab.owner == ClientId(CLIENT_A)
                    && grab.grab_window == ResourceId(W)
                    && grab.via_xi2
                    && grab.passive
                    && !grab.implicit
            })
    );
    assert_eq!(
        f.state.xi2_detached_masters.get(&XI_XTEST),
        Some(&XI_MASTER)
    );
    assert!(f.state.active_pointer_grab.is_none());
    assert_eq!(f.state.buttons_down, 0);
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi(4, XI_XTEST, XI_XTEST, W)]
    );
    assert!(read_packets(&mut f.client_b).is_empty());

    release(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi(5, XI_XTEST, XI_XTEST, W)]
    );
    assert!(read_packets(&mut f.client_b).is_empty());
    assert_task1_end_state(&f, &[XI_XTEST]);
}

#[test]
fn each_ungrab_removes_only_its_protocol() {
    let mut f = p_fixture();
    grab_button_core(&mut f, CLIENT_A, W, 1, ANY_MODIFIER, 1, false);
    xi_grab_button(&mut f, CLIENT_A, XI_MASTER, W, 1, 1, false, XI_BUTTONS);
    assert_eq!(f.state.button_grabs.len(), 2);

    xi_ungrab_button(&mut f, CLIENT_A, XI_MASTER, W, 1);
    assert_eq!(f.state.button_grabs.len(), 1);
    assert!(!f.state.button_grabs[0].via_xi2);

    xi_grab_button(&mut f, CLIENT_A, XI_MASTER, W, 1, 1, false, XI_BUTTONS);
    assert_eq!(f.state.button_grabs.len(), 2);
    ungrab_button_core(&mut f, CLIENT_A, W, 1, ANY_MODIFIER);
    assert_eq!(
        f.state.button_grabs.len(),
        1,
        "ungrab state: {:?}",
        f.state.button_grabs
    );
    assert!(f.state.button_grabs[0].via_xi2);
    assert_eq!(f.state.button_grabs[0].device_id, XI_MASTER);
    assert_task1_end_state(&f, &[]);
}
