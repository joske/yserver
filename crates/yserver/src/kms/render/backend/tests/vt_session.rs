use super::*;

// ─── Task 10: held-key tracking + synthesize-releases tests ───

/// `on_host_input` Key arm maintains `down_keys`: press inserts the
/// cooked keycode, release removes it. Tests the additive tracking
/// path without touching fanout behaviour.
#[test]
fn down_keys_maintained_on_key_press_and_release() {
    use yserver_core::{core_loop::HostInputEvent, host_x11::HostKeyEvent, server::ServerState};
    let mut b = KmsBackend::for_tests();
    let mut state = ServerState::new();

    let raw_press = HostKeyEvent {
        origin: yserver_core::core_loop::InputOrigin::NestedHost,
        keycode: 38, // evdev 'a' (US layout)
        pressed: true,
        state: 0,
        root_x: 0,
        root_y: 0,
        event_x: 0,
        event_y: 0,
        time: 0,
    };
    b.on_host_input(&mut state, HostInputEvent::Key(raw_press));
    assert!(
        b.core.down_keys.contains(&38),
        "key 38 must be in down_keys after press"
    );
    assert_eq!(b.core.down_keys.len(), 1);

    let raw_release = HostKeyEvent {
        origin: yserver_core::core_loop::InputOrigin::NestedHost,
        keycode: 38,
        pressed: false,
        state: 0,
        root_x: 0,
        root_y: 0,
        event_x: 0,
        event_y: 0,
        time: 0,
    };
    b.on_host_input(&mut state, HostInputEvent::Key(raw_release));
    assert!(
        !b.core.down_keys.contains(&38),
        "key 38 must be removed from down_keys after release"
    );
    assert!(b.core.down_keys.is_empty());
}

/// Retargeted for Task 14: KMS creates real source-owned held state,
/// drives the production VT path, then checks key/button and queue end
/// states after guarded physical cleanup.
#[test]
fn vt_suspend_clears_physical_source_keys_and_buttons_via_guarded_cleanup() {
    use yserver_core::{
        backend::Backend,
        core_loop::{DeviceInfo, HostInputEvent, InputOrigin, message::LibinputConfigSnapshot},
        host_x11::HostKeyEvent,
        server::ServerState,
        xinput::{InputCapabilities, InputSourceId, XiFacetKind},
    };

    let mut b = KmsBackend::for_tests();
    let mut state = ServerState::new();
    let source = InputSourceId(0xA140);
    b.on_host_input(
        &mut state,
        HostInputEvent::DeviceAdded(DeviceInfo {
            source_id: source,
            enabled: true,
            resume_key: None,
            capabilities: InputCapabilities {
                keyboard: true,
                pointer: true,
                touch: false,
            },
            name: "physical test device".into(),
            device_node: "/dev/input/event140".into(),
            sysname: "event140".into(),
            vendor_id: 1,
            product_id: 2,
            is_touchpad: false,
            config: LibinputConfigSnapshot::default(),
        }),
    );
    for keycode in [38, 56] {
        b.on_host_input(
            &mut state,
            HostInputEvent::Key(HostKeyEvent {
                origin: InputOrigin::Physical(source),
                keycode,
                pressed: true,
                time: 0,
                root_x: 0,
                root_y: 0,
                event_x: 0,
                event_y: 0,
                state: 0,
            }),
        );
    }
    for button in [0x110, 0x111] {
        b.on_host_input(
            &mut state,
            HostInputEvent::PointerButton {
                origin: InputOrigin::Physical(source),
                button,
                pressed: true,
                time: 0,
            },
        );
    }
    assert_eq!(b.core.down_keys.len(), 2);
    assert_eq!(b.core.button_mask & 0x0500, 0x0500);

    // Exercise the production VT driver so releases use each physical
    // facet's guarded state rather than synthetic global state.
    b.inject_seat_event_for_test(&mut state, false);

    assert!(b.core.down_keys.is_empty());
    assert_eq!(b.core.button_mask, 0);
    assert_eq!(
        state
            .xi_devices
            .device(
                state
                    .xi_devices
                    .facet(source, XiFacetKind::PointerTouch)
                    .unwrap()
            )
            .unwrap()
            .buttons_down,
        0,
    );
    assert!(
        state
            .key_down_by_device
            .get(
                &state
                    .xi_devices
                    .facet(source, XiFacetKind::Keyboard)
                    .unwrap()
            )
            .is_none_or(std::collections::HashMap::is_empty)
    );
    assert!(b.core.pending_pointer_events.is_empty());
}

