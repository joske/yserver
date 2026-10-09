use super::*;

#[test]
fn xi_dynamic_master_motion_only_grab_gets_input_thread_wheel_motion() {
    use yserver_core::{
        backend::Backend,
        core_loop::{DeviceInfo, HostInputEvent, process_request},
        resources::ROOT_WINDOW,
        server::ServerState,
        xinput::{InputCapabilities, InputSourceId, XiFacetKind},
    };
    use yserver_protocol::x11::{ClientId, RequestHeader, SequenceNumber};

    const CLIENT: u32 = 0xA729;
    const SOURCE: InputSourceId = InputSourceId(0xA7291);
    const XI_MOTION: u32 = 1 << 6;

    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    backend
        .core
        .xid_map
        .insert(backend.core.window_id, ROOT_WINDOW);
    let mut peer = kbd_map_client_id(&mut state, CLIENT);
    let send_request =
        |state: &mut ServerState, backend: &mut KmsBackend, sequence, minor, body: &[u8]| {
            process_request::process_request(
                state,
                backend,
                ClientId(CLIENT),
                SequenceNumber(sequence),
                RequestHeader {
                    opcode: 137,
                    data: minor,
                    length_units: u32::try_from((body.len() + 4) / 4).unwrap(),
                },
                body,
                None,
            )
            .expect("production XI2 request")
        };

    send_request(&mut state, &mut backend, 1, 47, &[2, 0, 3, 0]);
    assert_eq!(kbd_map_drain(&mut peer)[0], 1, "XIQueryVersion reply");
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceAdded(DeviceInfo {
            source_id: SOURCE,
            enabled: true,
            resume_key: None,
            capabilities: InputCapabilities {
                keyboard: false,
                pointer: true,
                touch: false,
            },
            name: "one physical wheel mouse".to_owned(),
            device_node: "/dev/input/event-one-mouse".to_owned(),
            sysname: "event-one-mouse".to_owned(),
            vendor_id: 1,
            product_id: 1,
            is_touchpad: false,
            config: Default::default(),
        }),
    );
    let pointer_id = state
        .xi_devices
        .facet(SOURCE, XiFacetKind::PointerTouch)
        .expect("the sole physical mouse has one slave pointer");

    let mut grab = Vec::with_capacity(20);
    grab.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    grab.extend_from_slice(&0u32.to_le_bytes()); // current time
    grab.extend_from_slice(&0u32.to_le_bytes()); // no cursor
    grab.extend_from_slice(&2u16.to_le_bytes()); // master pointer
    grab.extend_from_slice(&[1, 1, 0, 0]); // async modes, owner_events=false
    grab.extend_from_slice(&1u16.to_le_bytes()); // one mask word
    grab.extend_from_slice(&XI_MOTION.to_le_bytes());
    send_request(&mut state, &mut backend, 2, 51, &grab);
    assert_eq!(kbd_map_drain(&mut peer)[0], 1, "XIGrabDevice reply");
    assert!(state.active_pointer_grab.is_some_and(|grab| {
        grab.owner == ClientId(CLIENT) && grab.xi2_mask == u64::from(XI_MOTION)
    }));
    assert_eq!(
        state.xi_devices.device(pointer_id).unwrap().attached_master,
        Some(2)
    );

    let (_poll, sender, receiver) = yserver_core::core_loop::channel().expect("input channel");
    let mut input_thread = crate::input_thread::LibinputThreadState::new(800, 600);
    let mut pending_motion = None;
    crate::input_thread::process_batch(
        &mut input_thread,
        &sender,
        &mut pending_motion,
        [
            crate::input::InputEvent::PointerMotion {
                source_id: SOURCE,
                dx: 2.0,
                dy: 1.0,
            },
            crate::input::InputEvent::PointerScroll {
                source_id: SOURCE,
                dx_v120: 0,
                dy_v120: 120,
            },
        ],
        7,
    )
    .expect("input-thread production scroll conversion");
    let messages: Vec<_> = receiver.try_recv_all().collect();
    assert_eq!(
        messages.len(),
        3,
        "motion then a button-5 press/release pair"
    );
    let inputs: Vec<HostInputEvent> = messages
        .into_iter()
        .map(|message| match message {
            yserver_core::core_loop::Message::HostInput(input) => input,
            other => panic!("unexpected input-thread message: {other:?}"),
        })
        .collect();
    assert!(matches!(
        inputs.as_slice(),
        [
            HostInputEvent::PointerMotion { .. },
            HostInputEvent::PointerButton {
                button: 0x181,
                pressed: true,
                ..
            },
            HostInputEvent::PointerButton {
                button: 0x181,
                pressed: false,
                ..
            }
        ]
    ));

    let mut output = Vec::new();
    for input in inputs {
        Backend::on_host_input(&mut backend, &mut state, input);
        output.push(kbd_map_drain(&mut peer));
    }
    let ordinary_motion = xi2_events(&output[0]);
    let wheel_motion = xi2_events(&output[1]);
    let wheel_release = xi2_events(&output[2]);

    assert_eq!(ordinary_motion.len(), 1);
    assert_eq!(
        ordinary_motion[0].0, 6,
        "ordinary pointer motion is delivered"
    );
    assert_eq!(ordinary_motion[0].1, 2, "master grab keeps master identity");
    // Xorg getevents.c:1747 changes the scroll event's type to MotionNotify
    // before the active-grab event-mask filter runs.
    assert_eq!(
        wheel_motion.len(),
        1,
        "motion-only grab receives wheel Motion"
    );
    assert_eq!(wheel_motion[0].0, 6);
    assert_eq!(wheel_motion[0].1, 2);
    assert_eq!(wheel_motion[0].2, pointer_id);
    assert!(
        wheel_release.is_empty(),
        "Motion-only grab has no ButtonRelease"
    );
    assert!(
        state.active_pointer_grab.is_some(),
        "master grab stays active"
    );
    assert_eq!(
        state.xi_devices.device(pointer_id).unwrap().attached_master,
        Some(2)
    );
    assert_eq!(state.xi_devices.device(pointer_id).unwrap().buttons_down, 0);
    assert_eq!(state.buttons_down, 0);
    assert!(state.sync_pending.is_empty());
    assert!(!state.xi1_frozen[&2].frozen());
}

fn xi_owner_events_scroll_fixture() -> (
    crate::kms::render::backend::KmsBackend,
    yserver_core::server::ServerState,
    std::os::unix::net::UnixStream,
    yserver_core::xinput::InputSourceId,
    u16,
    yserver_protocol::x11::ResourceId,
) {
    use yserver_core::{
        backend::Backend,
        core_loop::{DeviceInfo, HostInputEvent, InputOrigin},
        resources::ROOT_WINDOW,
        server::ServerState,
        xinput::{InputCapabilities, InputSourceId, XiFacetKind},
    };
    use yserver_protocol::x11::ResourceId;

    const CLIENT: u32 = 5;
    const SOURCE: InputSourceId = InputSourceId(0xA7341);
    const CHILD: ResourceId = ResourceId(0x0010_A734);

    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    let mut peer = kbd_map_client_id(&mut state, CLIENT);
    backend
        .core
        .xid_map
        .insert(backend.core.window_id, ROOT_WINDOW);

    // This is the same DeviceAdded callback the KMS backend receives
    // from the input lifecycle, so the source and its enabled pointer
    // facet are produced by the real registry path.
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceAdded(DeviceInfo {
            source_id: SOURCE,
            enabled: true,
            resume_key: None,
            capabilities: InputCapabilities {
                keyboard: false,
                pointer: true,
                touch: false,
            },
            name: "owner-events smooth-scroll mouse".to_owned(),
            device_node: "/dev/input/event-owner-scroll".to_owned(),
            sysname: "event-owner-scroll".to_owned(),
            vendor_id: 1,
            product_id: 1,
            is_touchpad: false,
            config: Default::default(),
        }),
    );
    let pointer_id = state
        .xi_devices
        .facet(SOURCE, XiFacetKind::PointerTouch)
        .expect("physical pointer facet follows DeviceAdded");

    // Create and map an owned child through the core request dispatcher.
    // This gives the owner natural delivery on the child while the active
    // grab's root window remains an observable fallback target.
    let mut create_window = Vec::with_capacity(28);
    create_window.extend_from_slice(&CHILD.0.to_le_bytes());
    create_window.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    create_window.extend_from_slice(&20i16.to_le_bytes());
    create_window.extend_from_slice(&30i16.to_le_bytes());
    create_window.extend_from_slice(&100u16.to_le_bytes());
    create_window.extend_from_slice(&100u16.to_le_bytes());
    create_window.extend_from_slice(&0u16.to_le_bytes()); // border width
    create_window.extend_from_slice(&1u16.to_le_bytes()); // InputOutput
    create_window.extend_from_slice(&yserver_core::resources::ROOT_VISUAL.0.to_le_bytes());
    create_window.extend_from_slice(&0u32.to_le_bytes()); // value mask
    assert!(
        xi_xtest_grab_request(
            &mut state,
            &mut backend,
            &mut peer,
            1,
            1,
            24,
            &create_window,
        )
        .is_empty()
    );
    assert!(
        xi_xtest_grab_request(
            &mut state,
            &mut backend,
            &mut peer,
            2,
            8,
            0,
            &CHILD.0.to_le_bytes(),
        )
        .is_empty()
    );

    // Place the live source over the child through the KMS input
    // callback used for host motion.
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::PointerMotion {
            origin: InputOrigin::Physical(SOURCE),
            x: 45,
            y: 65,
            time: 40,
            relative: false,
            dx: 0,
            dy: 0,
            motion_delta: None,
        },
    );

    (backend, state, peer, SOURCE, pointer_id, CHILD)
}

fn xi_owner_events_select_body(window: yserver_protocol::x11::ResourceId, mask: u32) -> Vec<u8> {
    let mut body = Vec::with_capacity(20);
    body.extend_from_slice(&window.0.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes()); // one event mask
    body.extend_from_slice(&[0; 2]);
    body.extend_from_slice(&2u16.to_le_bytes()); // master pointer
    body.extend_from_slice(&u16::from(mask != 0).to_le_bytes()); // mask_len
    if mask != 0 {
        body.extend_from_slice(&mask.to_le_bytes());
    }
    body
}

fn xi_owner_events_grab_body(mask: u32) -> Vec<u8> {
    let mut body = Vec::with_capacity(20);
    body.extend_from_slice(&yserver_core::resources::ROOT_WINDOW.0.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    body.extend_from_slice(&0u32.to_le_bytes()); // no cursor
    body.extend_from_slice(&2u16.to_le_bytes()); // master pointer
    body.extend_from_slice(&[1, 1, 1, 0]); // async, async paired, owner_events=true
    body.extend_from_slice(&1u16.to_le_bytes());
    body.extend_from_slice(&mask.to_le_bytes());
    body
}

fn xi_owner_events_scroll_events(bytes: &[u8]) -> Vec<(u16, u32, i16, i16)> {
    let mut events = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        assert_eq!(bytes[offset], 35, "XI2 GenericEvent stream: {bytes:02x?}");
        let extra_units = u32::from_le_bytes(
            bytes[offset + 4..offset + 8]
                .try_into()
                .expect("GenericEvent length"),
        ) as usize;
        let event_len = 32 + extra_units * 4;
        assert!(offset + event_len <= bytes.len(), "complete GenericEvent");
        let read_i16_fp1616 = |at: usize| {
            (i32::from_le_bytes(bytes[offset + at..offset + at + 4].try_into().unwrap()) >> 16)
                as i16
        };
        events.push((
            u16::from_le_bytes(bytes[offset + 8..offset + 10].try_into().unwrap()),
            u32::from_le_bytes(bytes[offset + 24..offset + 28].try_into().unwrap()),
            read_i16_fp1616(40),
            read_i16_fp1616(44),
        ));
        offset += event_len;
    }
    events
}

fn xi_owner_events_state_snapshot(
    state: &yserver_core::server::ServerState,
    backend: &crate::kms::render::backend::KmsBackend,
    client: u32,
) -> String {
    let selections = &state.clients[&client];
    format!(
        "registry={:?}; held={:?}; detached={:?}; floating={:?}; properties={:?}; \
             selections={:?}; cursor={:?}; pointer_grab={:?}; slave_grabs={:?}; keyboard_grabs={:?}",
        xi_xtest_registry_snapshot(state),
        xi_xtest_held_snapshot(state, backend),
        state.xi2_detached_masters,
        state.floating_pointer_positions,
        xi_xtest_property_snapshot(state),
        (
            &selections.xi2_masks,
            &selections.xi1_event_classes,
            &selections.xi1_window_event_classes,
        ),
        (
            state.pointer_root,
            backend.core.cursor_x,
            backend.core.cursor_y,
        ),
        state.active_pointer_grab,
        state.xi2_pointer_grabs,
        state.xi2_keyboard_grabs,
    )
}

