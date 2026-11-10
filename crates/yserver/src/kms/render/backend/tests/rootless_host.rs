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

/// The host's keyboard state is X's as it is: a Shift the compositor holds
/// and a Caps Lock it has on are in the state of the next key, and no key
/// was pressed for them.
#[test]
fn the_hosts_keyboard_state_is_xs_without_a_key_pressed_for_it() {
    let mut b = KmsBackend::for_tests();
    let mut state = BackendTopology::from_backend(&mut b).into_server_state();
    yserver_core::backend::install_backend_root_bindings(&mut state, &b);
    let mut peer = kbd_map_client_id(&mut state, CLIENT);
    yserver_core::rootless::enable(&mut state, &mut b, CoordinatePolicy::Opaque).expect("rootless");
    let a = toplevel(&mut state, &mut b, A, 0);
    let mut work = HostWork::default();
    work.request(HostRequest::Activated { id: a });
    work.keyboard(crate::rootless::display::HostKeyboardState {
        depressed: 0x01,
        latched: 0,
        locked: 0x02,
        group: 0,
    });
    b.apply_host_work(&mut state, work);
    assert_eq!(b.serialize_modifiers() & 0xff, 0x03);
    kbd_map_drain(&mut peer);

    let mut work = HostWork::default();
    work.input(key(true));
    b.apply_host_work(&mut state, work);
    let presses: Vec<u16> = kbd_map_drain(&mut peer)
        .chunks(32)
        .filter(|e| e.len() == 32 && e[0] & 0x7f == 2)
        .map(|e| u16::from_le_bytes([e[28], e[29]]))
        .collect();
    assert_eq!(presses, [0x03], "one press, under Shift and Lock");
}

/// A host whose work comes from its own calls, not from a read of its
/// connection.
#[derive(Default)]
struct QueuingHost {
    queued: Vec<HostRequest>,
}

impl crate::rootless::host::RootlessHost for QueuingHost {
    fn handoff(&self, _id: ToplevelId, _depth: u8) -> crate::rootless::host::HandoffKind {
        crate::rootless::host::HandoffKind::Cpu
    }

    fn create_surface(
        &mut self,
        _id: ToplevelId,
        _vk: &crate::kms::vk::device::VkContext,
    ) -> std::io::Result<ash::vk::SurfaceKHR> {
        Err(std::io::Error::other("CPU frames"))
    }

    fn submit_cpu(&mut self, _frame: crate::rootless::host::CpuFrame) {}

    fn first_frame(&mut self, _id: ToplevelId) {}

    fn on_events(&mut self, _events: &[yserver_core::rootless::RootlessEvent]) {}
}

impl crate::rootless::display::DisplayHost for QueuingHost {
    fn fd(&self) -> std::os::fd::RawFd {
        -1
    }

    fn dispatch(&mut self, _work: &mut HostWork) -> std::io::Result<()> {
        Ok(())
    }

    fn take_queued(&mut self, work: &mut HostWork) {
        for request in self.queued.drain(..) {
            work.request(request);
        }
    }

    fn screen(&self) -> Option<crate::rootless::display::HostScreen> {
        None
    }
}

/// What a host queues between reads of its connection (the frame events of
/// the presenter's tick) is applied on the next loop iteration, without
/// waiting for the connection to be readable: a host asks for that
/// iteration with its wakeup, and work left queued would wake the loop for
/// ever.
#[test]
fn what_a_host_queued_between_reads_is_applied_on_the_next_iteration() {
    let mut b = KmsBackend::for_tests();
    let mut state = BackendTopology::from_backend(&mut b).into_server_state();
    yserver_core::backend::install_backend_root_bindings(&mut state, &b);
    let _peer = kbd_map_client_id(&mut state, CLIENT);
    let host = std::rc::Rc::new(std::cell::RefCell::new(QueuingHost::default()));
    b.attach_rootless_host(host.clone()).expect("attach");
    b.start_rootless(&mut state).expect("rootless");
    let a = toplevel(&mut state, &mut b, A, 0);

    host.borrow_mut()
        .queued
        .push(HostRequest::Activated { id: a });
    b.rootless_service(&mut state);

    assert_eq!(state.core_focus.raw, A);
    assert!(host.borrow().queued.is_empty());
}

/// The host's screen changing size later resizes the root, and the core
/// tells the host the root it has now.
#[test]
fn a_later_host_screen_size_is_the_roots() {
    let mut b = KmsBackend::for_tests();
    let mut state = BackendTopology::from_backend(&mut b).into_server_state();
    yserver_core::backend::install_backend_root_bindings(&mut state, &b);
    let _peer = kbd_map_client_id(&mut state, CLIENT);
    let host = std::rc::Rc::new(std::cell::RefCell::new(QueuingHost::default()));
    b.attach_rootless_host(host.clone()).expect("attach");
    b.start_rootless(&mut state).expect("rootless");

    let mut work = HostWork::default();
    work.request(HostRequest::ScreenLayout {
        outputs: vec![yserver_core::rootless::HostOutput {
            x: 0,
            y: 0,
            width: 1024,
            height: 700,
        }],
    });
    b.apply_host_work(&mut state, work);

    assert_eq!((b.platform.fb_w, b.platform.fb_h), (1024, 700));
    assert_eq!(
        (state.randr.screen_width, state.randr.screen_height),
        (1024, 700)
    );
    let root = state
        .resources
        .window(ROOT_WINDOW)
        .map(|root| (root.width, root.height));
    assert_eq!(root, Some((1024, 700)));
}