#[test]
fn xi_dynamic_reset_retires_kms_input_and_replays_inventory_on_the_runner_boundary() {
    use std::{
        collections::{HashMap, HashSet},
        thread,
        time::{Duration, Instant},
    };
    use yserver_core::{
        core_loop::{
            CoreSender, DeviceInfo, Generation, HostInputEvent, InputOrigin, Message, ResetPolicy,
            auth::AuthState,
            channel,
            message::{BoolSetting, FloatSetting, LibinputConfigSnapshot, OneHot2, U32Setting},
            poll_tokens::ClientIdAllocator,
            process_request::process_request,
            run::handle_host_input,
            run_core,
        },
        host_x11::HostKeyEvent,
        resources::ROOT_WINDOW,
        server::ServerState,
        xinput::{InputCapabilities, InputSourceId, XiFacetKind},
    };
    use yserver_protocol::x11::{ClientId, RequestHeader, SequenceNumber};

    fn info(source: InputSourceId, name: &str, keyboard: bool, pointer: bool) -> DeviceInfo {
        DeviceInfo {
            source_id: source,
            enabled: true,
            resume_key: None,
            capabilities: InputCapabilities {
                keyboard,
                pointer,
                touch: false,
            },
            name: name.into(),
            device_node: format!("/dev/input/event{}", source.0),
            sysname: format!("event{}", source.0),
            vendor_id: 0x1234,
            product_id: u32::try_from(source.0).unwrap_or_default(),
            is_touchpad: false,
            config: LibinputConfigSnapshot {
                accel: FloatSetting {
                    available: pointer,
                    current: 0.0,
                    default: 0.0,
                },
                accel_profile: OneHot2 {
                    available: pointer,
                    current: Some(0),
                    default: Some(0),
                },
                accel_profile_available_mask: if pointer { 0b011 } else { 0 },
                tap: BoolSetting::default(),
                scroll_button: U32Setting::default(),
                ..Default::default()
            },
        }
    }

    fn post_input(sender: &CoreSender, event: HostInputEvent) {
        sender
            .send(Message::HostInput(event))
            .expect("post process-lifetime input event");
    }

    fn wait_generation(counter: &yserver_core::core_loop::GenerationCounter, old: Generation) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while counter.current() == old && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert_ne!(counter.current(), old, "runner must cross reset boundary");
    }

    const MOUSE_20: InputSourceId = InputSourceId(20);
    const MOUSE_10: InputSourceId = InputSourceId(10);
    const MIXED: InputSourceId = InputSourceId(30);
    const REMOVED: InputSourceId = InputSourceId(40);
    const SHIFT_L: u8 = 50;
    const KEY_A: u8 = 38;
    const KEY_B: u8 = 56;
    const CAPS_LOCK: u8 = 66;

    let mut backend = KmsBackend::for_tests();
    let led_relay = std::sync::Arc::new(
        crate::input::LedRelay::new().expect("test keyboard LED relay eventfd"),
    );
    backend.set_led_relay(led_relay.clone());
    make_vt_fixture_headless(&mut backend);
    backend
        .core
        .xid_map
        .insert(backend.core.window_id, ROOT_WINDOW);
    let mut state = ServerState::new();
    for i in 0..32 {
        state.atoms.intern(&format!("_KMS_OLD_SESSION_{i}"), false);
    }
    let source_20 = info(MOUSE_20, "Mouse twenty", false, true);
    let source_30 = info(MIXED, "Mixed keyboard pointer", true, true);
    let source_10 = info(MOUSE_10, "Mouse ten", false, true);
    let removed = info(REMOVED, "Removed mouse", false, true);
    // Seed an old-generation property atom. DeviceAdded is dispatched
    // through run_core below and reuses it in this generation. Reset
    // replaces the table before the inventory recreates the atom.
    let old_profile_atom = state.atoms.intern("libinput Accel Profile Enabled", false);
    for device in [&source_20, &source_30, &source_10, &removed] {
        handle_host_input(
            &mut state,
            &mut backend,
            HostInputEvent::DeviceAdded(device.clone()),
        );
    }
    let changed_map = backend.core.recompile_keymap(&crate::kms::core::XkbRmlvo {
        rules: "evdev".into(),
        model: "pc105".into(),
        layout: "us,be".into(),
        variant: String::new(),
        options: None,
    });
    assert!(changed_map.is_some(), "alternate pre-reset keymap compiles");
    assert_eq!(backend.core.xkb_rmlvo.layout, "us,be");
    for pressed in [true, false] {
        handle_host_input(
            &mut state,
            &mut backend,
            HostInputEvent::Key(HostKeyEvent {
                origin: InputOrigin::Physical(MIXED),
                keycode: CAPS_LOCK,
                pressed,
                time: 1,
                root_x: 0,
                root_y: 0,
                event_x: 0,
                event_y: 0,
                state: 0,
            }),
        );
    }
    backend.sync_keyboard_leds();
    assert_eq!(
        led_relay.drain(),
        input::Led::CAPSLOCK.bits(),
        "pre-reset Caps Lock mask reaches the input relay",
    );
    let old_id_6_source = state.xi_devices.device(6).unwrap().source_id;
    assert_eq!(old_id_6_source, Some(MOUSE_20));

    let _peer = kbd_map_client_id(&mut state, 7);

    // Set up real device grabs through process_request before run_core.
    // This lets the KMS-specific test cover floating input without a
    // request/reply socket reader; the reset itself still runs through
    // run_core below.
    for (device_id, sequence, event_mask) in [
        (8u16, 1u16, (1u32 << 4) | (1 << 5)),
        (7u16, 2u16, (1u32 << 2) | (1 << 3)),
    ] {
        let mut grab = Vec::with_capacity(24);
        grab.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
        grab.extend_from_slice(&0u32.to_le_bytes());
        grab.extend_from_slice(&0u32.to_le_bytes());
        grab.extend_from_slice(&device_id.to_le_bytes());
        grab.extend_from_slice(&[1, 1, 0, 0]);
        grab.extend_from_slice(&1u16.to_le_bytes());
        grab.extend_from_slice(&event_mask.to_le_bytes());
        process_request(
            &mut state,
            &mut backend,
            ClientId(7),
            SequenceNumber(sequence),
            RequestHeader {
                opcode: 137,
                data: 51,
                length_units: 7,
            },
            &grab,
            None,
        )
        .expect("real XIGrabDevice request succeeds");
    }
    let mixed_keyboard_before = state
        .xi_devices
        .facet(MIXED, XiFacetKind::Keyboard)
        .unwrap();
    let mixed_pointer_before = state
        .xi_devices
        .facet(MIXED, XiFacetKind::PointerTouch)
        .unwrap();
    assert_eq!(mixed_keyboard_before, 7);
    assert_eq!(mixed_pointer_before, 8);
    assert_eq!(
        state
            .xi_devices
            .device(mixed_keyboard_before)
            .unwrap()
            .attached_master,
        None,
    );
    assert_eq!(
        state
            .xi_devices
            .device(mixed_pointer_before)
            .unwrap()
            .attached_master,
        None,
    );
    assert!(state.xi2_pointer_grabs.contains_key(&mixed_pointer_before));
    assert!(
        state
            .xi2_keyboard_grabs
            .contains_key(&mixed_keyboard_before)
    );

    // Exercise old-session master lock state and simultaneous physical,
    // XTEST, and floating key/button holds through the production KMS
    // input handler.
    for (origin, keycode) in [
        (InputOrigin::Physical(MIXED), SHIFT_L),
        (InputOrigin::Physical(MIXED), KEY_A),
        (InputOrigin::XTest(5), KEY_B),
    ] {
        handle_host_input(
            &mut state,
            &mut backend,
            HostInputEvent::Key(HostKeyEvent {
                origin,
                keycode,
                pressed: true,
                time: 2,
                root_x: 0,
                root_y: 0,
                event_x: 0,
                event_y: 0,
                state: 0,
            }),
        );
    }
    handle_host_input(
        &mut state,
        &mut backend,
        HostInputEvent::PointerMotion {
            origin: InputOrigin::Physical(MIXED),
            x: 12,
            y: 20,
            time: 3,
            relative: false,
            dx: 0,
            dy: 0,
            motion_delta: None,
        },
    );
    for (origin, button) in [
        (InputOrigin::Physical(MOUSE_20), 0x110),
        (InputOrigin::Physical(MIXED), 0x112),
        (InputOrigin::XTest(4), 0x111),
    ] {
        handle_host_input(
            &mut state,
            &mut backend,
            HostInputEvent::PointerButton {
                origin,
                button,
                pressed: true,
                time: 3,
            },
        );
    }
    assert_eq!(
        backend.current_led_bits(),
        input::Led::CAPSLOCK.bits(),
        "pre-reset master lock state lights Caps Lock",
    );
    assert_eq!(backend.core.down_keys, HashSet::from([KEY_B]));
    assert!(
        backend
            .floating_keyboard_states
            .contains_key(&mixed_keyboard_before)
    );
    assert_eq!(
        state.key_down_by_device.get(&mixed_keyboard_before),
        Some(&HashMap::from([
            (SHIFT_L, InputOrigin::Physical(MIXED)),
            (KEY_A, InputOrigin::Physical(MIXED)),
        ])),
    );
    assert!(
        state
            .floating_pointer_positions
            .contains_key(&mixed_pointer_before)
    );
    assert_eq!(
        state
            .xi_devices
            .device(mixed_pointer_before)
            .unwrap()
            .buttons_down,
        0b010,
    );
    assert_eq!(state.buttons_down & 0b101, 0b101);
    assert_eq!(state.buttons_down & 0b010, 0);
    assert_eq!(
        state.key_repeats[&InputOrigin::Physical(MIXED)]
            .event
            .keycode,
        KEY_A,
        "pre-reset physical key has an armed repeat",
    );
    assert_eq!(
        state.key_repeats[&InputOrigin::XTest(5)].event.keycode,
        KEY_B,
        "pre-reset XTEST key has an independent armed repeat",
    );
    let (poll, sender, receiver) = channel().expect("core channel");
    let generation_counter = receiver.generation_counter();
    let generation_zero = generation_counter.current();
    let input_sender = sender.clone_handle();
    let driver = thread::spawn(move || {
        // The runner inventory receives these same lifecycle messages;
        // a suspended source remains in it while a removed source does
        // not. Facet IDs before reset are 6, 7/8, 9, then 10.
        for device in [&source_20, &source_30, &source_10, &removed] {
            post_input(&input_sender, HostInputEvent::DeviceAdded(device.clone()));
        }

        // Repeating a live identity keeps the inventory's current
        // source configuration before the reset boundary. The runner
        // also records the remove and suspend facts before rebuilding.
        post_input(
            &input_sender,
            HostInputEvent::DeviceRemoved { source_id: REMOVED },
        );
        post_input(
            &input_sender,
            HostInputEvent::DeviceSuspended {
                source_id: MOUSE_10,
            },
        );

        input_sender
            .send(Message::ResetRequested)
            .expect("request first reset");
        wait_generation(&generation_counter, generation_zero);

        // Give the old repeat deadline a chance to fire. The cleanup hook
        // removes its source repeat and the fresh generation has none.
        thread::sleep(Duration::from_millis(700));
        for (origin, keycode) in [
            (InputOrigin::Physical(MIXED), SHIFT_L),
            (InputOrigin::Physical(MIXED), KEY_A),
            (InputOrigin::XTest(5), KEY_B),
        ] {
            post_input(
                &input_sender,
                HostInputEvent::Key(HostKeyEvent {
                    origin,
                    keycode,
                    pressed: false,
                    time: 4,
                    root_x: 0,
                    root_y: 0,
                    event_x: 0,
                    event_y: 0,
                    state: 0,
                }),
            );
        }
        for (origin, button) in [
            (InputOrigin::Physical(MOUSE_20), 0x110),
            (InputOrigin::Physical(MIXED), 0x112),
            (InputOrigin::XTest(4), 0x111),
        ] {
            post_input(
                &input_sender,
                HostInputEvent::PointerButton {
                    origin,
                    button,
                    pressed: false,
                    time: 5,
                },
            );
        }

        // Late old-source releases must not disturb the reset session.
        // Then send fresh pointer input, whose final state is inspected
        // after run_core returns.
        post_input(
            &input_sender,
            HostInputEvent::PointerMotion {
                origin: InputOrigin::Physical(MOUSE_20),
                x: 0,
                y: 0,
                time: 6,
                relative: true,
                dx: 5,
                dy: 0,
                motion_delta: Some([5.0, 0.0]),
            },
        );
        input_sender
            .send(Message::Shutdown)
            .expect("stop runner after fresh post-reset input");
    });

    run_core(
        poll,
        receiver,
        sender,
        &mut state,
        &mut backend,
        Vec::new(),
        &ClientIdAllocator::new(),
        AuthState::new(None),
        ResetPolicy::Reset,
        None,
    )
    .expect("core loop crosses both reset boundaries and shuts down cleanly");
    driver.join().expect("input driver thread");

    let pointer_10 = state
        .xi_devices
        .facet(MOUSE_10, XiFacetKind::PointerTouch)
        .expect("suspended mouse ten survives reset");
    let pointer_20 = state
        .xi_devices
        .facet(MOUSE_20, XiFacetKind::PointerTouch)
        .expect("mouse twenty survives reset");
    let mixed_keyboard = state
        .xi_devices
        .facet(MIXED, XiFacetKind::Keyboard)
        .expect("mixed keyboard facet survives reset");
    let mixed_pointer = state
        .xi_devices
        .facet(MIXED, XiFacetKind::PointerTouch)
        .expect("mixed pointer facet survives reset");
    assert_eq!(
        (pointer_10, pointer_20, mixed_keyboard, mixed_pointer),
        (6, 7, 8, 9)
    );
    assert_eq!(
        state.xi_devices.device(6).unwrap().source_id,
        Some(MOUSE_10)
    );
    assert_ne!(
        old_id_6_source,
        state.xi_devices.device(6).unwrap().source_id
    );
    assert!(state.xi_devices.source(REMOVED).is_none());
    assert!(!state.xi_devices.source(MOUSE_10).unwrap().enabled);
    assert!(state.xi_devices.source(MOUSE_20).unwrap().enabled);
    assert!(state.xi_devices.source(MIXED).unwrap().enabled);
    assert_eq!(
        state
            .xi_devices
            .device(mixed_keyboard)
            .unwrap()
            .attached_master,
        Some(yserver_core::xinput::DEVICEID_MASTER_KEYBOARD)
    );
    assert_eq!(
        state
            .xi_devices
            .device(mixed_pointer)
            .unwrap()
            .attached_master,
        Some(yserver_core::xinput::DEVICEID_MASTER_POINTER)
    );

    let current_profile_atom = state
        .atoms
        .id_for("libinput Accel Profile Enabled")
        .expect("new generation recreates property atom");
    assert_ne!(old_profile_atom, current_profile_atom);
    assert_eq!(
        state.atoms.name(current_profile_atom),
        Some("libinput Accel Profile Enabled")
    );
    assert!(
        state
            .xi_devices
            .device(pointer_20)
            .unwrap()
            .properties
            .contains_key(&current_profile_atom)
    );

    assert!(
        state.clients.is_empty(),
        "old-generation client was destroyed"
    );
    assert_eq!(
        state
            .keys_down
            .iter()
            .map(|byte| byte.count_ones())
            .sum::<u32>(),
        0,
        "the reset session has no stale keys held",
    );
    assert_eq!(state.buttons_down, 0);
    assert!(state.key_down_by_device.is_empty());
    assert!(state.unpublished_keyboard_keys_down.is_empty());
    assert!(state.unpublished_pointer_buttons_down.is_empty());
    assert!(
        state.key_repeats.is_empty(),
        "reset drops old repeat timers"
    );
    assert!(state.sync_pending.is_empty());
    assert!(state.xi1_frozen.is_empty());
    assert!(state.xi2_pointer_grabs.is_empty());
    assert!(state.xi2_keyboard_grabs.is_empty());
    assert!(state.xi2_detached_masters.is_empty());
    assert!(state.floating_pointer_positions.is_empty());
    assert_eq!(
        backend
            .core
            .xkb_state
            .0
            .serialize_mods(xkbcommon::xkb::STATE_MODS_DEPRESSED),
        0
    );
    assert_eq!(
        backend
            .core
            .xkb_state
            .0
            .serialize_mods(xkbcommon::xkb::STATE_MODS_LATCHED),
        0
    );
    assert_eq!(
        backend
            .core
            .xkb_state
            .0
            .serialize_mods(xkbcommon::xkb::STATE_MODS_LOCKED),
        0
    );
    assert_eq!(backend.current_led_bits(), 0);
    assert_eq!(backend.leds_sent, 0);
    assert_eq!(led_relay.drain(), 0, "reset relays the fresh LED mask");
    assert_eq!(backend.core.xkb_rmlvo.layout, "us");
    assert!(backend.core.down_keys.is_empty());
    assert_eq!(state.pointer_motion_history.len(), 1);
    assert!(state.pointer_motion_history[0].time > 0);
    assert!(backend.core.pending_pointer_events.is_empty());
    assert!(backend.floating_keyboard_states.is_empty());
    assert_eq!(state.xi_devices.device(pointer_10).unwrap().buttons_down, 0);
    assert_eq!(state.xi_devices.device(pointer_20).unwrap().buttons_down, 0);
    assert_eq!(
        state.xi_devices.device(mixed_pointer).unwrap().buttons_down,
        0
    );
}