// Xorg dix/events.c:4431-4464 tries owner-events natural delivery for
// each event and calls DeliverOneGrabbedEvent only if that event reached
// nobody. Kills requiring the emulated ButtonPress branch to create the
// fallback marker used by its separate smooth-scroll Motion.
#[test]
fn xi_owner_events_scroll_motion_falls_back_by_its_own_type() {
    use yserver_core::{
        backend::Backend,
        core_loop::{HostInputEvent, InputOrigin},
    };
    use yserver_protocol::x11::ClientId;

    const CLIENT: u32 = 5;
    const XI_BUTTON_PRESS: u32 = 1 << 4;
    const XI_MOTION: u32 = 1 << 6;

    let (mut backend, mut state, mut peer, source, pointer, child) =
        xi_owner_events_scroll_fixture();
    let before = xi_owner_events_state_snapshot(&state, &backend, CLIENT);
    assert_eq!(
        xi_xtest_grab_request(
            &mut state,
            &mut backend,
            &mut peer,
            3,
            137,
            47,
            &[2, 0, 3, 0],
        )
        .first(),
        Some(&1),
        "XIQueryVersion reply"
    );

    // Use the core request dispatcher for the selection and active grab.
    assert!(
        xi_xtest_grab_request(
            &mut state,
            &mut backend,
            &mut peer,
            4,
            137,
            46,
            &xi_owner_events_select_body(child, XI_BUTTON_PRESS),
        )
        .is_empty()
    );
    let grab_reply = xi_xtest_grab_request(
        &mut state,
        &mut backend,
        &mut peer,
        5,
        137,
        51,
        &xi_owner_events_grab_body(XI_MOTION),
    );
    assert_eq!(xi_xtest_grab_reply_status(&grab_reply), Some(0));
    assert!(state.active_pointer_grab.is_some_and(|grab| {
        grab.owner == ClientId(CLIENT)
            && grab.grab_window.0 == yserver_core::resources::ROOT_WINDOW.0
    }));

    let origin = InputOrigin::Physical(source);
    for pressed in [true, false] {
        Backend::on_host_input(
            &mut backend,
            &mut state,
            HostInputEvent::PointerButton {
                origin,
                button: 0x180, // input-thread SYNTH_SCROLL_UP
                pressed,
                time: 50,
            },
        );
    }
    let events = xi_owner_events_scroll_events(&kbd_map_drain(&mut peer));
    assert_eq!(events.len(), 2, "one scroll Motion and one emulated press");
    let press = events.iter().find(|event| event.0 == 4).unwrap();
    let motion = events.iter().find(|event| event.0 == 6).unwrap();
    assert_eq!((press.1, press.2, press.3), (child.0, 25, 35));
    assert_eq!(
        (motion.1, motion.2, motion.3),
        (yserver_core::resources::ROOT_WINDOW.0, 45, 65),
        "Motion falls back to the root grab window with root-relative coordinates"
    );
    assert_eq!(state.xi_devices.device(pointer).unwrap().buttons_down, 0);
    assert_eq!(state.buttons_down, 0);

    let _ungrab_events = xi_xtest_grab_request(
        &mut state,
        &mut backend,
        &mut peer,
        6,
        137,
        52,
        &xi_xtest_ungrab_body(2),
    );
    assert!(
        xi_xtest_grab_request(
            &mut state,
            &mut backend,
            &mut peer,
            7,
            137,
            46,
            &xi_owner_events_select_body(child, 0),
        )
        .is_empty()
    );
    assert_eq!(
        xi_owner_events_state_snapshot(&state, &backend, CLIENT),
        before,
        "ungrab and deselect restore registry, held state, maps, and selections"
    );
}

// Xorg dix/events.c:4431-4464 keeps an event on its natural selected
// window when owner-events delivery succeeds. Kills unconditionally
// redirecting Motion to the grab window despite a natural Motion selection.
#[test]
fn xi_owner_events_scroll_motion_stays_on_naturally_selected_child() {
    use yserver_core::{
        backend::Backend,
        core_loop::{HostInputEvent, InputOrigin},
    };

    const CLIENT: u32 = 5;
    const XI_BUTTON_PRESS: u32 = 1 << 4;
    const XI_MOTION: u32 = 1 << 6;

    let (mut backend, mut state, mut peer, source, pointer, child) =
        xi_owner_events_scroll_fixture();
    let before = xi_owner_events_state_snapshot(&state, &backend, CLIENT);
    assert_eq!(
        xi_xtest_grab_request(
            &mut state,
            &mut backend,
            &mut peer,
            3,
            137,
            47,
            &[2, 0, 3, 0],
        )
        .first(),
        Some(&1),
        "XIQueryVersion reply"
    );
    assert!(
        xi_xtest_grab_request(
            &mut state,
            &mut backend,
            &mut peer,
            4,
            137,
            46,
            &xi_owner_events_select_body(child, XI_BUTTON_PRESS | XI_MOTION),
        )
        .is_empty()
    );
    let grab_reply = xi_xtest_grab_request(
        &mut state,
        &mut backend,
        &mut peer,
        5,
        137,
        51,
        &xi_owner_events_grab_body(XI_MOTION),
    );
    assert_eq!(xi_xtest_grab_reply_status(&grab_reply), Some(0));

    let origin = InputOrigin::Physical(source);
    for pressed in [true, false] {
        Backend::on_host_input(
            &mut backend,
            &mut state,
            HostInputEvent::PointerButton {
                origin,
                button: 0x180, // input-thread SYNTH_SCROLL_UP
                pressed,
                time: 60,
            },
        );
    }
    let events = xi_owner_events_scroll_events(&kbd_map_drain(&mut peer));
    assert_eq!(events.len(), 2, "one scroll Motion and one emulated press");
    assert!(
        events
            .iter()
            .any(|event| { event.0 == 4 && (event.1, event.2, event.3) == (child.0, 25, 35) })
    );
    assert!(
        events
            .iter()
            .any(|event| { event.0 == 6 && (event.1, event.2, event.3) == (child.0, 25, 35) })
    );
    assert!(
        events
            .iter()
            .all(|event| event.1 != yserver_core::resources::ROOT_WINDOW.0),
        "neither event fell back to the root grab window"
    );
    assert_eq!(state.xi_devices.device(pointer).unwrap().buttons_down, 0);
    assert_eq!(state.buttons_down, 0);

    let _ungrab_events = xi_xtest_grab_request(
        &mut state,
        &mut backend,
        &mut peer,
        6,
        137,
        52,
        &xi_xtest_ungrab_body(2),
    );
    assert!(
        xi_xtest_grab_request(
            &mut state,
            &mut backend,
            &mut peer,
            7,
            137,
            46,
            &xi_owner_events_select_body(child, 0),
        )
        .is_empty()
    );
    assert_eq!(
        xi_owner_events_state_snapshot(&state, &backend, CLIENT),
        before,
        "ungrab and deselect restore registry, held state, maps, and selections"
    );
}

#[test]
fn xi_dynamic_owner_events_grab_fallback_preserves_slave_button_and_motion_identity() {
    use yserver_core::{
        backend::Backend,
        core_loop::{DeviceInfo, HostInputEvent, InputOrigin, process_request},
        resources::ROOT_WINDOW,
        server::ServerState,
        xinput::{InputCapabilities, InputSourceId, XiFacetKind},
    };
    use yserver_protocol::x11::{ClientId, RequestHeader, SequenceNumber};

    const CLIENT: u32 = 0xA731;
    const SOURCE: InputSourceId = InputSourceId(0xA7311);
    const XI_BUTTON_PRESS: u32 = 1 << 4;
    const XI_BUTTON_RELEASE: u32 = 1 << 5;
    const XI_MOTION: u32 = 1 << 6;

    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    backend
        .core
        .xid_map
        .insert(backend.core.window_id, ROOT_WINDOW);
    let mut peer = kbd_map_client_id(&mut state, CLIENT);
    let send_request =
        |state: &mut ServerState, backend: &mut KmsBackend, sequence, minor, body: &[u8]| {
            process_request::process_request(
                state,
                backend,
                ClientId(CLIENT),
                SequenceNumber(sequence),
                RequestHeader {
                    opcode: 137,
                    data: minor,
                    length_units: u32::try_from((body.len() + 4) / 4).unwrap(),
                },
                body,
                None,
            )
            .expect("production XI2 request")
        };

    send_request(&mut state, &mut backend, 1, 47, &[2, 0, 3, 0]);
    assert_eq!(kbd_map_drain(&mut peer)[0], 1, "XIQueryVersion reply");
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceAdded(DeviceInfo {
            source_id: SOURCE,
            enabled: true,
            resume_key: None,
            capabilities: InputCapabilities {
                keyboard: false,
                pointer: true,
                touch: false,
            },
            name: "owner-events fallback mouse".to_owned(),
            device_node: "/dev/input/event-owner-events".to_owned(),
            sysname: "event-owner-events".to_owned(),
            vendor_id: 1,
            product_id: 1,
            is_touchpad: false,
            config: Default::default(),
        }),
    );
    let pointer_id = state
        .xi_devices
        .facet(SOURCE, XiFacetKind::PointerTouch)
        .expect("physical mouse pointer facet");

    let mut grab = Vec::with_capacity(20);
    grab.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    grab.extend_from_slice(&0u32.to_le_bytes()); // current time
    grab.extend_from_slice(&0u32.to_le_bytes()); // no cursor
    grab.extend_from_slice(&pointer_id.to_le_bytes()); // exact physical slave
    grab.extend_from_slice(&[1, 1, 1, 0]); // async modes, owner_events=true
    grab.extend_from_slice(&1u16.to_le_bytes()); // one mask word
    grab.extend_from_slice(&(XI_BUTTON_PRESS | XI_BUTTON_RELEASE | XI_MOTION).to_le_bytes());
    send_request(&mut state, &mut backend, 2, 51, &grab);
    let grab_reply = kbd_map_drain(&mut peer);
    assert_eq!(grab_reply[0], 1, "XIGrabDevice reply");
    assert_eq!(grab_reply[8], 0, "XIGrabDevice succeeds");
    assert_eq!(
        state.xi_devices.device(pointer_id).unwrap().attached_master,
        None
    );
    assert!(state.xi2_pointer_grabs.contains_key(&pointer_id));

    let physical = InputOrigin::Physical(SOURCE);
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::PointerButton {
            origin: physical,
            button: 0x110, // BTN_LEFT
            pressed: true,
            time: 10,
        },
    );
    let press = xi2_events(&kbd_map_drain(&mut peer));
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::PointerMotion {
            origin: physical,
            x: 12,
            y: 14,
            time: 11,
            relative: false,
            dx: 0,
            dy: 0,
            motion_delta: None,
        },
    );
    let motion = xi2_events(&kbd_map_drain(&mut peer));
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::PointerButton {
            origin: physical,
            button: 0x110,
            pressed: false,
            time: 12,
        },
    );
    let release = xi2_events(&kbd_map_drain(&mut peer));

    assert_eq!(press.len(), 1, "grab owner gets the fallback button press");
    assert_eq!(
        (press[0].0, press[0].1, press[0].2, press[0].3),
        (4, pointer_id, pointer_id, 1)
    );
    assert_eq!(motion.len(), 1, "grab owner gets the fallback motion");
    assert_eq!(
        (motion[0].0, motion[0].1, motion[0].2),
        (6, pointer_id, pointer_id)
    );
    assert_eq!(
        release.len(),
        1,
        "grab owner gets the fallback button release"
    );
    assert_eq!(
        (release[0].0, release[0].1, release[0].2, release[0].3),
        (5, pointer_id, pointer_id, 1)
    );
    assert_eq!(
        state.xi_devices.device(pointer_id).unwrap().attached_master,
        None
    );
    assert!(state.xi2_pointer_grabs.contains_key(&pointer_id));
    assert_eq!(state.xi_devices.device(pointer_id).unwrap().buttons_down, 0);
    assert_eq!(state.buttons_down, 0);
    assert!(state.sync_pending.is_empty());
    assert!(!state.xi1_frozen[&pointer_id].frozen());
}

