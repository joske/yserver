//! A rootless host served on the core thread: what one read of its
//! connection said is applied in the order it was said.

use super::*;
use crate::rootless::display::HostWork;
use yserver_core::{
    backend::BackendTopology,
    core_loop::{HostInputEvent, InputOrigin, process_request},
    host_x11::HostKeyEvent,
    resources::ROOT_WINDOW,
    rootless::{CoordinatePolicy, HostRequest, ToplevelId},
    server::ServerState,
};
use yserver_protocol::x11::{ClientId, RequestHeader, ResourceId, SequenceNumber};

const CLIENT: u32 = 14;
const A: u32 = 0x0010_0b01;
const B: u32 = 0x0010_0b02;
/// X keycode 38, `a` in the default keymap.
const KEY: u8 = 38;

fn request(state: &mut ServerState, b: &mut KmsBackend, opcode: u8, body: &[u8]) {
    process_request::process_request(
        state,
        b,
        ClientId(CLIENT),
        SequenceNumber(1),
        RequestHeader {
            opcode,
            data: 0,
            length_units: u32::try_from(1 + body.len() / 4).expect("length"),
        },
        body,
        None,
    )
    .expect("process_request");
}

/// A mapped top-level at `x` that selects KeyPress, and its id.
fn toplevel(state: &mut ServerState, b: &mut KmsBackend, wid: u32, x: i16) -> ToplevelId {
    let body = [
        wid.to_le_bytes().as_slice(),
        &ROOT_WINDOW.0.to_le_bytes(),
        &x.to_le_bytes(),
        &0i16.to_le_bytes(),
        &100u16.to_le_bytes(),
        &100u16.to_le_bytes(),
        &0u16.to_le_bytes(),
        &1u16.to_le_bytes(),
        &0u32.to_le_bytes(),
        // CWEventMask: KeyPress.
        &0x0800u32.to_le_bytes(),
        &1u32.to_le_bytes(),
    ]
    .concat();
    request(state, b, 1, &body);
    request(state, b, 8, &wid.to_le_bytes());
    yserver_core::rootless::reconcile_and_dispatch(state, b);
    state
        .rootless
        .as_ref()
        .and_then(|r| r.toplevel_id(ResourceId(wid)))
        .expect("a top-level")
}

fn key(pressed: bool) -> HostInputEvent {
    HostInputEvent::Key(HostKeyEvent {
        origin: InputOrigin::NestedHost,
        keycode: KEY,
        pressed,
        state: 0,
        root_x: 0,
        root_y: 0,
        event_x: 0,
        event_y: 0,
        time: 0,
    })
}

/// The windows the client's KeyPress events were sent to.
fn key_press_windows(peer: &mut std::os::unix::net::UnixStream) -> Vec<u32> {
    kbd_map_drain(peer)
        .chunks(32)
        .filter(|e| e.len() == 32 && e[0] & 0x7f == 2)
        .map(|e| u32::from_le_bytes(e[12..16].try_into().unwrap()))
        .collect()
}

/// A key pressed on the first window and then a focus change to a second,
/// read in one go, reach the core in that order: the press goes to the
/// window that had the focus when it was pressed. Applied by kind (input
/// after requests), it went to the second.
#[test]
fn a_key_read_before_a_focus_change_goes_to_the_window_it_was_pressed_in() {
    let mut b = KmsBackend::for_tests();
    let mut state = BackendTopology::from_backend(&mut b).into_server_state();
    yserver_core::backend::install_backend_root_bindings(&mut state, &b);
    let mut peer = kbd_map_client_id(&mut state, CLIENT);
    yserver_core::rootless::enable(&mut state, &mut b, CoordinatePolicy::Opaque).expect("rootless");
    let a = toplevel(&mut state, &mut b, A, 0);
    let second = toplevel(&mut state, &mut b, B, 200);

    let mut work = HostWork::default();
    work.request(HostRequest::Activated { id: a });
    b.apply_host_work(&mut state, work);
    assert_eq!(state.core_focus.raw, A);
    kbd_map_drain(&mut peer);

    let mut work = HostWork::default();
    work.input(key(true));
    work.request(HostRequest::Deactivated);
    work.request(HostRequest::Activated { id: second });
    work.input(key(false));
    b.apply_host_work(&mut state, work);

    assert_eq!(state.core_focus.raw, B);
    assert_eq!(key_press_windows(&mut peer), [A]);
}