// ── Task 13: stub-backed VT-switch suspend/resume integration tests ──
//
// These tests drive `inject_seat_event_for_test` directly — no DRM, no
// real hardware. They exercise the full state-machine path
// plus `run_suspend` side-effects that are reachable in the stub harness.
//
// Resume path note: the ordinary fixture's synthetic `/dev/null` DRM
// device fails connector probing. That failure must leave the state in
// `Resuming` with scanout closed; production concurrently requests a
// process exit. Tests that need a successful logical VT cycle convert the
// fixture to the supported zero-device headless configuration first.

fn make_vt_fixture_headless(b: &mut KmsBackend) {
    b.platform.devices.clear();
    b.platform.outputs.clear();
    b.platform.scanout_pools.clear();
    b.platform.bo_generations.clear();
    b.platform.first_pageflip_logged.clear();
    b.platform.fb_w = 0;
    b.platform.fb_h = 0;
    b.kms_outputs_active = false;
    b.scene
        .rebuild_outputs(&b.platform)
        .expect("headless scene rebuild");
}

/// A screensaver blank that lands after a VT switch away used to issue a
/// modeset while we no longer held DRM master, failing with EACCES
/// part-way through `dpms_set_outputs_active` and leaving
/// `kms_outputs_active` disagreeing with the hardware. Reported on
/// discussion #79 (Alpine/AMD GX-424CC): `libseat disable() ok` followed by
/// `disable_output for eDP-1 failed: ... Permission denied (os error 13)`.
/// `scanout_allowed()` is the documented gate for master-requiring work.
#[test]
fn set_dpms_power_is_skipped_while_suspended_and_failed_resume_stays_closed() {
    use crate::vt::state::VtState;
    use yserver_core::backend::Backend;

    let mut b = KmsBackend::for_tests();
    let mut state = ServerState::new();

    assert_eq!(b.vt_state, VtState::Active);
    assert!(b.kms_outputs_active, "outputs start on");

    // Switch away: master is dropped, scanout gate closes.
    b.inject_seat_event_for_test(&mut state, false);
    assert_eq!(b.vt_state, VtState::Suspended);
    assert!(!b.scanout_allowed());

    // A blank arriving now must be a no-op, not a modeset attempt.
    b.set_dpms_power(3).expect("suspended DPMS must not error");
    assert!(
        b.kms_outputs_active,
        "cached output state must not flip while suspended — the commit \
             cannot have run, so claiming the outputs are off would desync \
             the cache from the hardware"
    );

    // The synthetic DRM fd cannot be probed on resume. Do not reopen the
    // scanout gate merely because shutdown was requested asynchronously.
    b.inject_seat_event_for_test(&mut state, true);
    assert_eq!(b.vt_state, VtState::Resuming);
    assert!(
        !b.scanout_allowed(),
        "failed resume must keep the gate closed"
    );
}