#[test]
// Mutation killed: save the action mask as filter->priv instead of only its pre-press locked bits.
fn xi_lockmods_default_caps_from_unlocked_survives_keyboard_grab() {
    use yserver_core::{
        backend::Backend,
        core_loop::{DeviceInfo, HostInputEvent, InputOrigin, process_request},
        host_x11::HostKeyEvent,
        resources::ROOT_WINDOW,
        server::ServerState,
        xinput::{InputCapabilities, InputSourceId, XiFacetKind},
    };
    use yserver_protocol::x11::{ClientId, RequestHeader, SequenceNumber};

    const CLIENT: u32 = 0xA732;
    const SOURCE: InputSourceId = InputSourceId(0xA7321);
    const CAPS_LOCK: u8 = 66;
    const LOCK_MASK: u32 = 1 << 1;
    const XI_KEY_PRESS_RELEASE: u32 = (1 << 2) | (1 << 3);

    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    backend
        .core
        .xid_map
        .insert(backend.core.window_id, ROOT_WINDOW);
    let mut peer = kbd_map_client_id(&mut state, CLIENT);
    let send_request =
        |state: &mut ServerState, backend: &mut KmsBackend, sequence, minor, body: &[u8]| {
            process_request::process_request(
                state,
                backend,
                ClientId(CLIENT),
                SequenceNumber(sequence),
                RequestHeader {
                    opcode: 137,
                    data: minor,
                    length_units: u32::try_from((body.len() + 4) / 4).unwrap(),
                },
                body,
                None,
            )
            .expect("production XI2 request")
        };

    send_request(&mut state, &mut backend, 1, 47, &[2, 0, 3, 0]);
    assert_eq!(kbd_map_drain(&mut peer)[0], 1, "XIQueryVersion reply");
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceAdded(DeviceInfo {
            source_id: SOURCE,
            enabled: true,
            resume_key: None,
            capabilities: InputCapabilities {
                keyboard: true,
                pointer: false,
                touch: false,
            },
            name: "held Caps Lock keyboard".to_owned(),
            device_node: "/dev/input/event-caps-lock".to_owned(),
            sysname: "event-caps-lock".to_owned(),
            vendor_id: 1,
            product_id: 2,
            is_touchpad: false,
            config: Default::default(),
        }),
    );
    let keyboard_id = state
        .xi_devices
        .facet(SOURCE, XiFacetKind::Keyboard)
        .expect("physical keyboard facet");
    let key = |pressed| {
        HostInputEvent::Key(HostKeyEvent {
            origin: InputOrigin::Physical(SOURCE),
            pressed,
            keycode: CAPS_LOCK,
            time: 10,
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            state: 0,
        })
    };

    Backend::on_host_input(&mut backend, &mut state, key(true));
    let master_locked_before_grab = backend
        .core
        .xkb_state
        .0
        .serialize_mods(xkbcommon::xkb::STATE_MODS_LOCKED);
    assert_ne!(
        master_locked_before_grab & LOCK_MASK,
        0,
        "Caps Lock is held and locked"
    );
    assert!(state.key_down_by_device[&keyboard_id].contains_key(&CAPS_LOCK));
    let master_caps_down_bit = 1u8 << (CAPS_LOCK % 8);
    assert_ne!(
        state.keys_down[usize::from(CAPS_LOCK / 8)] & master_caps_down_bit,
        0,
        "master QueryKeymap contains Caps before detachment",
    );

    let mut grab = Vec::with_capacity(20);
    grab.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    grab.extend_from_slice(&0u32.to_le_bytes()); // current time
    grab.extend_from_slice(&0u32.to_le_bytes()); // no cursor
    grab.extend_from_slice(&keyboard_id.to_le_bytes());
    grab.extend_from_slice(&[1, 1, 0, 0]); // async modes, owner_events=false
    grab.extend_from_slice(&1u16.to_le_bytes()); // one mask word
    grab.extend_from_slice(&XI_KEY_PRESS_RELEASE.to_le_bytes());
    send_request(&mut state, &mut backend, 2, 51, &grab);
    let grab_wire = kbd_map_drain_until(&mut peer, |bytes| {
        let mut offset = 0;
        while offset + 32 <= bytes.len() {
            if bytes[offset] == 1 {
                return true;
            }
            let event_len = if bytes[offset] == 35 {
                32 + u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize
                    * 4
            } else {
                32
            };
            offset += event_len;
        }
        false
    });
    let mut offset = 0;
    while offset + 32 <= grab_wire.len() && grab_wire[offset] != 1 {
        let event_len = if grab_wire[offset] == 35 {
            32 + u32::from_le_bytes(grab_wire[offset + 4..offset + 8].try_into().unwrap()) as usize
                * 4
        } else {
            32
        };
        offset += event_len;
    }
    assert!(
        offset + 32 <= grab_wire.len(),
        "XIGrabDevice reply follows events"
    );
    assert_eq!(grab_wire[offset], 1, "XIGrabDevice reply");
    assert_eq!(grab_wire[offset + 8], 0, "XIGrabDevice succeeds");
    assert!(
        backend.floating_keyboard_states[&keyboard_id]
            .down_keys
            .contains(&CAPS_LOCK)
    );
    assert_eq!(
        state
            .xi_devices
            .device(keyboard_id)
            .unwrap()
            .attached_master,
        None
    );

    Backend::on_host_input(&mut backend, &mut state, key(false));
    let _ = kbd_map_drain(&mut peer);
    let floating_locked_after_release = backend.floating_keyboard_states[&keyboard_id]
        .xkb_state
        .0
        .serialize_mods(xkbcommon::xkb::STATE_MODS_LOCKED);

    // Xorg's lock filter at xkb/xkbActions.c:372 stores the pre-press
    // locked bits and clears only those bits on release. Detaching must
    // not reconstruct a second lock action from the held key.
    assert_eq!(
        floating_locked_after_release & LOCK_MASK,
        master_locked_before_grab & LOCK_MASK,
        "releasing held Caps Lock preserves the inherited lock",
    );
    assert!(
        !backend.floating_keyboard_states[&keyboard_id]
            .down_keys
            .contains(&CAPS_LOCK)
    );
    assert!(
        state
            .key_down_by_device
            .get(&keyboard_id)
            .is_none_or(std::collections::HashMap::is_empty)
    );
    assert_ne!(
        state.keys_down[usize::from(CAPS_LOCK / 8)] & master_caps_down_bit,
        0,
        "Xorg DetachFromMaster (dix/events.c:1463-1471) leaves the master key bitmap alone",
    );
    assert_eq!(
        state
            .xi_devices
            .device(keyboard_id)
            .unwrap()
            .attached_master,
        None
    );
    assert!(state.xi2_keyboard_grabs.contains_key(&keyboard_id));
    assert!(state.sync_pending.is_empty());
    assert!(!state.xi1_frozen[&keyboard_id].frozen());
}

#[test]
// Mutation killed: ignore LockNoUnlock and unconditionally clear the saved Lock bit on floating release.
fn xi_lockmods_affect_lock_survives_floating_keyboard_release() {
    use yserver_core::{
        backend::Backend,
        core_loop::{HostInputEvent, InputOrigin},
        host_x11::HostKeyEvent,
        resources::ROOT_WINDOW,
        server::ServerState,
        xinput::InputSourceId,
    };

    const CLIENT: u32 = 0xA742;
    const SOURCE: InputSourceId = InputSourceId(0xA7421);
    const CAPS_LOCK: u8 = 66;
    const LETTER_A: u8 = 38;
    const LOCK_MASK: u32 = 1 << 1;

    fn set_caps_lock_action(mut action: [u8; 8], flags: u8) -> Vec<u8> {
        let mut body = vec![0u8; 32];
        body[0..2].copy_from_slice(&0x0100u16.to_le_bytes()); // XkbUseCoreKbd
        body[2..4].copy_from_slice(&0x0010u16.to_le_bytes()); // XkbKeyActionsMask
        body[6] = 8; // min_key_code
        body[7] = 255; // max_key_code
        body[14] = CAPS_LOCK; // first_key_act
        body[15] = 1; // n_key_acts
        body[16..18].copy_from_slice(&1u16.to_le_bytes()); // total_acts
        body.extend_from_slice(&[1, 0, 0, 0]); // one action for Caps
        action[1] = flags;
        body.extend_from_slice(&action);
        body
    }

    fn state_snapshot(backend: &KmsBackend, state: &ServerState) -> String {
        format!(
            "{:?}",
            (
                (
                    state
                        .xi_devices
                        .devices()
                        .iter()
                        .map(|device| (
                            device.id,
                            device.source_id,
                            device.facet,
                            device.enabled,
                            device.attached_master,
                            device.buttons_down,
                            device.properties.clone(),
                        ))
                        .collect::<Vec<_>>(),
                    state.keys_down,
                    state.buttons_down,
                    state.key_down_by_device.clone(),
                    state.xi2_detached_masters.clone(),
                    state.floating_pointer_positions.clone(),
                    state.xi2_keyboard_grabs.clone(),
                ),
                (
                    state.clients[&CLIENT].xi2_masks.clone(),
                    state.clients[&CLIENT].xi1_event_classes.clone(),
                    state.clients[&CLIENT].xi1_window_event_classes.clone(),
                    backend
                        .floating_keyboard_states
                        .keys()
                        .copied()
                        .collect::<Vec<_>>(),
                    backend.lock_filter_priv_by_device.clone(),
                    backend.core.xkb_desc.acts[usize::from(CAPS_LOCK)].clone(),
                    backend
                        .core
                        .xkb_state
                        .0
                        .serialize_mods(xkbcommon::xkb::STATE_MODS_LOCKED),
                ),
            )
        )
    }

    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    backend
        .core
        .xid_map
        .insert(backend.core.window_id, ROOT_WINDOW);
    let mut peer = kbd_map_client_id(&mut state, CLIENT);
    xkb_client_request(&mut state, &mut backend, CLIENT, 136, 0, &[1, 0, 0, 0]);
    assert_eq!(kbd_map_drain(&mut peer)[0], 1, "XkbUseExtension reply");
    let baseline = state_snapshot(&backend, &state);
    let original_caps_action = backend.core.xkb_desc.acts[usize::from(CAPS_LOCK)].clone();
    let original_caps_action_value = original_caps_action.as_ref().unwrap()[0];

    // The supported action writer must retain LockNoUnlock from this real SetMap request.
    xkb_client_request(
        &mut state,
        &mut backend,
        CLIENT,
        136,
        9,
        &set_caps_lock_action(original_caps_action_value, 0x02),
    );
    assert_eq!(
        backend.core.xkb_desc.acts[usize::from(CAPS_LOCK)]
            .as_ref()
            .map(|actions| actions[0]),
        Some([
            original_caps_action_value[0],
            0x02,
            original_caps_action_value[2],
            original_caps_action_value[3],
            original_caps_action_value[4],
            original_caps_action_value[5],
            original_caps_action_value[6],
            original_caps_action_value[7],
        ]),
        "SetMap and the keymap writer retain LockMods(affect=lock)",
    );
    let _ = kbd_map_drain(&mut peer);

    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceAdded(dynamic_test_device(SOURCE, true, false)),
    );
    let keyboard_id = state
        .xi_devices
        .facet(SOURCE, yserver_core::xinput::XiFacetKind::Keyboard)
        .expect("physical keyboard facet follows production add path");
    let key = |keycode, pressed| {
        HostInputEvent::Key(HostKeyEvent {
            origin: InputOrigin::Physical(SOURCE),
            pressed,
            keycode,
            time: 10,
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            state: 0,
        })
    };

    // First turn Lock on; the second press is the held action crossing the grab.
    Backend::on_host_input(&mut backend, &mut state, key(CAPS_LOCK, true));
    Backend::on_host_input(&mut backend, &mut state, key(CAPS_LOCK, false));
    assert_eq!(
        backend
            .core
            .xkb_state
            .0
            .serialize_mods(xkbcommon::xkb::STATE_MODS_LOCKED)
            & LOCK_MASK,
        LOCK_MASK,
        "Lock is on before the held press",
    );
    Backend::on_host_input(&mut backend, &mut state, key(CAPS_LOCK, true));
    assert!(state.key_down_by_device[&keyboard_id].contains_key(&CAPS_LOCK));

    process_dynamic_test_keyboard_grab(&mut backend, &mut state, CLIENT, keyboard_id, 2);
    let _ = kbd_map_drain(&mut peer);
    assert_eq!(
        state
            .xi_devices
            .device(keyboard_id)
            .unwrap()
            .attached_master,
        None
    );
    assert!(
        backend.floating_keyboard_states[&keyboard_id]
            .down_keys
            .contains(&CAPS_LOCK)
    );

    Backend::on_host_input(&mut backend, &mut state, key(CAPS_LOCK, false));
    assert_eq!(
        backend.floating_keyboard_states[&keyboard_id]
            .xkb_state
            .0
            .serialize_mods(xkbcommon::xkb::STATE_MODS_LOCKED)
            & LOCK_MASK,
        LOCK_MASK,
        "LockNoUnlock keeps the inherited Lock bit on floating release",
    );

    Backend::on_host_input(&mut backend, &mut state, key(LETTER_A, true));
    assert_eq!(
        backend.floating_keyboard_states[&keyboard_id]
            .xkb_state
            .0
            .serialize_mods(xkbcommon::xkb::STATE_MODS_LOCKED)
            & LOCK_MASK,
        LOCK_MASK,
        "the next floating key is cooked with Lock still enabled",
    );
    Backend::on_host_input(&mut backend, &mut state, key(LETTER_A, false));
    assert!(
        backend.floating_keyboard_states[&keyboard_id]
            .down_keys
            .is_empty()
    );

    process_dynamic_test_keyboard_ungrab(&mut backend, &mut state, CLIENT, keyboard_id, 3);
    let _ = kbd_map_drain(&mut peer);
    assert!(!state.xi2_keyboard_grabs.contains_key(&keyboard_id));
    assert!(!state.xi2_detached_masters.contains_key(&keyboard_id));
    assert!(!backend.floating_keyboard_states.contains_key(&keyboard_id));

    // Restore the original action and clear the master key/Lock state through host input.
    xkb_client_request(
        &mut state,
        &mut backend,
        CLIENT,
        136,
        9,
        &set_caps_lock_action(original_caps_action_value, original_caps_action_value[1]),
    );
    let _ = kbd_map_drain(&mut peer);
    for _ in 0..3 {
        if backend
            .core
            .xkb_state
            .0
            .serialize_mods(xkbcommon::xkb::STATE_MODS_LOCKED)
            & LOCK_MASK
            == 0
        {
            break;
        }
        Backend::on_host_input(&mut backend, &mut state, key(CAPS_LOCK, true));
        Backend::on_host_input(&mut backend, &mut state, key(CAPS_LOCK, false));
    }
    assert_eq!(
        backend
            .core
            .xkb_state
            .0
            .serialize_mods(xkbcommon::xkb::STATE_MODS_LOCKED)
            & LOCK_MASK,
        0,
        "cleanup restores the starting Lock state",
    );
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceRemoved { source_id: SOURCE },
    );
    assert_eq!(state_snapshot(&backend, &state), baseline);
    assert_eq!(
        backend.core.xkb_desc.acts[usize::from(CAPS_LOCK)],
        original_caps_action,
        "cleanup restores the original Caps action",
    );
}