/// After `inject_seat_event_for_test(false)` the backend must be in
/// `Suspended` and `scanout_allowed()` must return `false`.
///
/// Also verifies that physical key/button holds created through KMS input
/// dispatch are cleared by `release_device_state` inside `run_suspend`.
#[test]
fn vt_switch_disable_transitions_to_suspended_and_releases_held_input() {
    use crate::vt::state::VtState;
    use yserver_core::{
        backend::Backend,
        core_loop::{DeviceInfo, HostInputEvent, InputOrigin, message::LibinputConfigSnapshot},
        host_x11::HostKeyEvent,
        xinput::{InputCapabilities, InputSourceId, XiFacetKind},
    };

    let mut b = KmsBackend::for_tests();
    let mut state = ServerState::new();
    let source = InputSourceId(0xA141);

    b.on_host_input(
        &mut state,
        HostInputEvent::DeviceAdded(DeviceInfo {
            source_id: source,
            enabled: true,
            resume_key: None,
            capabilities: InputCapabilities {
                keyboard: true,
                pointer: true,
                touch: false,
            },
            name: "VT test device".into(),
            device_node: "/dev/input/event141".into(),
            sysname: "event141".into(),
            vendor_id: 1,
            product_id: 2,
            is_touchpad: false,
            config: LibinputConfigSnapshot::default(),
        }),
    );
    for keycode in [38, 56] {
        b.on_host_input(
            &mut state,
            HostInputEvent::Key(HostKeyEvent {
                origin: InputOrigin::Physical(source),
                keycode,
                pressed: true,
                time: 0,
                root_x: 0,
                root_y: 0,
                event_x: 0,
                event_y: 0,
                state: 0,
            }),
        );
    }
    b.on_host_input(
        &mut state,
        HostInputEvent::PointerButton {
            origin: InputOrigin::Physical(source),
            button: 0x110,
            pressed: true,
            time: 0,
        },
    );
    assert_eq!(b.core.down_keys.len(), 2);
    assert_ne!(b.core.button_mask, 0);

    // Precondition: starts Active with scanout allowed.
    assert_eq!(b.vt_state, VtState::Active);
    assert!(
        b.scanout_allowed(),
        "scanout must be allowed before disable"
    );

    // Drive the Disable event.
    b.inject_seat_event_for_test(&mut state, false);

    // (a) State machine reached Suspended.
    assert_eq!(
        b.vt_state,
        VtState::Suspended,
        "vt_state must be Suspended after Disable"
    );

    // (b) Scanout gate is closed.
    assert!(
        !b.scanout_allowed(),
        "scanout must not be allowed while Suspended"
    );

    // (c) Physical keys cleared by the guarded source release path.
    assert!(
        b.core.down_keys.is_empty(),
        "physical down_keys must be empty after suspend"
    );

    // (d) Physical buttons cleared and their source remains registered but
    // disabled for VT continuation.
    assert_eq!(
        b.core.button_mask, 0,
        "physical button_mask must be 0 after suspend"
    );
    assert!(!state.xi_devices.source(source).unwrap().enabled);
    assert_eq!(
        state
            .xi_devices
            .device(
                state
                    .xi_devices
                    .facet(source, XiFacetKind::PointerTouch)
                    .unwrap()
            )
            .unwrap()
            .buttons_down,
        0,
    );
    assert!(
        state
            .key_down_by_device
            .get(
                &state
                    .xi_devices
                    .facet(source, XiFacetKind::Keyboard)
                    .unwrap()
            )
            .is_none_or(std::collections::HashMap::is_empty)
    );
}