#[test]
// Mutation killed: replay a held SA_LOCK_GROUP key with update_key(Down) when detaching, applying the relative lock twice.
fn xi_lockmods_held_group_lock_key_is_not_replayed_on_detach() {
    use yserver_core::{
        backend::Backend,
        core_loop::{HostInputEvent, InputOrigin},
        host_x11::HostKeyEvent,
        resources::ROOT_WINDOW,
        server::ServerState,
        xinput::{InputSourceId, XiFacetKind},
    };

    const CLIENT: u32 = 0xA743;
    const SOURCE: InputSourceId = InputSourceId(0xA7431);
    const GROUP_LOCK_KEY: u8 = 38;
    const LOCK_GROUP_ACTION: [u8; 8] = [0x06, 0, 1, 0, 0, 0, 0, 0];

    fn set_key_actions(keycode: u8, action_slots: usize, action: [u8; 8]) -> Vec<u8> {
        let mut body = vec![0u8; 32];
        body[0..2].copy_from_slice(&0x0100u16.to_le_bytes()); // XkbUseCoreKbd
        body[2..4].copy_from_slice(&0x0010u16.to_le_bytes()); // XkbKeyActionsMask
        body[6] = 8; // min_key_code
        body[7] = 255; // max_key_code
        body[14] = keycode; // first_key_act
        body[15] = 1; // one key
        body[16..18].copy_from_slice(&u16::try_from(action_slots).unwrap().to_le_bytes());
        body.extend_from_slice(&[u8::try_from(action_slots).unwrap(), 0, 0, 0]);
        for _ in 0..action_slots {
            body.extend_from_slice(&action);
        }
        body
    }

    fn state_snapshot(backend: &KmsBackend, state: &ServerState) -> String {
        format!(
            "{:?}",
            (
                (
                    state
                        .xi_devices
                        .devices()
                        .iter()
                        .map(|device| (
                            device.id,
                            device.source_id,
                            device.facet,
                            device.enabled,
                            device.attached_master,
                            device.buttons_down,
                            device.properties.clone(),
                        ))
                        .collect::<Vec<_>>(),
                    state.keys_down,
                    state.buttons_down,
                    state.key_down_by_device.clone(),
                    state.xi2_detached_masters.clone(),
                    state.floating_pointer_positions.clone(),
                    state.xi2_keyboard_grabs.clone(),
                ),
                (
                    state.clients[&CLIENT].xi2_masks.clone(),
                    state.clients[&CLIENT].xi1_event_classes.clone(),
                    state.clients[&CLIENT].xi1_window_event_classes.clone(),
                ),
                (
                    backend
                        .floating_keyboard_states
                        .keys()
                        .copied()
                        .collect::<Vec<_>>(),
                    backend.lock_filter_priv_by_device.clone(),
                    backend.core.xkb_desc.acts[usize::from(GROUP_LOCK_KEY)].clone(),
                    backend
                        .core
                        .xkb_state
                        .0
                        .serialize_layout(xkbcommon::xkb::STATE_LAYOUT_LOCKED),
                    backend.core.locked_group,
                ),
            )
        )
    }

    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    backend
        .core
        .xid_map
        .insert(backend.core.window_id, ROOT_WINDOW);
    let mut peer = kbd_map_client_id(&mut state, CLIENT);
    xkb_client_request(&mut state, &mut backend, CLIENT, 136, 0, &[1, 0, 0, 0]);
    assert_eq!(kbd_map_drain(&mut peer)[0], 1, "XkbUseExtension reply");
    let baseline = state_snapshot(&backend, &state);
    let original_rmlvo = backend.core.xkb_rmlvo.clone();

    // A second layout makes a duplicate relative LockGroup press observable.
    let multigroup_rmlvo = crate::kms::core::XkbRmlvo {
        layout: "us,ru".to_owned(),
        ..original_rmlvo.clone()
    };
    assert!(backend.core.recompile_keymap(&multigroup_rmlvo).is_some());
    assert_eq!(backend.core.keymap_group_count(), 2);
    let action_slots = backend.core.xkb_desc.keys[usize::from(GROUP_LOCK_KEY)].num_syms();
    assert!(
        action_slots >= 2,
        "the key has symbol slots in both layouts and levels"
    );
    xkb_client_request(
        &mut state,
        &mut backend,
        CLIENT,
        136,
        9,
        &set_key_actions(GROUP_LOCK_KEY, action_slots, LOCK_GROUP_ACTION),
    );
    assert!(
        backend.core.xkb_desc.acts[usize::from(GROUP_LOCK_KEY)]
            .as_ref()
            .unwrap()
            .iter()
            .all(|action| action[0] == crate::kms::xkb_desc::SA_LOCK_GROUP),
        "the real SetMap request assigns LockGroup(+1) at every key level",
    );
    let _ = kbd_map_drain(&mut peer);

    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceAdded(dynamic_test_device(SOURCE, true, false)),
    );
    let keyboard_id = state
        .xi_devices
        .facet(SOURCE, XiFacetKind::Keyboard)
        .expect("physical keyboard facet follows production add path");
    let key = |pressed| {
        HostInputEvent::Key(HostKeyEvent {
            origin: InputOrigin::Physical(SOURCE),
            pressed,
            keycode: GROUP_LOCK_KEY,
            time: 10,
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            state: 0,
        })
    };

    Backend::on_host_input(&mut backend, &mut state, key(true));
    assert_eq!(
        backend.core.locked_group, 1,
        "the attached press locks group 2"
    );
    assert!(state.key_down_by_device[&keyboard_id].contains_key(&GROUP_LOCK_KEY));

    process_dynamic_test_keyboard_grab(&mut backend, &mut state, CLIENT, keyboard_id, 2);
    let _ = kbd_map_drain(&mut peer);
    assert_eq!(
        state
            .xi_devices
            .device(keyboard_id)
            .unwrap()
            .attached_master,
        None,
        "XIGrabDevice floats the held keyboard",
    );
    assert_eq!(
        backend.floating_keyboard_states[&keyboard_id]
            .xkb_state
            .0
            .serialize_layout(xkbcommon::xkb::STATE_LAYOUT_LOCKED),
        1,
        "detachment inherits the one locked group without replaying LockGroup; master locked={:?}, floating cached={}",
        backend
            .core
            .xkb_state
            .0
            .serialize_layout(xkbcommon::xkb::STATE_LAYOUT_LOCKED),
        backend.floating_keyboard_states[&keyboard_id].locked_group,
    );

    Backend::on_host_input(&mut backend, &mut state, key(false));
    assert_eq!(
        backend.floating_keyboard_states[&keyboard_id]
            .xkb_state
            .0
            .serialize_layout(xkbcommon::xkb::STATE_LAYOUT_LOCKED),
        1,
        "releasing the held LockGroup key does not lock a second group",
    );
    assert!(
        state
            .key_down_by_device
            .get(&keyboard_id)
            .is_none_or(std::collections::HashMap::is_empty)
    );
    process_dynamic_test_keyboard_ungrab(&mut backend, &mut state, CLIENT, keyboard_id, 3);
    let _ = kbd_map_drain(&mut peer);
    assert!(!state.xi2_keyboard_grabs.contains_key(&keyboard_id));
    assert!(!state.xi2_detached_masters.contains_key(&keyboard_id));
    assert!(!backend.floating_keyboard_states.contains_key(&keyboard_id));

    // The master accepted the initial press but does not receive the floating
    // release. After reattachment, reconcile that master hold through real
    // host input, then use the same LockGroup action to return to group 1.
    Backend::on_host_input(&mut backend, &mut state, key(true));
    Backend::on_host_input(&mut backend, &mut state, key(false));
    Backend::on_host_input(&mut backend, &mut state, key(true));
    Backend::on_host_input(&mut backend, &mut state, key(false));
    assert_eq!(backend.core.locked_group, 0);
    assert_eq!(
        state.keys_down[usize::from(GROUP_LOCK_KEY / 8)] & (1 << (GROUP_LOCK_KEY % 8)),
        0
    );

    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceRemoved { source_id: SOURCE },
    );
    assert!(backend.core.recompile_keymap(&original_rmlvo).is_some());
    assert_eq!(state_snapshot(&backend, &state), baseline);
}