/// After a Disable→Enable cycle the state machine must return to `Active`.
///
/// The test uses a zero-device configuration, for which an empty connector
/// snapshot and an empty re-light are both successful by design.
#[test]
fn vt_switch_enable_after_disable_returns_to_active() {
    use crate::vt::state::VtState;

    let mut b = KmsBackend::for_tests();
    let mut state = ServerState::new();
    make_vt_fixture_headless(&mut b);

    // Drive Disable → Suspended.
    b.inject_seat_event_for_test(&mut state, false);
    assert_eq!(b.vt_state, VtState::Suspended);

    // Drive Enable → Active through the valid headless resume path.
    b.inject_seat_event_for_test(&mut state, true);

    assert_eq!(
        b.vt_state,
        VtState::Active,
        "vt_state must be Active after Enable completes"
    );
    assert!(
        b.scanout_allowed(),
        "scanout must be allowed after returning to Active"
    );
}

/// A rapid Disable-then-Enable-then-Disable sequence exercises the
/// no-blink boundary: a `Disable` coalesced during a resume sequence
/// causes `resume_complete` to return `BeginSuspend`, skipping `Active`
/// entirely.  The final state must be `Suspended`, not `Active`.
///
/// Concretely this test drives:
///
///  1. Disable → Suspending → (suspend sequence) → Suspended
///  2. Enable  → Resuming → (resume sequence) → resume_complete;
///     before step 2 we pre-seed `pending_disable = true` to simulate
///     a Disable that arrived mid-resume.
///  3. `resume_complete` sees `pending_disable` → returns `BeginSuspend`
///     → run_suspend again → Suspended
///
/// The final assertion is `Suspended` and no panic (RefCell re-entrancy
/// is exercised by the full Disable→Enable path above as well).
#[test]
fn vt_switch_rapid_double_switch_never_passes_through_active() {
    use crate::vt::state::{VtPending, VtState};

    let mut b = KmsBackend::for_tests();
    let mut state = ServerState::new();
    make_vt_fixture_headless(&mut b);

    // Step 1: normal Disable → Suspended.
    b.inject_seat_event_for_test(&mut state, false);
    assert_eq!(b.vt_state, VtState::Suspended);

    // Simulate a Disable that arrives during the resume sequence by
    // pre-seeding `pending_disable`.  In production this would be set
    // by `on_event(Disable)` arriving while `vt_state == Resuming`
    // (the coalesce arm in `VtState::on_event`).  We set it directly
    // here because the stub drives events synchronously and we cannot
    // interleave them mid-sequence without modifying the backend.
    b.vt_pending = VtPending {
        pending_disable: true,
        pending_enable: false,
    };

    // Step 2: Enable with pending_disable set → resume_complete skips
    // Active and goes straight to Suspending → run_suspend → Suspended.
    b.inject_seat_event_for_test(&mut state, true);

    assert_eq!(
        b.vt_state,
        VtState::Suspended,
        "rapid double-switch must end in Suspended, never passing through Active"
    );
    assert!(
        !b.scanout_allowed(),
        "scanout must not be allowed after rapid double-switch lands in Suspended"
    );
    // pending_disable must have been consumed by resume_complete.
    assert!(
        !b.vt_pending.pending_disable,
        "pending_disable must be cleared after resume_complete consumed it"
    );
}

/// A coalesced `pending_enable` (an Enable that arrived during the
/// suspend sequence) must be consumed by the drive loop and resume the
/// session, not strand it in `Suspended`. Regression test for the
/// final-review finding that `pending_enable` was set but never acted
/// on. We pre-seed `pending_enable` (the synchronous stub can't
/// interleave a real Enable mid-suspend) and drive a Disable; the loop
/// must run suspend, then consume the flag and resume to `Active`.
#[test]
fn vt_switch_coalesced_enable_resumes_not_stranded_in_suspended() {
    use crate::vt::state::{VtPending, VtState};

    let mut b = KmsBackend::for_tests();
    let mut state = ServerState::new();
    make_vt_fixture_headless(&mut b);

    b.vt_pending = VtPending {
        pending_enable: true,
        pending_disable: false,
    };

    b.inject_seat_event_for_test(&mut state, false);

    assert_eq!(
        b.vt_state,
        VtState::Active,
        "a coalesced pending_enable must drive a resume after suspend, not strand in Suspended"
    );
    assert!(
        !b.vt_pending.pending_enable,
        "pending_enable must be cleared once the consume-loop acts on it"
    );
}