#[test]
// Mutation killed: treat the default action as LockNoUnlock and fail to clear its saved pre-press Lock bit.
fn xi_lockmods_default_caps_clears_saved_prepress_lock_after_keyboard_grab() {
    use yserver_core::{
        backend::Backend,
        core_loop::{HostInputEvent, InputOrigin, process_request},
        host_x11::HostKeyEvent,
        resources::ROOT_WINDOW,
        server::ServerState,
        xinput::{InputSourceId, XiFacetKind},
    };
    use yserver_protocol::x11::{ClientId, RequestHeader, SequenceNumber};

    const CLIENT: u32 = 0xA742;
    const SOURCE: InputSourceId = InputSourceId(0xA7421);
    const CAPS_LOCK: u8 = 66;
    const LOCK_MASK: u32 = 1 << 1;

    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    backend
        .core
        .xid_map
        .insert(backend.core.window_id, ROOT_WINDOW);
    let _peer = kbd_map_client_id(&mut state, CLIENT);
    process_request::process_request(
        &mut state,
        &mut backend,
        ClientId(CLIENT),
        SequenceNumber(1),
        RequestHeader {
            opcode: 137,
            data: 47,
            length_units: 2,
        },
        &[2, 0, 3, 0],
        None,
    )
    .expect("production XIQueryVersion request");
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceAdded(dynamic_test_device(SOURCE, true, false)),
    );
    let keyboard_id = state
        .xi_devices
        .facet(SOURCE, XiFacetKind::Keyboard)
        .expect("physical keyboard facet");
    let key = |pressed| {
        HostInputEvent::Key(HostKeyEvent {
            origin: InputOrigin::Physical(SOURCE),
            pressed,
            keycode: CAPS_LOCK,
            time: 10,
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            state: 0,
        })
    };

    // First press/release turns Caps on. A second press starts while
    // Lock is already set; Xorg saves that pre-press bit in the lock
    // filter (xkb/xkbActions.c:368-383) and clears only it on release.
    Backend::on_host_input(&mut backend, &mut state, key(true));
    Backend::on_host_input(&mut backend, &mut state, key(false));
    assert_eq!(
        backend
            .core
            .xkb_state
            .0
            .serialize_mods(xkbcommon::xkb::STATE_MODS_LOCKED)
            & LOCK_MASK,
        LOCK_MASK,
        "the first Caps cycle turns Lock on",
    );
    Backend::on_host_input(&mut backend, &mut state, key(true));
    assert!(state.key_down_by_device[&keyboard_id].contains_key(&CAPS_LOCK));
    assert_eq!(
        backend.lock_filter_priv_by_device[&keyboard_id][&CAPS_LOCK].pre_press_locked_mods,
        LOCK_MASK,
        "the floating release carries Xorg's pre-press Lock bit",
    );

    process_dynamic_test_keyboard_grab(&mut backend, &mut state, CLIENT, keyboard_id, 2);
    assert_eq!(
        state
            .xi_devices
            .device(keyboard_id)
            .unwrap()
            .attached_master,
        None
    );
    assert!(
        backend.floating_keyboard_states[&keyboard_id]
            .down_keys
            .contains(&CAPS_LOCK)
    );

    Backend::on_host_input(&mut backend, &mut state, key(false));
    let floating_locked_after_release = backend.floating_keyboard_states[&keyboard_id]
        .xkb_state
        .0
        .serialize_mods(xkbcommon::xkb::STATE_MODS_LOCKED);
    assert_eq!(
        floating_locked_after_release & LOCK_MASK,
        0,
        "floating release clears the saved pre-press Caps bit",
    );
    assert!(
        !backend.floating_keyboard_states[&keyboard_id]
            .down_keys
            .contains(&CAPS_LOCK)
    );
    assert!(
        state
            .key_down_by_device
            .get(&keyboard_id)
            .is_none_or(std::collections::HashMap::is_empty)
    );
    assert!(
        !backend
            .lock_filter_priv_by_device
            .contains_key(&keyboard_id)
    );
    assert!(
        backend.floating_keyboard_states[&keyboard_id]
            .lock_filter_priv_by_key
            .is_empty()
    );
    assert_eq!(
        state
            .xi_devices
            .device(keyboard_id)
            .unwrap()
            .attached_master,
        None
    );
    assert!(state.xi2_keyboard_grabs.contains_key(&keyboard_id));
    assert!(state.sync_pending.is_empty());
    assert_ne!(
        state.keys_down[usize::from(CAPS_LOCK / 8)] & (1u8 << (CAPS_LOCK % 8)),
        0,
        "detached release leaves the master key bitmap unchanged",
    );
}

#[test]
fn record_keeps_pointer_events_enqueued_while_master_is_synchronously_frozen() {
    use yserver_core::{
        backend::Backend,
        core_loop::{DeviceInfo, HostInputEvent, InputOrigin, process_request},
        resources::ROOT_WINDOW,
        server::{QueuedInputEvent, ServerState},
        xinput::{InputCapabilities, InputSourceId, XiFacetKind},
    };
    use yserver_protocol::x11::{ClientId, RequestHeader, SequenceNumber};

    const RECORDER: u32 = 5;
    const GRABBER: u32 = 0xA734;
    const XTEST_CLIENT: u32 = 0xA735;
    const SOURCE: InputSourceId = InputSourceId(0xA7331);
    const XI_BUTTON_PRESS_RELEASE: u32 = (1 << 4) | (1 << 5);

    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    backend
        .core
        .xid_map
        .insert(backend.core.window_id, ROOT_WINDOW);
    let mut recorder_peer = kbd_map_client_id(&mut state, RECORDER);
    let _grab_peer = kbd_map_client_id(&mut state, GRABBER);
    let _xtest_peer = kbd_map_client_id(&mut state, XTEST_CLIENT);

    // RECORD CreateContext selects core ButtonPress..ButtonRelease,
    // followed by EnableContext, through the production dispatcher.
    let mut create = Vec::new();
    for word in [1u32, 0, 1, 1, 2] {
        create.extend_from_slice(&word.to_le_bytes());
    }
    create.extend_from_slice(&[0; 18]);
    create.extend_from_slice(&[2, 5, 0, 0, 0, 0]);
    kbd_map_request(&mut state, &mut backend, 154, 1, &create);
    kbd_map_request(&mut state, &mut backend, 154, 5, &1u32.to_le_bytes());
    let _ = kbd_map_drain(&mut recorder_peer);

    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceAdded(DeviceInfo {
            source_id: SOURCE,
            enabled: true,
            resume_key: None,
            capabilities: InputCapabilities {
                keyboard: false,
                pointer: true,
                touch: false,
            },
            name: "RECORD synchronous-grab mouse".to_owned(),
            device_node: "/dev/input/event-record-sync".to_owned(),
            sysname: "event-record-sync".to_owned(),
            vendor_id: 1,
            product_id: 3,
            is_touchpad: false,
            config: Default::default(),
        }),
    );
    let pointer_id = state
        .xi_devices
        .facet(SOURCE, XiFacetKind::PointerTouch)
        .expect("physical mouse pointer facet");

    let mut grab = Vec::with_capacity(24);
    grab.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    grab.extend_from_slice(&0u32.to_le_bytes()); // current time
    grab.extend_from_slice(&0u32.to_le_bytes()); // no cursor
    grab.extend_from_slice(&2u16.to_le_bytes()); // master pointer
    grab.extend_from_slice(&[0, 1, 0, 0]); // sync master, async paired, owner_events=false
    grab.extend_from_slice(&1u16.to_le_bytes()); // one mask word
    grab.extend_from_slice(&XI_BUTTON_PRESS_RELEASE.to_le_bytes());
    process_request::process_request(
        &mut state,
        &mut backend,
        ClientId(GRABBER),
        SequenceNumber(1),
        RequestHeader {
            opcode: 137,
            data: 51,
            length_units: 7,
        },
        &grab,
        None,
    )
    .expect("production synchronous XIGrabDevice");
    assert!(state.active_pointer_grab.is_some_and(|active| {
        active.owner == ClientId(GRABBER) && active.xi2_mask == u64::from(XI_BUTTON_PRESS_RELEASE)
    }));
    assert!(state.xi1_frozen[&2].frozen(), "sync grab freezes master 2");

    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::PointerButton {
            origin: InputOrigin::Physical(SOURCE),
            button: 0x110, // BTN_LEFT -> core button 1
            pressed: true,
            time: 20,
        },
    );

    let mut fake_button = |event_type: u8| {
        let mut body = Vec::with_capacity(28);
        body.extend_from_slice(&[event_type, 1, 0, 0]); // type, detail, pad
        body.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
        body.extend_from_slice(&0u32.to_le_bytes()); // current root
        body.extend_from_slice(&[0; 8]);
        body.extend_from_slice(&0i16.to_le_bytes()); // root x
        body.extend_from_slice(&0i16.to_le_bytes()); // root y
        body.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(body.len(), 28);
        process_request::process_request(
            &mut state,
            &mut backend,
            ClientId(XTEST_CLIENT),
            SequenceNumber(u16::from(event_type)),
            RequestHeader {
                opcode: 146,
                data: 2, // XTEST FakeInput
                length_units: 8,
            },
            &body,
            None,
        )
        .expect("production XTEST FakeInput request")
    };
    fake_button(4); // XTestFakeButtonEvent press
    fake_button(5); // XTestFakeButtonEvent release

    let bytes = kbd_map_drain_until(&mut recorder_peer, |bytes| {
        let mut at = 0usize;
        let mut found = 0;
        while at + 32 <= bytes.len() {
            let words = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
            let len = 32 + words * 4;
            if at + len > bytes.len() {
                break;
            }
            if bytes[at] == 1 && bytes[at + 1] == 0 && len >= 34 && matches!(bytes[at + 32], 4 | 5)
            {
                found += 1;
            }
            at += len;
        }
        found >= 3
    });
    let mut button_records = Vec::new();
    let mut at = 0usize;
    while at + 32 <= bytes.len() {
        let words = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
        let len = 32 + words * 4;
        assert!(at + len <= bytes.len(), "complete RECORD stream element");
        if bytes[at] == 1 && bytes[at + 1] == 0 && len >= 34 {
            let event = &bytes[at + 32..at + len];
            if matches!(event[0], 4 | 5) {
                button_records.push((event[0], event[1]));
            }
        }
        at += len;
    }

    assert_eq!(
        state.sync_pending.len(),
        3,
        "physical and XTEST edges are frozen"
    );
    assert_eq!(
        button_records,
        [(4, 1), (4, 1), (5, 1)],
        "RECORD gets physical press plus both XTEST edges while frozen",
    );
    assert_eq!(
        state.sync_pending.len(),
        3,
        "all three input edges are queued"
    );
    let queued: Vec<_> = state
        .sync_pending
        .iter()
        .map(|pending| {
            assert_eq!(pending.device, 2, "master 2 controls the queue");
            match &pending.event {
                QueuedInputEvent::HostPointer(event) => (event.origin, event.kind, event.detail),
                other => panic!("unexpected queued input: {other:?}"),
            }
        })
        .collect();
    assert_eq!(
        queued,
        [
            (
                InputOrigin::Physical(SOURCE),
                yserver_core::host_x11::PointerEventKind::ButtonPress,
                1
            ),
            (
                InputOrigin::XTest(4),
                yserver_core::host_x11::PointerEventKind::ButtonPress,
                1
            ),
            (
                InputOrigin::XTest(4),
                yserver_core::host_x11::PointerEventKind::ButtonRelease,
                1
            ),
        ]
    );
    assert_eq!(state.buttons_down, 1, "physical Button1 stays held");
    assert_eq!(state.xi_devices.device(pointer_id).unwrap().buttons_down, 1);
    assert_eq!(
        state
            .xi_devices
            .device(yserver_core::xinput::DEVICEID_XTEST_POINTER)
            .unwrap()
            .buttons_down,
        0,
        "the queued XTEST click releases its own source button",
    );
    assert!(state.xi1_frozen[&2].frozen());
    assert!(
        state
            .active_pointer_grab
            .is_some_and(|active| active.owner == ClientId(GRABBER))
    );
    assert_eq!(
        state.xi_devices.device(pointer_id).unwrap().attached_master,
        Some(2)
    );
}