/// Re-entrancy smoke: a full Disable→Enable cycle completes without a
/// `RefCell` borrow panic. In the stub harness this verifies that no
/// backend `RefCell` panics during the sequence.
#[test]
fn vt_switch_full_cycle_no_refcell_panic() {
    use crate::vt::state::VtState;

    let mut b = KmsBackend::for_tests();
    let mut state = ServerState::new();
    make_vt_fixture_headless(&mut b);

    // Two full cycles — if any RefCell is double-borrowed this panics.
    for _ in 0..2 {
        b.inject_seat_event_for_test(&mut state, false);
        assert_eq!(b.vt_state, VtState::Suspended);
        b.inject_seat_event_for_test(&mut state, true);
        assert_eq!(b.vt_state, VtState::Active);
    }
}

#[test]
fn xi_source_removal_vt_suspend_releases_physical_state_once_and_resumes_same_facets() {
    use yserver_core::{
        backend::Backend,
        core_loop::{
            DeviceInfo, HostInputEvent, InputInventory, InputOrigin,
            message::{FloatSetting, LibinputConfigSnapshot},
            run::{dispatch_vt_acquire, dispatch_vt_release},
        },
        host_x11::HostKeyEvent,
        resources::ROOT_WINDOW,
        server::ServerState,
        xinput::{InputCapabilities, InputSourceId, XiFacetKind},
    };

    const SOURCE: InputSourceId = InputSourceId(0xE140);
    const CLIENT: u32 = 0xE14;
    const CTRL_L: u8 = 37;
    const ALT_L: u8 = 64;
    const CAPS_LOCK: u8 = 66;
    const NUM_LOCK: u8 = 77;
    const SUPER_L: u8 = 133;
    const SHIFT_L: u8 = 50;
    const KEY_MASK: u64 = (1 << 2) | (1 << 3);
    const BUTTON_MASK: u64 = (1 << 4) | (1 << 5);

    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    make_vt_fixture_headless(&mut backend);
    backend.vt_switching_armed = true;
    let mut input_inventory = InputInventory::new();
    backend
        .core
        .xid_map
        .insert(backend.core.window_id, ROOT_WINDOW);
    state.core_focus.raw = ROOT_WINDOW.0;
    let mut peer = kbd_map_client_id(&mut state, CLIENT);
    let source_config = LibinputConfigSnapshot {
        accel: FloatSetting {
            available: true,
            current: 0.375,
            default: 0.0,
        },
        ..LibinputConfigSnapshot::default()
    };
    let info = DeviceInfo {
        source_id: SOURCE,
        enabled: true,
        resume_key: None,
        capabilities: InputCapabilities {
            keyboard: true,
            pointer: true,
            touch: false,
        },
        name: "VT mixed device".to_owned(),
        device_node: "/dev/input/event140".to_owned(),
        sysname: "event140".to_owned(),
        vendor_id: 0x1234,
        product_id: 0x5678,
        is_touchpad: false,
        config: source_config,
    };
    input_inventory.add(info.clone());

    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceAdded(info.clone()),
    );
    let keyboard = state
        .xi_devices
        .facet(SOURCE, XiFacetKind::Keyboard)
        .expect("keyboard facet");
    let pointer = state
        .xi_devices
        .facet(SOURCE, XiFacetKind::PointerTouch)
        .expect("pointer facet");
    let keyboard_properties = state
        .xi_devices
        .device(keyboard)
        .unwrap()
        .properties
        .clone();
    let pointer_properties = state.xi_devices.device(pointer).unwrap().properties.clone();
    for (device, mask) in [
        (keyboard, KEY_MASK),
        (pointer, BUTTON_MASK),
        (2, BUTTON_MASK),
        (3, KEY_MASK),
        (4, BUTTON_MASK),
        (5, KEY_MASK),
    ] {
        state
            .clients
            .get_mut(&CLIENT)
            .unwrap()
            .xi2_masks
            .insert((ROOT_WINDOW, device), mask);
    }

    // Create the active pointer grab through the real XIGrabDevice
    // request path. Suspend must end it and restore the facet's master
    // attachment, while retaining its selection masks.
    let mut grab_body = Vec::with_capacity(24);
    grab_body.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    grab_body.extend_from_slice(&0u32.to_le_bytes());
    grab_body.extend_from_slice(&0u32.to_le_bytes());
    grab_body.extend_from_slice(&pointer.to_le_bytes());
    grab_body.extend_from_slice(&[1, 1, 0, 0]);
    grab_body.extend_from_slice(&1u16.to_le_bytes());
    grab_body.extend_from_slice(&((1u32 << 4) | (1u32 << 5)).to_le_bytes());
    yserver_core::core_loop::process_request::process_request(
        &mut state,
        &mut backend,
        yserver_protocol::x11::ClientId(CLIENT),
        yserver_protocol::x11::SequenceNumber(1),
        yserver_protocol::x11::RequestHeader {
            opcode: 137,
            data: 51,
            length_units: 7,
        },
        &grab_body,
        None,
    )
    .expect("XIGrabDevice on the pointer facet");
    assert!(state.xi2_pointer_grabs.contains_key(&pointer));
    assert_eq!(
        state.xi_devices.device(pointer).unwrap().attached_master,
        None
    );

    let key = |origin, keycode, pressed| {
        HostInputEvent::Key(HostKeyEvent {
            origin,
            pressed,
            keycode,
            time: 1,
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            state: 0,
        })
    };
    let button = |origin, button, pressed| HostInputEvent::PointerButton {
        origin,
        button,
        pressed,
        time: 2,
    };
    for keycode in [CAPS_LOCK, NUM_LOCK, CTRL_L, ALT_L] {
        Backend::on_host_input(
            &mut backend,
            &mut state,
            key(InputOrigin::Physical(SOURCE), keycode, true),
        );
        if keycode == CAPS_LOCK || keycode == NUM_LOCK {
            Backend::on_host_input(
                &mut backend,
                &mut state,
                key(InputOrigin::Physical(SOURCE), keycode, false),
            );
        }
    }
    Backend::on_host_input(
        &mut backend,
        &mut state,
        key(InputOrigin::XTest(keyboard), SUPER_L, true),
    );
    Backend::on_host_input(
        &mut backend,
        &mut state,
        key(InputOrigin::XTest(5), SHIFT_L, true),
    );
    Backend::on_host_input(
        &mut backend,
        &mut state,
        button(InputOrigin::Physical(SOURCE), 0x110, true),
    );
    Backend::on_host_input(
        &mut backend,
        &mut state,
        button(InputOrigin::XTest(pointer), 0x112, true),
    );
    Backend::on_host_input(
        &mut backend,
        &mut state,
        button(InputOrigin::XTest(4), 0x112, true),
    );

    let drain_peer = |state: &mut ServerState, peer: &mut std::os::unix::net::UnixStream| {
        let mut bytes = Vec::new();
        for _ in 0..64 {
            bytes.extend(kbd_map_drain(peer));
            let Some(client) = state.clients.get_mut(&CLIENT) else {
                break;
            };
            if client.outbound.is_empty() {
                break;
            }
            let outcome = yserver_core::core_loop::client_io::drain_outbound(client)
                .expect("flush buffered VT events");
            assert_ne!(
                outcome,
                yserver_core::core_loop::client_io::WriteOutcome::Disconnect,
                "the event-capture peer remains connected",
            );
        }
        bytes.extend(kbd_map_drain(peer));
        bytes
    };
    let _ = drain_peer(&mut state, &mut peer);
    assert_eq!(
        backend.current_led_bits(),
        input::Led::CAPSLOCK.bits() | input::Led::NUMLOCK.bits()
    );
    assert_eq!(state.xi_devices.device(pointer).unwrap().buttons_down, 3);
    assert_eq!(state.xi_devices.device(4).unwrap().buttons_down, 2);
    assert!(
        state
            .key_down_by_device
            .get(&keyboard)
            .is_some_and(|held| held.contains_key(&SUPER_L))
    );

    // This is the production callback invoked for Message::VtRelease;
    // the empty headless fixture still exercises drive_vt_event and
    // run_suspend without opening DRM or switching a real VT.
    dispatch_vt_release(
        &mut state,
        &mut backend,
        &mut input_inventory,
        |_, _, _, _| {},
    );
    assert!(!input_inventory.get(SOURCE).unwrap().enabled);
    assert!(!state.xi_devices.source(SOURCE).unwrap().enabled);
    assert_eq!(state.xi_devices.device(keyboard).unwrap().id, keyboard);
    assert_eq!(state.xi_devices.device(pointer).unwrap().id, pointer);
    assert_eq!(state.xi_devices.device(pointer).unwrap().buttons_down, 0);
    assert!(!state.key_down_by_device.get(&keyboard).is_some_and(
        |held| held.contains_key(&CTRL_L)
            || held.contains_key(&ALT_L)
            || held.contains_key(&SUPER_L)
    ));
    assert!(backend.core.down_keys.contains(&SHIFT_L));
    assert_eq!(state.xi_devices.device(4).unwrap().buttons_down, 2);
    assert_eq!(state.key_down_by_device.get(&5).unwrap().len(), 1);
    // Xorg DisableDevice releases held input and clears the master's
    // lastSlave, then reports DeviceDisabled before floating the device
    // (devices.c:466-468, 488-492, 532-539). It does not deactivate the
    // explicit XI grab; XIUngrabDevice does that (xigrabdev.c:166-172).
    assert_eq!(
        state.xi_devices.device(pointer).unwrap().attached_master,
        None
    );
    assert!(state.xi2_pointer_grabs.contains_key(&pointer));
    assert!(
        state.clients[&CLIENT]
            .xi2_masks
            .contains_key(&(ROOT_WINDOW, keyboard))
    );
    assert!(
        state.clients[&CLIENT]
            .xi2_masks
            .contains_key(&(ROOT_WINDOW, pointer))
    );
    assert_eq!(
        backend.current_led_bits(),
        input::Led::CAPSLOCK.bits() | input::Led::NUMLOCK.bits()
    );

    let release_bytes = drain_peer(&mut state, &mut peer);
    let release_events = xi2_events(&release_bytes);
    for (kind, device, source, detail) in [
        (5, pointer, pointer, 1),
        (5, pointer, pointer, 2),
        (3, keyboard, keyboard, u32::from(CTRL_L)),
        (3, keyboard, keyboard, u32::from(ALT_L)),
        (3, keyboard, keyboard, u32::from(SUPER_L)),
    ] {
        assert_eq!(
            release_events
                .iter()
                .filter(|event| event.0 == kind
                    && event.1 == device
                    && event.2 == source
                    && event.3 == detail)
                .count(),
            1,
            "each physical facet release is delivered exactly once; events={release_events:?}",
        );
    }
    for keycode in [CTRL_L, ALT_L, SUPER_L] {
        assert_eq!(
            release_events
                .iter()
                .filter(|event| {
                    event.0 == 3
                        && event.1 == 3
                        && event.2 == keyboard
                        && event.3 == u32::from(keycode)
                })
                .count(),
            1,
            "each accepted physical key release reaches the master once",
        );
    }
    let last_button_release = release_events
        .iter()
        .rposition(|event| event.0 == 5)
        .expect("physical ButtonRelease events");
    let first_key_release = release_events
        .iter()
        .position(|event| event.0 == 3)
        .expect("physical KeyRelease events");
    assert!(
        last_button_release < first_key_release,
        "button cleanup precedes key cleanup per Xorg ReleaseButtonsAndKeys",
    );
    assert!(
        release_events.iter().all(|event| !(matches!(event.0, 3 | 5)
            && event.1 == event.2
            && matches!(event.1, 4 | 5))),
        "VT cleanup does not drain virtual XTEST facets",
    );
    assert!(
        release_events
            .iter()
            .all(|event| { !(matches!(event.0, 3 | 5) && event.1 == 3 && event.2 == 3) }),
        "no origin-less release is synthesized through the master as NestedHost"
    );
    assert!(backend.core.pending_pointer_events.is_empty());
    assert!(state.unpublished_keyboard_keys_down.is_empty());
    assert!(state.unpublished_pointer_buttons_down.is_empty());

    let paused_state = (
        backend.core.cursor_x,
        backend.core.cursor_y,
        backend.core.down_keys.clone(),
        backend.core.button_mask,
        state.keys_down,
        state.buttons_down,
        state.pointer_root,
        state.xi_devices.device(pointer).unwrap().buttons_down,
    );
    Backend::on_host_input(
        &mut backend,
        &mut state,
        key(InputOrigin::Physical(SOURCE), SHIFT_L, true),
    );
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::PointerMotion {
            origin: InputOrigin::Physical(SOURCE),
            x: 600,
            y: 400,
            time: 3,
            relative: false,
            dx: 0,
            dy: 0,
            motion_delta: None,
        },
    );
    assert_eq!(
        (
            backend.core.cursor_x,
            backend.core.cursor_y,
            backend.core.down_keys.clone(),
            backend.core.button_mask,
            state.keys_down,
            state.buttons_down,
            state.pointer_root,
            state.xi_devices.device(pointer).unwrap().buttons_down,
        ),
        paused_state,
        "ordinary physical input is rejected before KMS, XKB, and XI state changes",
    );
    assert!(drain_peer(&mut state, &mut peer).is_empty());

    // The input thread's delayed suspend notification is the same
    // production lifecycle message it emits after handling Pause.
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceSuspended { source_id: SOURCE },
    );
    assert!(drain_peer(&mut state, &mut peer).is_empty());
    assert_eq!(state.xi_devices.device(pointer).unwrap().buttons_down, 0);
    assert!(
        state
            .key_down_by_device
            .get(&keyboard)
            .is_none_or(std::collections::HashMap::is_empty)
    );

    // The production acquire callback runs the same VT state machine;
    // DeviceResumed is the input thread's restored-source message.
    dispatch_vt_acquire(&mut state, &mut backend);
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceResumed(info.clone()),
    );
    assert!(state.xi_devices.source(SOURCE).unwrap().enabled);
    assert_eq!(
        state.xi_devices.facet(SOURCE, XiFacetKind::Keyboard),
        Some(keyboard)
    );
    assert_eq!(
        state.xi_devices.facet(SOURCE, XiFacetKind::PointerTouch),
        Some(pointer)
    );
    assert_eq!(
        state.xi_devices.device(keyboard).unwrap().properties,
        keyboard_properties
    );
    assert_eq!(
        state.xi_devices.device(pointer).unwrap().properties,
        pointer_properties
    );
    assert_eq!(
        state
            .xi_devices
            .source(SOURCE)
            .unwrap()
            .config
            .accel
            .current,
        info.config.accel.current,
    );
    assert!(
        state.clients[&CLIENT]
            .xi2_masks
            .contains_key(&(ROOT_WINDOW, keyboard))
    );
    assert!(
        state.clients[&CLIENT]
            .xi2_masks
            .contains_key(&(ROOT_WINDOW, pointer))
    );
    assert_eq!(
        backend.current_led_bits(),
        input::Led::CAPSLOCK.bits() | input::Led::NUMLOCK.bits()
    );
    assert!(
        state
            .key_down_by_device
            .get(&5)
            .unwrap()
            .contains_key(&SHIFT_L)
    );
    assert_eq!(state.xi_devices.device(4).unwrap().buttons_down, 2);
    assert_eq!(state.buttons_down, 2, "virtual XTEST Button2 remains held");
    for keycode in [CTRL_L, ALT_L] {
        assert_eq!(
            state.keys_down[usize::from(keycode / 8)] & (1 << (keycode % 8)),
            0,
            "master key bitmap clears the released modifier",
        );
    }
    assert_ne!(
        state.keys_down[usize::from(SHIFT_L / 8)] & (1 << (SHIFT_L % 8)),
        0,
        "virtual XTEST Shift remains down on the master",
    );
    assert!(
        state.xi2_pointer_grabs.contains_key(&pointer),
        "the active explicit grab remains until XIUngrabDevice"
    );
    assert!(state.xi_devices.source(SOURCE).is_some());
    assert_eq!(state.xi_devices.devices().len(), 6);
    assert!(backend.core.pending_pointer_events.is_empty());
}

#[test]
fn xi_source_removal_vt_suspend_releases_unpublished_physical_source() {
    use yserver_core::{
        backend::Backend,
        core_loop::{
            DeviceInfo, HostInputEvent, InputInventory, InputOrigin,
            message::LibinputConfigSnapshot, run::dispatch_vt_release,
        },
        host_x11::HostKeyEvent,
        server::ServerState,
        xinput::{InputCapabilities, InputSourceId, XiFacetKind},
    };

    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    make_vt_fixture_headless(&mut backend);
    backend.vt_switching_armed = true;
    let mut input_inventory = InputInventory::new();
    let make_info = |source_id, keyboard, pointer| DeviceInfo {
        source_id,
        enabled: true,
        resume_key: None,
        capabilities: InputCapabilities {
            keyboard,
            pointer,
            touch: false,
        },
        name: format!("capacity source {}", source_id.0),
        device_node: format!("/dev/input/event{}", source_id.0),
        sysname: format!("event{}", source_id.0),
        vendor_id: 0,
        product_id: source_id.0 as u32,
        is_touchpad: false,
        config: LibinputConfigSnapshot::default(),
    };

    let published = InputSourceId(0xE200);
    let info = make_info(published, true, true);
    input_inventory.add(info.clone());
    Backend::on_host_input(&mut backend, &mut state, HostInputEvent::DeviceAdded(info));
    for id in 0..120 {
        let info = make_info(InputSourceId(0xE300 + id), false, true);
        input_inventory.add(info.clone());
        Backend::on_host_input(&mut backend, &mut state, HostInputEvent::DeviceAdded(info));
    }
    let unpublished = InputSourceId(0xE400);
    let info = make_info(unpublished, true, false);
    input_inventory.add(info.clone());
    Backend::on_host_input(&mut backend, &mut state, HostInputEvent::DeviceAdded(info));
    assert!(state.xi_devices.source(unpublished).is_some());
    assert_eq!(
        state.xi_devices.facet(unpublished, XiFacetKind::Keyboard),
        None
    );

    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::Key(HostKeyEvent {
            origin: InputOrigin::Physical(unpublished),
            pressed: true,
            keycode: 37,
            time: 1,
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            state: 0,
        }),
    );
    assert_eq!(
        state
            .unpublished_keyboard_keys_down
            .get(&unpublished)
            .unwrap()
            .len(),
        1,
        "production KMS input routes a published-source-less key to guarded internal state",
    );

    dispatch_vt_release(
        &mut state,
        &mut backend,
        &mut input_inventory,
        |_, _, _, _| {},
    );
    assert!(
        input_inventory
            .devices_by_source()
            .iter()
            .all(|info| !info.enabled)
    );
    assert!(
        !state
            .unpublished_keyboard_keys_down
            .contains_key(&unpublished)
    );
    assert!(!state.xi_devices.source(unpublished).unwrap().enabled);
    assert!(state.xi_devices.source(published).is_some());
    assert!(
        state
            .xi_devices
            .devices()
            .iter()
            .any(|device| device.source_id == Some(published))
    );
    assert_eq!(backend.core.down_keys.len(), 0);
    assert!(state.keys_down.iter().all(|byte| *byte == 0));
    assert!(backend.core.pending_pointer_events.is_empty());
}