#[test]
fn xi_slave_switch_scroll_stop_next_motion_and_query_share_current_source_value() {
    use yserver_core::{
        backend::Backend,
        core_loop::{DeviceInfo, HostInputEvent, InputOrigin, process_request},
        resources::ROOT_WINDOW,
        server::ServerState,
        xinput::{InputCapabilities, InputSourceId, XiFacetKind},
    };
    use yserver_protocol::x11::{ClientId, RequestHeader, SequenceNumber};

    const CLIENT: u32 = 0xA730;
    const MOUSE_A: InputSourceId = InputSourceId(0xA7301);
    const MOUSE_B: InputSourceId = InputSourceId(0xA7302);
    const XI_DEVICE_CHANGED: u32 = 1 << 1;
    const XI_BUTTON_PRESS: u32 = 1 << 4;
    const XI_MOTION: u32 = 1 << 6;

    fn xi_scroll_values(bytes: &[u8]) -> Vec<(u16, u16, i32)> {
        let mut values = Vec::new();
        let mut offset = 0;
        while offset < bytes.len() {
            assert_eq!(bytes[offset], 35, "GenericEvent stream");
            let units = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap());
            let evtype = u16::from_le_bytes([bytes[offset + 8], bytes[offset + 9]]);
            let deviceid = u16::from_le_bytes([bytes[offset + 10], bytes[offset + 11]]);
            if evtype == 6 && units == 28 {
                let sourceid =
                    u16::from_le_bytes(bytes[offset + 52..offset + 54].try_into().unwrap());
                let value =
                    i32::from_le_bytes(bytes[offset + 136..offset + 140].try_into().unwrap());
                values.push((deviceid, sourceid, value));
            }
            offset += 32 + units as usize * 4;
        }
        assert_eq!(offset, bytes.len(), "complete XI2 event stream");
        values
    }

    fn master_scroll_value(bytes: &[u8]) -> i32 {
        assert_eq!(bytes[0], 1, "XIQueryDevice reply");
        let devices = u16::from_le_bytes([bytes[8], bytes[9]]) as usize;
        let mut offset = 32;
        for _ in 0..devices {
            let device = u16::from_le_bytes([bytes[offset], bytes[offset + 1]]);
            let classes = u16::from_le_bytes([bytes[offset + 6], bytes[offset + 7]]) as usize;
            let name_len = u16::from_le_bytes([bytes[offset + 8], bytes[offset + 9]]) as usize;
            let mut class_offset = offset + 12 + name_len;
            while !class_offset.is_multiple_of(4) {
                class_offset += 1;
            }
            for _ in 0..classes {
                let class = u16::from_le_bytes([bytes[class_offset], bytes[class_offset + 1]]);
                let units =
                    u16::from_le_bytes([bytes[class_offset + 2], bytes[class_offset + 3]]) as usize;
                let axis = u16::from_le_bytes([bytes[class_offset + 6], bytes[class_offset + 7]]);
                if device == 2 && class == 2 && axis == 2 {
                    return i32::from_le_bytes(
                        bytes[class_offset + 28..class_offset + 32]
                            .try_into()
                            .unwrap(),
                    );
                }
                class_offset += units * 4;
            }
            offset = class_offset;
        }
        panic!("master pointer vertical valuator missing from XIQueryDevice");
    }

    fn request(
        state: &mut ServerState,
        backend: &mut KmsBackend,
        sequence: u16,
        minor: u8,
        body: &[u8],
    ) {
        process_request::process_request(
            state,
            backend,
            ClientId(CLIENT),
            SequenceNumber(sequence),
            RequestHeader {
                opcode: 137,
                data: minor,
                length_units: u32::try_from((body.len() + 4) / 4).unwrap(),
            },
            body,
            None,
        )
        .expect("production XI2 request");
    }

    let source_info = |source_id: InputSourceId, name: &str| DeviceInfo {
        source_id,
        enabled: true,
        resume_key: None,
        capabilities: InputCapabilities {
            keyboard: false,
            pointer: true,
            touch: false,
        },
        name: name.to_owned(),
        device_node: format!("/dev/input/{name}"),
        sysname: name.to_owned(),
        vendor_id: 1,
        product_id: source_id.0 as u32,
        is_touchpad: false,
        config: Default::default(),
    };
    let motion = |source_id, x, y, time| HostInputEvent::PointerMotion {
        origin: InputOrigin::Physical(source_id),
        x,
        y,
        time,
        relative: false,
        dx: 0,
        dy: 0,
        motion_delta: None,
    };
    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    backend
        .core
        .xid_map
        .insert(backend.core.window_id, ROOT_WINDOW);
    let mut peer = kbd_map_client_id(&mut state, CLIENT);
    request(&mut state, &mut backend, 1, 47, &[2, 0, 3, 0]);
    let version_reply = kbd_map_drain(&mut peer);
    assert_eq!(version_reply[0], 1, "XIQueryVersion reply");
    let mut select = Vec::new();
    select.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    select.extend_from_slice(&1u16.to_le_bytes());
    select.extend_from_slice(&[0; 2]);
    select.extend_from_slice(&2u16.to_le_bytes());
    select.extend_from_slice(&1u16.to_le_bytes());
    select.extend_from_slice(&(XI_DEVICE_CHANGED | XI_BUTTON_PRESS | XI_MOTION).to_le_bytes());
    request(&mut state, &mut backend, 2, 46, &select);

    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceAdded(source_info(MOUSE_A, "scroll-mouse-a")),
    );
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceAdded(source_info(MOUSE_B, "scroll-mouse-b")),
    );
    let mouse_a = state
        .xi_devices
        .facet(MOUSE_A, XiFacetKind::PointerTouch)
        .unwrap();
    let mouse_b = state
        .xi_devices
        .facet(MOUSE_B, XiFacetKind::PointerTouch)
        .unwrap();
    let _ = kbd_map_drain(&mut peer);

    Backend::on_host_input(&mut backend, &mut state, motion(MOUSE_A, 10, 20, 1));
    let _ = kbd_map_drain(&mut peer);
    for n in 0..10 {
        for pressed in [true, false] {
            Backend::on_host_input(
                &mut backend,
                &mut state,
                HostInputEvent::PointerButton {
                    origin: InputOrigin::Physical(MOUSE_A),
                    button: 0x181,
                    pressed,
                    time: 2 + n,
                },
            );
        }
    }
    let _ = kbd_map_drain(&mut peer);
    assert_eq!(
        state.xi_devices.device(mouse_a).unwrap().scroll_axis_values,
        [10, 0]
    );

    Backend::on_host_input(&mut backend, &mut state, motion(MOUSE_B, 30, 40, 20));
    let switched = kbd_map_drain(&mut peer);
    assert_eq!(state.xi_last_slave(2), Some(mouse_b));
    let query = |backend: &mut KmsBackend,
                 state: &mut ServerState,
                 peer: &mut std::os::unix::net::UnixStream,
                 sequence| {
        request(state, backend, sequence, 48, &[2, 0, 0, 0]);
        master_scroll_value(&kbd_map_drain(peer))
    };
    let after_switch = query(&mut backend, &mut state, &mut peer, 2);

    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::PointerButton {
            origin: InputOrigin::Physical(MOUSE_B),
            button: 0x181,
            pressed: true,
            time: 21,
        },
    );
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::PointerButton {
            origin: InputOrigin::Physical(MOUSE_B),
            button: 0x181,
            pressed: false,
            time: 22,
        },
    );
    let first_b_scroll = xi_scroll_values(&kbd_map_drain(&mut peer));
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::PointerScrollStop {
            origin: InputOrigin::Physical(MOUSE_B),
            time: 23,
        },
    );
    let stop = xi_scroll_values(&kbd_map_drain(&mut peer));
    let after_stop = query(&mut backend, &mut state, &mut peer, 3);
    for pressed in [true, false] {
        Backend::on_host_input(
            &mut backend,
            &mut state,
            HostInputEvent::PointerButton {
                origin: InputOrigin::Physical(MOUSE_B),
                button: 0x181,
                pressed,
                time: 24,
            },
        );
    }
    let next_b_scroll = xi_scroll_values(&kbd_map_drain(&mut peer));
    let after_next_scroll = query(&mut backend, &mut state, &mut peer, 4);

    let mut offset = 0;
    let mut saw_switch = false;
    while offset < switched.len() {
        assert_eq!(switched[offset], 35, "XI2 GenericEvent");
        let units =
            u32::from_le_bytes(switched[offset + 4..offset + 8].try_into().unwrap()) as usize;
        let evtype = u16::from_le_bytes([switched[offset + 8], switched[offset + 9]]);
        let deviceid = u16::from_le_bytes([switched[offset + 10], switched[offset + 11]]);
        if evtype == 1 {
            saw_switch |= deviceid == 2
                && u16::from_le_bytes([switched[offset + 18], switched[offset + 19]]) == mouse_b
                && switched[offset + 20] == 1;
        }
        offset += 32 + units * 4;
    }
    assert!(
        saw_switch,
        "KMS motion emits SlaveSwitch DeviceChanged for mouse B"
    );
    assert_eq!(after_switch, 0, "XIQueryDevice reports B's switch baseline");
    assert_eq!(first_b_scroll, vec![(2, mouse_b, 1)]);
    assert_eq!(
        stop,
        vec![(2, mouse_b, 1), (2, mouse_b, 0)],
        "stop keeps both of B's master scroll-axis baselines"
    );
    assert_eq!(after_stop, 1, "query and PointerScrollStop agree");
    assert_eq!(next_b_scroll, vec![(2, mouse_b, 2)]);
    assert_eq!(
        after_next_scroll, 2,
        "XIQueryDevice follows the next B motion"
    );
    assert_eq!(
        state.xi_devices.device(mouse_a).unwrap().scroll_axis_values,
        [10, 0]
    );
    assert_eq!(
        state.xi_devices.device(mouse_b).unwrap().scroll_axis_values,
        [2, 0]
    );
    assert_eq!(state.scroll_axis_value, [2, 0]);
    assert_eq!(state.buttons_down, 0);
    assert!(state.sync_pending.is_empty());
    assert!(state.unpublished_pointer_buttons_down.is_empty());
    assert_eq!(state.xi_devices.device(mouse_a).unwrap().buttons_down, 0);
    assert_eq!(state.xi_devices.device(mouse_b).unwrap().buttons_down, 0);
}

#[test]
fn xi_slave_switch_kms_suspend_and_removal_clear_last_source() {
    use yserver_core::{
        backend::Backend,
        core_loop::{DeviceInfo, HostInputEvent, InputOrigin},
        resources::ROOT_WINDOW,
        server::ServerState,
        xinput::{InputCapabilities, InputSourceId, XiFacetKind},
    };
    use yserver_protocol::x11::ClientId;

    const CLIENT: u32 = 0xA73;
    const CHANGE_AND_MOTION: u64 = (1 << 1) | (1 << 6);
    let info = |source_id, name: &str| DeviceInfo {
        source_id: InputSourceId(source_id),
        enabled: true,
        resume_key: None,
        capabilities: InputCapabilities {
            keyboard: false,
            pointer: true,
            touch: false,
        },
        name: name.to_owned(),
        device_node: format!("/dev/input/event-{name}"),
        sysname: format!("event-{name}"),
        vendor_id: 1,
        product_id: source_id as u32,
        is_touchpad: false,
        config: Default::default(),
    };
    let motion = |source_id, x, y, time| HostInputEvent::PointerMotion {
        origin: InputOrigin::Physical(InputSourceId(source_id)),
        x,
        y,
        time,
        relative: false,
        dx: 0,
        dy: 0,
        motion_delta: None,
    };
    let events = |bytes: &[u8]| {
        let mut result = Vec::new();
        let mut offset = 0;
        while offset + 32 <= bytes.len() {
            if bytes[offset] & 0x7f == 35 {
                let event_type = u16::from_le_bytes([bytes[offset + 8], bytes[offset + 9]]);
                let device_id = u16::from_le_bytes([bytes[offset + 10], bytes[offset + 11]]);
                let source_offset = if event_type == 1 { 18 } else { 52 };
                let source_id = u16::from_le_bytes([
                    bytes[offset + source_offset],
                    bytes[offset + source_offset + 1],
                ]);
                let reason = if event_type == 1 {
                    bytes[offset + 20]
                } else {
                    0
                };
                result.push((event_type, device_id, source_id, reason));
                let units = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap());
                offset += 32 + units as usize * 4;
            } else {
                offset += 32;
            }
        }
        assert_eq!(offset, bytes.len(), "complete KMS XI2 event stream");
        result
    };

    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    backend
        .core
        .xid_map
        .insert(backend.core.window_id, ROOT_WINDOW);
    let mut peer = kbd_map_client_id(&mut state, CLIENT);
    state
        .clients
        .get_mut(&CLIENT)
        .unwrap()
        .xi2_masks
        .insert((ROOT_WINDOW, 2), CHANGE_AND_MOTION);
    state.xi2_client_versions.insert(ClientId(CLIENT), (2, 2));

    let razer = info(0xA731, "Razer");
    let hyperx = info(0xA732, "HyperX");
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceAdded(razer.clone()),
    );
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceAdded(hyperx.clone()),
    );
    let razer_id = state
        .xi_devices
        .facet(razer.source_id, XiFacetKind::PointerTouch)
        .unwrap();
    let hyperx_id = state
        .xi_devices
        .facet(hyperx.source_id, XiFacetKind::PointerTouch)
        .unwrap();
    assert_eq!((razer_id, hyperx_id), (6, 7));

    Backend::on_host_input(
        &mut backend,
        &mut state,
        motion(razer.source_id.0, 20, 30, 1),
    );
    let first = events(&kbd_map_drain(&mut peer));
    assert_eq!(first[0], (1, 2, razer_id, 1));
    assert_eq!(state.xi_last_slave(2), Some(razer_id));

    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceSuspended {
            source_id: razer.source_id,
        },
    );
    assert_eq!(state.xi_last_slave(2), None, "suspend clears lastSlave");
    assert!(!state.xi_devices.source(razer.source_id).unwrap().enabled);
    assert!(state.xi_devices.device(razer_id).is_some());

    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceResumed(razer.clone()),
    );
    Backend::on_host_input(
        &mut backend,
        &mut state,
        motion(razer.source_id.0, 40, 50, 2),
    );
    let resumed = events(&kbd_map_drain(&mut peer));
    assert_eq!(resumed[0], (1, 2, razer_id, 1));
    assert_eq!(state.xi_last_slave(2), Some(razer_id));

    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceRemoved {
            source_id: razer.source_id,
        },
    );
    assert_eq!(state.xi_last_slave(2), None, "removal clears lastSlave");
    assert!(state.xi_devices.source(razer.source_id).is_none());
    assert!(state.xi_devices.device(razer_id).is_none());

    let replacement = info(0xA733, "replacement");
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceAdded(replacement.clone()),
    );
    let replacement_id = state
        .xi_devices
        .facet(replacement.source_id, XiFacetKind::PointerTouch)
        .unwrap();
    assert_eq!(replacement_id, razer_id, "the released XI ID is reused");
    Backend::on_host_input(
        &mut backend,
        &mut state,
        motion(replacement.source_id.0, 60, 70, 3),
    );
    let replacement_events = events(&kbd_map_drain(&mut peer));
    assert_eq!(replacement_events[0], (1, 2, replacement_id, 1));
    assert_eq!(state.xi_last_slave(2), Some(replacement_id));
    assert!(state.xi_devices.source(hyperx.source_id).is_some());
    assert!(state.xi_devices.device(hyperx_id).unwrap().enabled);
    assert_eq!(
        state
            .xi_devices
            .device(hyperx_id)
            .unwrap()
            .scroll_axis_values,
        [0, 0]
    );
    assert_eq!(state.xi_devices.devices().len(), 6);
    assert!(state.pending_xi_device_removals.is_empty());
    assert_eq!(state.buttons_down, 0);
    assert!(state.sync_pending.is_empty());
    assert!(state.unpublished_pointer_buttons_down.is_empty());
    assert!(state.unpublished_keyboard_keys_down.is_empty());
    assert!(backend.core.pending_pointer_events.is_empty());
}

fn xi_master_classes_dispatch_request(
    state: &mut yserver_core::server::ServerState,
    backend: &mut KmsBackend,
    peer: &mut std::os::unix::net::UnixStream,
    client: u32,
    sequence: u16,
    minor: u8,
    body: &[u8],
) -> Vec<u8> {
    use yserver_core::core_loop::process_request;
    use yserver_protocol::x11::{ClientId, RequestHeader, SequenceNumber};

    process_request::process_request(
        state,
        backend,
        ClientId(client),
        SequenceNumber(sequence),
        RequestHeader {
            opcode: 137,
            data: minor,
            length_units: u32::try_from(1 + body.len().div_ceil(4)).unwrap(),
        },
        body,
        None,
    )
    .expect("XI request through the core dispatcher");
    kbd_map_drain(peer)
}

#[derive(Debug, PartialEq, Eq)]
struct XiMasterClassSummary {
    class_types: Vec<u16>,
    class_source_ids: Vec<u16>,
    button_count: Option<u16>,
    scroll_flags: Vec<u32>,
}

fn xi_master_class_summary(reply: &[u8], target_id: u16) -> XiMasterClassSummary {
    assert_eq!(reply.first(), Some(&1), "XIQueryDevice reply: {reply:?}");
    let num_devices = usize::from(u16::from_le_bytes([reply[8], reply[9]]));
    let mut device_offset = 32;
    for _ in 0..num_devices {
        let device_id = u16::from_le_bytes([reply[device_offset], reply[device_offset + 1]]);
        let num_classes = usize::from(u16::from_le_bytes([
            reply[device_offset + 6],
            reply[device_offset + 7],
        ]));
        let name_len = usize::from(u16::from_le_bytes([
            reply[device_offset + 8],
            reply[device_offset + 9],
        ]));
        let mut class_offset = device_offset + 12 + name_len;
        while !class_offset.is_multiple_of(4) {
            class_offset += 1;
        }
        let mut class_types = Vec::with_capacity(num_classes);
        let mut class_source_ids = Vec::with_capacity(num_classes);
        let mut button_count = None;
        let mut scroll_flags = Vec::new();
        for _ in 0..num_classes {
            let class_type = u16::from_le_bytes([reply[class_offset], reply[class_offset + 1]]);
            let class_units = usize::from(u16::from_le_bytes([
                reply[class_offset + 2],
                reply[class_offset + 3],
            ]));
            class_types.push(class_type);
            class_source_ids.push(u16::from_le_bytes([
                reply[class_offset + 4],
                reply[class_offset + 5],
            ]));
            if class_type == 1 {
                button_count = Some(u16::from_le_bytes([
                    reply[class_offset + 6],
                    reply[class_offset + 7],
                ]));
            } else if class_type == 3 {
                scroll_flags.push(u32::from_le_bytes(
                    reply[class_offset + 12..class_offset + 16]
                        .try_into()
                        .unwrap(),
                ));
            }
            class_offset += class_units * 4;
        }
        if device_id == target_id {
            return XiMasterClassSummary {
                class_types,
                class_source_ids,
                button_count,
                scroll_flags,
            };
        }
        device_offset = class_offset;
    }
    panic!("XIQueryDevice reply omitted device {target_id}");
}

fn xi_master_changed_summaries(bytes: &[u8]) -> Vec<(u16, u16, u8, XiMasterClassSummary)> {
    let mut result = Vec::new();
    let mut offset = 0;
    while offset + 32 <= bytes.len() {
        assert_eq!(bytes[offset], 35, "XI2 GenericEvent stream");
        let units = usize::try_from(u32::from_le_bytes(
            bytes[offset + 4..offset + 8].try_into().unwrap(),
        ))
        .unwrap();
        let event_len = 32 + units * 4;
        assert!(
            offset + event_len <= bytes.len(),
            "complete DeviceChanged event"
        );
        assert_eq!(
            u16::from_le_bytes([bytes[offset + 8], bytes[offset + 9]]),
            1,
            "XI_DeviceChanged"
        );
        let num_classes = usize::from(u16::from_le_bytes([bytes[offset + 16], bytes[offset + 17]]));
        let mut class_offset = offset + 32;
        let mut summary = XiMasterClassSummary {
            class_types: Vec::with_capacity(num_classes),
            class_source_ids: Vec::with_capacity(num_classes),
            button_count: None,
            scroll_flags: Vec::new(),
        };
        for _ in 0..num_classes {
            let class_type = u16::from_le_bytes([bytes[class_offset], bytes[class_offset + 1]]);
            let class_units = usize::from(u16::from_le_bytes([
                bytes[class_offset + 2],
                bytes[class_offset + 3],
            ]));
            summary.class_types.push(class_type);
            summary.class_source_ids.push(u16::from_le_bytes([
                bytes[class_offset + 4],
                bytes[class_offset + 5],
            ]));
            if class_type == 1 {
                summary.button_count = Some(u16::from_le_bytes([
                    bytes[class_offset + 6],
                    bytes[class_offset + 7],
                ]));
            } else if class_type == 3 {
                summary.scroll_flags.push(u32::from_le_bytes(
                    bytes[class_offset + 12..class_offset + 16]
                        .try_into()
                        .unwrap(),
                ));
            }
            class_offset += class_units * 4;
        }
        assert_eq!(class_offset, offset + event_len, "class block fills event");
        result.push((
            u16::from_le_bytes([bytes[offset + 10], bytes[offset + 11]]),
            u16::from_le_bytes([bytes[offset + 18], bytes[offset + 19]]),
            bytes[offset + 20],
            summary,
        ));
        offset += event_len;
    }
    assert_eq!(offset, bytes.len(), "complete XI2 event stream");
    result
}

#[test]
fn xi_master_classes_initial_pointer_uses_core_pointer_shape_and_own_sourceid() {
    // Kills an encoder mutation that always gives master 2 the physical
    // pointer class set instead of CorePointerProc's initial classes.
    use yserver_core::server::ServerState;

    const CLIENT: u32 = 0xA740;
    let mut state = ServerState::new();
    let mut backend = KmsBackend::for_tests();
    let mut peer = kbd_map_client_id(&mut state, CLIENT);
    let before_ids: Vec<_> = state.xi_devices.devices().iter().map(|d| d.id).collect();
    let before_properties: HashMap<_, _> = state
        .xi_devices
        .devices()
        .iter()
        .map(|device| (device.id, device.properties.clone()))
        .collect();
    let before_selections = (
        state.clients[&CLIENT].event_masks.clone(),
        state.clients[&CLIENT].xi2_masks.clone(),
        state.clients[&CLIENT].xi1_event_classes.clone(),
        state.clients[&CLIENT].xi1_window_event_classes.clone(),
    );
    let before_held = (
        state.keys_down,
        state.buttons_down,
        state
            .xi_devices
            .devices()
            .iter()
            .map(|device| (device.id, device.buttons_down))
            .collect::<Vec<_>>(),
    );
    let before_detached = state.xi2_detached_masters.clone();
    let before_floating = state.floating_pointer_positions.clone();

    let pointer = xi_master_classes_dispatch_request(
        &mut state,
        &mut backend,
        &mut peer,
        CLIENT,
        1,
        48,
        &[2, 0, 0, 0],
    );
    assert_eq!(
        xi_master_class_summary(&pointer, 2),
        XiMasterClassSummary {
            class_types: vec![1, 2, 2],
            class_source_ids: vec![2, 2, 2],
            button_count: Some(10),
            scroll_flags: vec![],
        },
        "a fresh master pointer uses CorePointerProc classes sourced from itself"
    );

    let keyboard = xi_master_classes_dispatch_request(
        &mut state,
        &mut backend,
        &mut peer,
        CLIENT,
        2,
        48,
        &[3, 0, 0, 0],
    );
    assert_eq!(
        xi_master_class_summary(&keyboard, 3),
        XiMasterClassSummary {
            class_types: vec![0],
            class_source_ids: vec![3],
            button_count: None,
            scroll_flags: vec![],
        },
        "a fresh master keyboard keeps its own initial key-class sourceid"
    );

    assert_eq!(
        state
            .xi_devices
            .devices()
            .iter()
            .map(|d| d.id)
            .collect::<Vec<_>>(),
        before_ids,
        "read-only queries do not change registry membership"
    );
    for device in state.xi_devices.devices() {
        assert_eq!(device.properties, before_properties[&device.id]);
    }
    assert_eq!(
        (
            state.keys_down,
            state.buttons_down,
            state
                .xi_devices
                .devices()
                .iter()
                .map(|device| (device.id, device.buttons_down))
                .collect::<Vec<_>>(),
        ),
        before_held
    );
    assert_eq!(state.xi2_detached_masters, before_detached);
    assert_eq!(state.floating_pointer_positions, before_floating);
    assert_eq!(
        (
            state.clients[&CLIENT].event_masks.clone(),
            state.clients[&CLIENT].xi2_masks.clone(),
            state.clients[&CLIENT].xi1_event_classes.clone(),
            state.clients[&CLIENT].xi1_window_event_classes.clone(),
        ),
        before_selections
    );
}

#[test]
fn xi_master_classes_store_last_slave_shape_after_removal_then_xtest_switch() {
    // Kills deriving class sourceid from lastSlave after it is cleared,
    // and kills encoding master 2 with a fixed physical class shape.
    use yserver_core::{
        backend::Backend,
        core_loop::{DeviceInfo, HostInputEvent, InputOrigin},
        resources::ROOT_WINDOW,
        server::ServerState,
        xinput::{InputCapabilities, InputSourceId, XiFacetKind},
    };

    const CLIENT: u32 = 0xA741;
    const SOURCE: InputSourceId = InputSourceId(0xA741);
    const DEVICE_CHANGED_MASK: u32 = 1 << 1;
    let mut state = ServerState::new();
    let mut backend = KmsBackend::for_tests();
    backend
        .core
        .xid_map
        .insert(backend.core.window_id, ROOT_WINDOW);
    let mut peer = kbd_map_client_id(&mut state, CLIENT);
    let initial_registry: Vec<_> = state
        .xi_devices
        .devices()
        .iter()
        .map(|device| {
            (
                device.id,
                device.enabled,
                device.session_enabled,
                device.client_disabled,
                device.source_id,
                device.facet,
                device.attached_master,
                device.properties.clone(),
            )
        })
        .collect();
    let initial_properties: HashMap<_, _> = state
        .xi_devices
        .devices()
        .iter()
        .map(|device| (device.id, device.properties.clone()))
        .collect();
    let initial_held = (
        state.keys_down,
        state.buttons_down,
        state
            .xi_devices
            .devices()
            .iter()
            .map(|device| (device.id, device.buttons_down))
            .collect::<Vec<_>>(),
    );
    let initial_detached = state.xi2_detached_masters.clone();
    let initial_floating = state.floating_pointer_positions.clone();
    let initial_selections = (
        state.clients[&CLIENT].event_masks.clone(),
        state.clients[&CLIENT].xi2_masks.clone(),
        state.clients[&CLIENT].xi1_event_classes.clone(),
        state.clients[&CLIENT].xi1_window_event_classes.clone(),
    );

    let mut select = Vec::new();
    select.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    select.extend_from_slice(&1u16.to_le_bytes());
    select.extend_from_slice(&[0; 2]);
    select.extend_from_slice(&2u16.to_le_bytes());
    select.extend_from_slice(&1u16.to_le_bytes());
    select.extend_from_slice(&DEVICE_CHANGED_MASK.to_le_bytes());
    let selection_result = xi_master_classes_dispatch_request(
        &mut state,
        &mut backend,
        &mut peer,
        CLIENT,
        1,
        46,
        &select,
    );
    let initial_bootstrap = xi_master_changed_summaries(&selection_result);
    assert_eq!(
        initial_bootstrap.len(),
        1,
        "master selection gets one bootstrap"
    );
    assert_eq!(
        initial_bootstrap[0],
        (
            2,
            2,
            1,
            XiMasterClassSummary {
                class_types: vec![1, 2, 2],
                class_source_ids: vec![2; 3],
                button_count: Some(10),
                scroll_flags: vec![],
            },
        ),
        "the initial DeviceChanged block matches master 2's stored CorePointerProc classes"
    );
    let selected_masks = (
        state.clients[&CLIENT].event_masks.clone(),
        state.clients[&CLIENT].xi2_masks.clone(),
        state.clients[&CLIENT].xi1_event_classes.clone(),
        state.clients[&CLIENT].xi1_window_event_classes.clone(),
    );
    let mut expected_xi2_selections = initial_selections.1.clone();
    expected_xi2_selections.insert((ROOT_WINDOW, 2), u64::from(DEVICE_CHANGED_MASK));
    assert_eq!(selected_masks.1, expected_xi2_selections);

    let info = DeviceInfo {
        source_id: SOURCE,
        enabled: true,
        resume_key: None,
        capabilities: InputCapabilities {
            keyboard: false,
            pointer: true,
            touch: false,
        },
        name: "master class test mouse".to_owned(),
        device_node: "/dev/input/event-master-class".to_owned(),
        sysname: "event-master-class".to_owned(),
        vendor_id: 0x1234,
        product_id: 0x5678,
        is_touchpad: false,
        config: Default::default(),
    };
    Backend::on_host_input(&mut backend, &mut state, HostInputEvent::DeviceAdded(info));
    let physical_id = state
        .xi_devices
        .facet(SOURCE, XiFacetKind::PointerTouch)
        .expect("DeviceAdded publishes the physical pointer facet");
    assert_eq!(physical_id, 6);
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::PointerMotion {
            origin: InputOrigin::Physical(SOURCE),
            x: 240,
            y: 160,
            time: 1,
            relative: false,
            dx: 0,
            dy: 0,
            motion_delta: None,
        },
    );
    let physical_changed = xi_master_changed_summaries(&kbd_map_drain(&mut peer));
    assert_eq!(
        physical_changed.len(),
        1,
        "one selected DeviceChanged event"
    );
    assert_eq!(
        physical_changed[0],
        (
            2,
            physical_id,
            1,
            XiMasterClassSummary {
                class_types: vec![1, 2, 2, 2, 2, 3, 3],
                class_source_ids: vec![physical_id; 7],
                button_count: Some(7),
                scroll_flags: vec![0, 0],
            },
        ),
        "SlaveSwitch carries the copied physical classes and their sourceid"
    );
    let physical_query = xi_master_classes_dispatch_request(
        &mut state,
        &mut backend,
        &mut peer,
        CLIENT,
        2,
        48,
        &[2, 0, 0, 0],
    );
    assert_eq!(
        xi_master_class_summary(&physical_query, 2),
        physical_changed[0].3,
        "XIQueryDevice serializes the same stored physical class set"
    );
    assert!(kbd_map_drain(&mut peer).is_empty());

    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceRemoved { source_id: SOURCE },
    );
    assert_eq!(state.xi_last_slave(2), None, "removal clears lastSlave");
    assert!(state.xi_devices.source(SOURCE).is_none());
    assert!(state.xi_devices.device(physical_id).is_none());
    assert!(
        kbd_map_drain(&mut peer).is_empty(),
        "removal has no class-switch event"
    );
    let removed_query = xi_master_classes_dispatch_request(
        &mut state,
        &mut backend,
        &mut peer,
        CLIENT,
        3,
        48,
        &[2, 0, 0, 0],
    );
    assert_eq!(
        xi_master_class_summary(&removed_query, 2),
        physical_changed[0].3,
        "clearing lastSlave preserves the copied classes and sourceid"
    );

    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::PointerMotion {
            origin: InputOrigin::XTest(4),
            x: 400,
            y: 300,
            time: 2,
            relative: false,
            dx: 0,
            dy: 0,
            motion_delta: None,
        },
    );
    let xtest_changed = xi_master_changed_summaries(&kbd_map_drain(&mut peer));
    assert_eq!(
        xtest_changed.len(),
        1,
        "XTEST source switch changes master classes"
    );
    assert_eq!(
        xtest_changed[0],
        (
            2,
            4,
            1,
            XiMasterClassSummary {
                class_types: vec![1, 2, 2],
                class_source_ids: vec![4; 3],
                button_count: Some(10),
                scroll_flags: vec![],
            },
        ),
        "SlaveSwitch carries XTEST 4's CorePointerProc classes"
    );
    let xtest_query = xi_master_classes_dispatch_request(
        &mut state,
        &mut backend,
        &mut peer,
        CLIENT,
        4,
        48,
        &[2, 0, 0, 0],
    );
    assert_eq!(
        xi_master_class_summary(&xtest_query, 2),
        xtest_changed[0].3,
        "XIQueryDevice and DeviceChanged use the same stored XTEST classes"
    );

    let final_registry: Vec<_> = state
        .xi_devices
        .devices()
        .iter()
        .map(|device| {
            (
                device.id,
                device.enabled,
                device.session_enabled,
                device.client_disabled,
                device.source_id,
                device.facet,
                device.attached_master,
                device.properties.clone(),
            )
        })
        .collect();
    assert_eq!(
        final_registry, initial_registry,
        "removing the test mouse restores the original live registry"
    );
    for device in state.xi_devices.devices() {
        assert_eq!(device.properties, initial_properties[&device.id]);
        assert_eq!(device.buttons_down, 0);
    }
    assert_eq!(
        (
            state.keys_down,
            state.buttons_down,
            state
                .xi_devices
                .devices()
                .iter()
                .map(|device| (device.id, device.buttons_down))
                .collect::<Vec<_>>(),
        ),
        initial_held
    );
    assert!(state.key_down_by_device.is_empty());
    assert_eq!(state.xi2_detached_masters, initial_detached);
    assert_eq!(state.floating_pointer_positions, initial_floating);
    assert_eq!(selected_masks.0, initial_selections.0);
    assert_eq!(selected_masks.2, initial_selections.2);
    assert_eq!(selected_masks.3, initial_selections.3);
    assert_eq!(
        (
            state.clients[&CLIENT].event_masks.clone(),
            state.clients[&CLIENT].xi2_masks.clone(),
            state.clients[&CLIENT].xi1_event_classes.clone(),
            state.clients[&CLIENT].xi1_window_event_classes.clone(),
        ),
        selected_masks
    );
    assert!(state.pending_xi_device_removals.is_empty());
    assert!(state.clients[&CLIENT].outbound.is_empty());
}

#[test]
fn xi_slave_switch_kms_keyboard_precedes_raw_and_key_events() {
    use yserver_core::{
        backend::Backend,
        core_loop::{DeviceInfo, HostInputEvent, InputOrigin},
        host_x11::HostKeyEvent,
        resources::ROOT_WINDOW,
        server::ServerState,
        xinput::{InputCapabilities, InputSourceId, XiFacetKind},
    };
    use yserver_protocol::x11::ClientId;

    const CLIENT: u32 = 0xA74;
    const KEY_MASK: u64 = (1 << 1) | (1 << 2) | (1 << 13);
    let info = |source_id| DeviceInfo {
        source_id: InputSourceId(source_id),
        enabled: true,
        resume_key: None,
        capabilities: InputCapabilities {
            keyboard: true,
            pointer: false,
            touch: false,
        },
        name: format!("keyboard-{source_id}"),
        device_node: format!("/dev/input/event-{source_id}"),
        sysname: format!("event-{source_id}"),
        vendor_id: 1,
        product_id: source_id as u32,
        is_touchpad: false,
        config: Default::default(),
    };
    let key = |source_id, keycode, pressed, time| {
        HostInputEvent::Key(HostKeyEvent {
            origin: InputOrigin::Physical(InputSourceId(source_id)),
            pressed,
            keycode,
            time,
            root_x: 10,
            root_y: 20,
            event_x: 10,
            event_y: 20,
            state: 0,
        })
    };
    let parse = |bytes: &[u8]| {
        let mut events = Vec::new();
        let mut offset = 0;
        while offset + 32 <= bytes.len() {
            assert_eq!(bytes[offset] & 0x7f, 35, "XI2 GenericEvent");
            let event_type = u16::from_le_bytes([bytes[offset + 8], bytes[offset + 9]]);
            let device_id = u16::from_le_bytes([bytes[offset + 10], bytes[offset + 11]]);
            let source_offset = if event_type == 1 {
                18
            } else if (13..=17).contains(&event_type) {
                20
            } else {
                52
            };
            let source_id = u16::from_le_bytes([
                bytes[offset + source_offset],
                bytes[offset + source_offset + 1],
            ]);
            let reason = if event_type == 1 {
                bytes[offset + 20]
            } else {
                0
            };
            if event_type == 1 {
                assert_eq!(
                    u16::from_le_bytes([bytes[offset + 32], bytes[offset + 33]]),
                    0,
                    "DeviceChanged carries KeyClass"
                );
                assert_eq!(
                    u16::from_le_bytes([bytes[offset + 36], bytes[offset + 37]]),
                    source_id,
                    "KeyClass source id"
                );
            }
            events.push((event_type, device_id, source_id, reason));
            let units = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap());
            offset += 32 + units as usize * 4;
        }
        assert_eq!(offset, bytes.len(), "complete KMS keyboard event stream");
        events
    };

    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    state.core_focus.raw = ROOT_WINDOW.0;
    let mut peer = kbd_map_client_id(&mut state, CLIENT);
    state
        .clients
        .get_mut(&CLIENT)
        .unwrap()
        .xi2_masks
        .insert((ROOT_WINDOW, 3), KEY_MASK);
    state.xi2_client_versions.insert(ClientId(CLIENT), (2, 2));

    let razer = info(0xA741);
    let hyperx = info(0xA742);
    for device in [razer.clone(), hyperx.clone()] {
        Backend::on_host_input(
            &mut backend,
            &mut state,
            HostInputEvent::DeviceAdded(device),
        );
    }
    let razer_id = state
        .xi_devices
        .facet(razer.source_id, XiFacetKind::Keyboard)
        .unwrap();
    let hyperx_id = state
        .xi_devices
        .facet(hyperx.source_id, XiFacetKind::Keyboard)
        .unwrap();
    assert_eq!((razer_id, hyperx_id), (6, 7));

    Backend::on_host_input(
        &mut backend,
        &mut state,
        key(razer.source_id.0, 38, true, 1),
    );
    let first = parse(&kbd_map_drain(&mut peer));
    assert_eq!(
        first,
        vec![
            (1, 3, razer_id, 1),
            (13, 3, razer_id, 0),
            (2, 3, razer_id, 0)
        ],
        "UpdateFromMaster DeviceChanged precedes raw and normal key delivery"
    );
    assert_eq!(state.xi_last_slave(3), Some(razer_id));

    Backend::on_host_input(
        &mut backend,
        &mut state,
        key(razer.source_id.0, 39, true, 2),
    );
    let same = parse(&kbd_map_drain(&mut peer));
    assert_eq!(same, vec![(13, 3, razer_id, 0), (2, 3, razer_id, 0)]);

    Backend::on_host_input(
        &mut backend,
        &mut state,
        key(hyperx.source_id.0, 40, true, 3),
    );
    let switched = parse(&kbd_map_drain(&mut peer));
    assert_eq!(
        switched,
        vec![
            (1, 3, hyperx_id, 1),
            (13, 3, hyperx_id, 0),
            (2, 3, hyperx_id, 0)
        ]
    );
    for (source, keycode, time) in [
        (razer.source_id.0, 38, 4),
        (razer.source_id.0, 39, 5),
        (hyperx.source_id.0, 40, 6),
    ] {
        Backend::on_host_input(&mut backend, &mut state, key(source, keycode, false, time));
        let _ = kbd_map_drain(&mut peer);
    }
    assert_eq!(state.xi_last_slave(3), Some(hyperx_id));
    assert!(state.keys_down.iter().all(|byte| *byte == 0));
    assert!(state.key_down_by_device.is_empty());
    assert!(state.sync_pending.is_empty());
    assert_eq!(state.xi_devices.devices().len(), 6);
    assert!(state.xi_devices.source(razer.source_id).is_some());
    assert!(state.xi_devices.source(hyperx.source_id).is_some());
    assert!(backend.core.down_keys.is_empty());
}
