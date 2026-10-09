use super::*;

// ─── Stage 3f.7: input dispatch tests ───────────────────────

/// `serialize_modifiers` returns 0 against a fresh xkb_state
/// (no modifiers held). Regression gate for the bit layout.
#[test]
fn serialize_modifiers_zero_on_fresh_state() {
    let b = KmsBackend::for_tests();
    assert_eq!(b.serialize_modifiers(), 0);
}

/// `XkbGroupForCoreState`: the active locked group is stamped into
/// the cooked `state` bits 13-14. Group 0 sets no group bits;
/// group 1 sets bit 13 (`0x2000`).
#[test]
fn serialize_modifiers_encodes_locked_group() {
    let mut backend = KmsBackend::for_tests();
    // group 0 -> no group bits
    assert_eq!(backend.serialize_modifiers() & 0x6000, 0x0000);
    let changed = backend.core.recompile_keymap(&crate::kms::core::XkbRmlvo {
        rules: "evdev".into(),
        model: "pc105".into(),
        layout: "us,be".into(),
        variant: String::new(),
        options: None,
    });
    assert!(changed.is_some(), "us,be keymap must compile");
    backend.core.locked_group = 1;
    // group 1 -> bits 13-14 == 1 (XkbGroupForCoreState)
    assert_eq!(backend.serialize_modifiers() & 0x6000, 0x2000);
}

/// A stale or invalid locked group must not leak into core key-event
/// state when the active keymap only has group 0. Awesome treats the
/// group bits as part of the modifier state when matching global
/// keybindings, so a bogus `0x2000` makes every Mod4 binding miss.
#[test]
fn locked_group_is_clamped_to_keymap_group_count() {
    use yserver_core::backend::Backend;

    let mut backend = KmsBackend::for_tests();
    backend.core.locked_group = 1;
    assert_eq!(
        backend.serialize_modifiers() & 0x6000,
        0x0000,
        "single-layout keymap must not stamp stale group 1 into core state",
    );

    Backend::set_locked_group(&mut backend, 1);
    assert_eq!(
        Backend::current_group(&backend),
        0,
        "set_locked_group must clamp invalid group 1 to group 0",
    );
}

/// `cook_host_key` fills root + event coords from cursor and
/// stamps the modifier mask that was in effect *immediately
/// before* the event, per the X11 KeyPress/KeyRelease `state`
/// contract (`dix`: the state is computed before the key's own
/// action is applied). A modifier key's own press therefore
/// reports `state=0` (the modifier is not active yet); its
/// release reports the modifier still set (it was active until
/// this release). See the wire-confirmed i3 off-by-one.
#[test]
fn cook_host_key_reports_pre_event_modifier_state() {
    use yserver_core::host_x11::HostKeyEvent;
    let mut b = KmsBackend::for_tests();
    b.core.cursor_x = 100.0;
    b.core.cursor_y = 200.0;
    let key = |keycode: u8, pressed: bool| HostKeyEvent {
        origin: yserver_core::core_loop::InputOrigin::NestedHost,
        keycode,
        pressed,
        state: 0,
        root_x: 0,
        root_y: 0,
        event_x: 0,
        event_y: 0,
        time: 0,
    };
    // 50 == X keycode for Left Shift (evdev 42 + 8); xkbcommon's
    // default keymap maps it to the Shift modifier.
    let shift_press = b.cook_host_key(key(50, true));
    assert_eq!(shift_press.root_x, 100);
    assert_eq!(shift_press.root_y, 200);
    assert_eq!(shift_press.event_x, 100);
    assert_eq!(shift_press.event_y, 200);
    // Pre-event state: Shift is NOT yet active on its own press.
    assert_eq!(
        shift_press.state & 0x00ff,
        0,
        "a modifier key's own press must report pre-press state (Shift not yet set)"
    );
    // A non-modifier key pressed while Shift is held DOES carry
    // Shift (it was active before this key). 38 == X keycode 'a'.
    let a_press = b.cook_host_key(key(38, true));
    assert_eq!(
        a_press.state & 0x00ff,
        0x01,
        "a key pressed under Shift must carry the Shift bit"
    );
    // Releasing Shift reports it still set (active until this release).
    let shift_release = b.cook_host_key(key(50, false));
    assert_eq!(
        shift_release.state & 0x00ff,
        0x01,
        "a modifier key's own release must report pre-release state (Shift still set)"
    );
}

/// Regression guard for WM keybindings: direct-mode raw key input
/// must be cooked before core passive-grab routing, so a held
/// Super_L (Mod4) makes the following Return press match a
/// `GrabKey(Mod4+Return)` on the root window.
#[test]
fn on_host_input_super_return_activates_mod4_passive_grab() {
    use yserver_core::{
        core_loop::HostInputEvent,
        host_x11::HostKeyEvent,
        resources::ROOT_WINDOW,
        server::{ActiveKeyboardGrabSource, KeyGrab, ServerState},
    };
    use yserver_protocol::x11::ClientId;

    const WM: ClientId = ClientId(7);
    const SUPER_L: u8 = 133;
    const RETURN: u8 = 36;
    const MOD4: u16 = 0x40;

    let mut b = KmsBackend::for_tests();
    let mut state = ServerState::new();
    state.key_grabs.push(KeyGrab {
        device_id: 0,
        owner: WM,
        grab_window: ROOT_WINDOW,
        keycode: RETURN,
        modifiers: MOD4,
        owner_events: false,
        pointer_mode: 1,
        keyboard_mode: 1,
        via_xi2: false,
        xi2_mask: 0,
    });

    let key = |keycode, pressed| {
        HostInputEvent::Key(HostKeyEvent {
            origin: yserver_core::core_loop::InputOrigin::NestedHost,
            keycode,
            pressed,
            state: 0,
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            time: 0,
        })
    };

    b.on_host_input(&mut state, key(SUPER_L, true));
    assert_eq!(
        b.serialize_modifiers() & MOD4,
        MOD4,
        "Super_L press must set the backend's effective Mod4 bit"
    );

    b.on_host_input(&mut state, key(RETURN, true));
    match state.active_keyboard_grab {
        Some(grab) => {
            assert_eq!(grab.owner, WM);
            assert_eq!(grab.grab_window, ROOT_WINDOW);
            match grab.source {
                ActiveKeyboardGrabSource::PassiveKey { keycode } => {
                    assert_eq!(keycode, RETURN);
                }
                other => panic!("expected PassiveKey grab source, got {other:?}"),
            }
        }
        None => panic!("Mod4+Return must activate the WM passive key grab"),
    }
}

/// Issue #168: `xdotool key super+5` sends Super_L press THREE
/// times, then 5, then Super_L release only TWICE, then 5's release
/// (XTEST FakeInput sequence captured from yserver's log in the vng
/// `xtest-super-stuck` scenario). Xorg drops a press of a key that
/// is already down and a release of a key that is not
/// (`Xi/exevents.c` "don't allow ddx to generate multiple downs" /
/// "guard against duplicates"), so neither reaches XKB. Fed to
/// xkbcommon, three downs against two ups left Mod4 set after every
/// key was up: Super stuck system-wide until a VT switch.
#[test]
fn duplicate_key_press_and_release_do_not_stick_modifier() {
    use yserver_core::{core_loop::HostInputEvent, host_x11::HostKeyEvent, server::ServerState};

    const SUPER_L: u8 = 133;
    const FIVE: u8 = 14;
    const MOD4: u16 = 0x40;

    let mut b = KmsBackend::for_tests();
    let mut state = ServerState::new();
    let key = |keycode, pressed| {
        HostInputEvent::Key(HostKeyEvent {
            origin: yserver_core::core_loop::InputOrigin::NestedHost,
            keycode,
            pressed,
            state: 0,
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            time: 0,
        })
    };

    for (keycode, pressed) in [
        (SUPER_L, true),
        (SUPER_L, true),
        (SUPER_L, true),
        (FIVE, true),
        (SUPER_L, false),
        (SUPER_L, false),
        (FIVE, false),
    ] {
        b.on_host_input(&mut state, key(keycode, pressed));
    }

    assert_eq!(
        b.serialize_modifiers() & MOD4,
        0,
        "Super_L must not stay latched once every key is released"
    );
    assert!(b.core.down_keys.is_empty(), "no key may remain held");
    assert!(
        state.keys_down.iter().all(|&byte| byte == 0),
        "QueryKeymap must report no held keys"
    );
}

#[test]
fn xi_dynamic_keyboard_release_after_grab_clears_the_slave_hold() {
    use yserver_core::{
        backend::Backend,
        core_loop::{HostInputEvent, InputOrigin},
        host_x11::HostKeyEvent,
        server::ServerState,
        xinput::{InputSourceId, XiFacetKind},
    };

    const SOURCE: InputSourceId = InputSourceId(0xA201);
    const CLIENT: u32 = 71;
    const KEYCODE: u8 = 50;
    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    let mut peer = kbd_map_client_id(&mut state, CLIENT);
    backend.on_host_input(
        &mut state,
        HostInputEvent::DeviceAdded(dynamic_test_device(SOURCE, true, false)),
    );
    let device_id = state
        .xi_devices
        .facet(SOURCE, XiFacetKind::Keyboard)
        .expect("DeviceAdded publishes the keyboard facet");
    let key = |pressed| {
        HostInputEvent::Key(HostKeyEvent {
            origin: InputOrigin::Physical(SOURCE),
            pressed,
            keycode: KEYCODE,
            time: 0,
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            state: 0,
        })
    };

    backend.on_host_input(&mut state, key(true));
    assert!(state.key_down_by_device[&device_id].contains_key(&KEYCODE));
    let master_key_bit = 1u8 << (KEYCODE % 8);
    assert_ne!(
        state.keys_down[usize::from(KEYCODE / 8)] & master_key_bit,
        0
    );

    process_dynamic_test_keyboard_grab(&mut backend, &mut state, CLIENT, device_id, 1);
    assert_eq!(
        state.xi_devices.device(device_id).unwrap().attached_master,
        None
    );
    assert!(state.xi2_keyboard_grabs.contains_key(&device_id));
    let _ = kbd_map_drain(&mut peer);

    backend.on_host_input(&mut state, key(false));

    assert!(
        !state
            .key_down_by_device
            .get(&device_id)
            .is_some_and(|held| held.contains_key(&KEYCODE)),
        "the physical slave's held key clears when its floating release is accepted",
    );
    assert!(
        backend.floating_keyboard_states[&device_id]
            .down_keys
            .is_empty()
    );
    assert!(
        state.xi2_keyboard_grabs.contains_key(&device_id),
        "the release does not end an explicit active grab",
    );
    assert_eq!(
        state.xi_devices.device(device_id).unwrap().attached_master,
        None
    );
    assert!(state.sync_pending.is_empty());
    assert_ne!(
        state.keys_down[usize::from(KEYCODE / 8)] & master_key_bit,
        0,
        "Xorg's floating slave release clears its own key bitmap; the detached master bitmap remains as it was",
    );
    let release_wire = kbd_map_drain_until(&mut peer, |bytes| {
        xi2_events(bytes)
            .iter()
            .any(|event| event.0 == 3 && event.1 == device_id && event.3 == u32::from(KEYCODE))
    });
    let release_events = xi2_events(&release_wire);
    assert_eq!(
        release_events
            .iter()
            .filter(|event| event.0 == 3 && event.1 == device_id && event.3 == u32::from(KEYCODE))
            .count(),
        1,
        "the active slave grab receives the floating KeyRelease: events={release_events:?}, bytes={}, buffered={}, freeze={:?}, grab={:?}",
        release_wire.len(),
        state.clients[&CLIENT].outbound.len(),
        state.xi1_frozen.get(&device_id),
        state.xi2_keyboard_grabs.get(&device_id),
    );

    process_dynamic_test_keyboard_ungrab(&mut backend, &mut state, CLIENT, device_id, 2);
    assert!(!state.xi2_keyboard_grabs.contains_key(&device_id));
    assert_eq!(
        state.xi_devices.device(device_id).unwrap().attached_master,
        Some(yserver_core::xinput::DEVICEID_MASTER_KEYBOARD),
    );
    assert!(!backend.floating_keyboard_states.contains_key(&device_id));
    assert!(state.sync_pending.is_empty());
    assert!(!state.key_down_by_device.contains_key(&device_id));
}

#[test]
fn xi_dynamic_keyboard_removal_releases_a_held_key_before_unregister() {
    use yserver_core::{
        backend::Backend,
        core_loop::HostInputEvent,
        host_x11::HostKeyEvent,
        server::ServerState,
        xinput::{InputSourceId, XiFacetKind},
    };

    const SOURCE: InputSourceId = InputSourceId(0xA202);
    const CLIENT: u32 = 72;
    const KEYCODE: u8 = 50;
    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    let mut peer = kbd_map_client_id(&mut state, CLIENT);
    backend.on_host_input(
        &mut state,
        HostInputEvent::DeviceAdded(dynamic_test_device(SOURCE, true, false)),
    );
    let device_id = state
        .xi_devices
        .facet(SOURCE, XiFacetKind::Keyboard)
        .expect("DeviceAdded publishes the keyboard facet");
    backend.on_host_input(
        &mut state,
        HostInputEvent::Key(HostKeyEvent {
            origin: yserver_core::core_loop::InputOrigin::Physical(SOURCE),
            pressed: true,
            keycode: KEYCODE,
            time: 0,
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            state: 0,
        }),
    );
    process_dynamic_test_keyboard_grab(&mut backend, &mut state, CLIENT, device_id, 1);
    let _ = kbd_map_drain(&mut peer);

    backend.on_host_input(
        &mut state,
        HostInputEvent::DeviceRemoved { source_id: SOURCE },
    );

    let release_wire = kbd_map_drain_until(&mut peer, |bytes| {
        xi2_events(bytes)
            .iter()
            .any(|event| event.0 == 3 && event.1 == device_id && event.3 == u32::from(KEYCODE))
    });
    let events = xi2_events(&release_wire);
    assert_eq!(
        events
            .iter()
            .filter(|event| event.0 == 3 && event.1 == device_id && event.3 == u32::from(KEYCODE))
            .count(),
        1,
        "ReleaseButtonsAndKeys sends a release through the live slave grab before teardown",
    );
    assert!(state.xi_devices.source(SOURCE).is_none());
    assert!(state.xi_devices.device(device_id).is_none());
    assert!(!state.key_down_by_device.contains_key(&device_id));
    assert!(!state.xi2_keyboard_grabs.contains_key(&device_id));
    assert!(!state.xi2_detached_masters.contains_key(&device_id));
    assert!(!state.xi1_frozen.contains_key(&device_id));
    assert!(state.sync_pending.is_empty());
    assert!(!backend.floating_keyboard_states.contains_key(&device_id));
}

#[test]
fn xi_registry_unregister_drops_a_real_facets_held_key_bitmap() {
    use yserver_core::{
        backend::Backend,
        core_loop::HostInputEvent,
        host_x11::HostKeyEvent,
        server::ServerState,
        xinput::{InputSourceId, XiFacetKind},
    };

    const SOURCE: InputSourceId = InputSourceId(0xA20A);
    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    backend.on_host_input(
        &mut state,
        HostInputEvent::DeviceAdded(dynamic_test_device(SOURCE, true, false)),
    );
    let device_id = state
        .xi_devices
        .facet(SOURCE, XiFacetKind::Keyboard)
        .expect("DeviceAdded publishes the keyboard facet");
    backend.on_host_input(
        &mut state,
        HostInputEvent::Key(HostKeyEvent {
            origin: yserver_core::core_loop::InputOrigin::Physical(SOURCE),
            pressed: true,
            keycode: 50,
            time: 0,
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            state: 0,
        }),
    );
    assert!(state.key_down_by_device[&device_id].contains_key(&50));

    let removed_ids = state.xi_unregister_source(SOURCE);

    assert!(removed_ids.contains(&device_id));
    assert!(state.xi_devices.source(SOURCE).is_none());
    assert!(!state.key_down_by_device.contains_key(&device_id));
    assert!(!state.unpublished_keyboard_keys_down.contains_key(&SOURCE));
}

#[test]
fn xi_dynamic_pointer_mapping_cleanup_releases_the_held_logical_button() {
    use yserver_core::{
        backend::Backend,
        core_loop::{HostInputEvent, InputOrigin},
        server::ServerState,
        xinput::{InputSourceId, XiFacetKind},
    };
    use yserver_protocol::x11::{ClientId, RequestHeader, SequenceNumber};

    const SOURCE: InputSourceId = InputSourceId(0xA203);
    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    let _peer = kbd_map_client_id(&mut state, 73);
    // PickPointer is the ten-button CorePointer (`dix/devices.c:662-690`),
    // so SetPointerMapping must supply all ten entries.
    let mapping = [3u8, 2, 1, 4, 5, 6, 7, 8, 9, 10];
    yserver_core::core_loop::process_request::process_request(
        &mut state,
        &mut backend as &mut dyn Backend,
        ClientId(73),
        SequenceNumber(1),
        RequestHeader {
            opcode: 116,
            data: u8::try_from(mapping.len()).unwrap(),
            length_units: 4,
        },
        &mapping,
        None,
    )
    .expect("SetPointerMapping through process_request");
    backend.on_host_input(
        &mut state,
        HostInputEvent::DeviceAdded(dynamic_test_device(SOURCE, false, true)),
    );
    let device_id = state
        .xi_devices
        .facet(SOURCE, XiFacetKind::PointerTouch)
        .expect("DeviceAdded publishes the pointer facet");

    backend.on_host_input(
        &mut state,
        HostInputEvent::PointerButton {
            origin: InputOrigin::Physical(SOURCE),
            button: 0x110, // physical Button1 maps to logical Button3
            pressed: true,
            time: 0,
        },
    );
    assert_eq!(
        state.xi_devices.device(device_id).unwrap().buttons_down,
        1 << 2
    );
    assert_eq!(state.buttons_down, 1 << 2);

    backend.on_host_input(
        &mut state,
        HostInputEvent::DeviceRemoved { source_id: SOURCE },
    );

    assert!(state.xi_devices.source(SOURCE).is_none());
    assert!(state.xi_devices.device(device_id).is_none());
    assert_eq!(
        state.buttons_down, 0,
        "the logical master Button3 hold is released"
    );
    assert!(state.sync_pending.is_empty());
    assert!(!state.xi2_pointer_grabs.contains_key(&device_id));
    assert!(!state.xi2_detached_masters.contains_key(&device_id));
}

#[test]
fn xtest_key_input_to_a_floating_keyboard_does_not_change_master_state() {
    use yserver_core::{
        backend::Backend,
        core_loop::{HostInputEvent, InputOrigin},
        host_x11::HostKeyEvent,
        server::ServerState,
        xinput::{InputSourceId, XiFacetKind},
    };

    const FLOATING: InputSourceId = InputSourceId(0xA204);
    const ATTACHED: InputSourceId = InputSourceId(0xA205);
    const CLIENT: u32 = 74;
    const KEYCODE: u8 = 38;
    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    let mut peer = kbd_map_client_id(&mut state, CLIENT);
    for source in [FLOATING, ATTACHED] {
        backend.on_host_input(
            &mut state,
            HostInputEvent::DeviceAdded(dynamic_test_device(source, true, false)),
        );
    }
    let floating_id = state
        .xi_devices
        .facet(FLOATING, XiFacetKind::Keyboard)
        .unwrap();
    let attached_id = state
        .xi_devices
        .facet(ATTACHED, XiFacetKind::Keyboard)
        .unwrap();
    backend.on_host_input(
        &mut state,
        HostInputEvent::Key(HostKeyEvent {
            origin: InputOrigin::Physical(FLOATING),
            pressed: true,
            keycode: 50,
            time: 0,
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            state: 0,
        }),
    );
    process_dynamic_test_keyboard_grab(&mut backend, &mut state, CLIENT, floating_id, 1);
    let _ = kbd_map_drain(&mut peer);

    let key = |origin, pressed| {
        HostInputEvent::Key(HostKeyEvent {
            origin,
            pressed,
            keycode: KEYCODE,
            time: 0,
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            state: 0,
        })
    };
    backend.on_host_input(&mut state, key(InputOrigin::XTest(floating_id), true));
    let master_key_bit = 1u8 << (KEYCODE % 8);
    assert_eq!(
        state.keys_down[usize::from(KEYCODE / 8)] & master_key_bit,
        0,
        "XTEST's floating slave key is absent from QueryKeymap's master bitmap",
    );
    assert!(
        backend.floating_keyboard_states[&floating_id]
            .down_keys
            .contains(&KEYCODE)
    );
    assert!(state.key_down_by_device[&floating_id].contains_key(&KEYCODE));

    backend.on_host_input(&mut state, key(InputOrigin::Physical(ATTACHED), true));
    assert!(
        backend.core.down_keys.contains(&KEYCODE),
        "the next attached keyboard press is accepted by the master XKB state",
    );
    assert!(state.key_down_by_device[&attached_id].contains_key(&KEYCODE));
    assert!(state.xi2_keyboard_grabs.contains_key(&floating_id));
    assert_eq!(
        state
            .xi_devices
            .device(floating_id)
            .unwrap()
            .attached_master,
        None
    );
    assert!(state.sync_pending.is_empty());

    backend.on_host_input(&mut state, key(InputOrigin::Physical(ATTACHED), false));
    backend.on_host_input(&mut state, key(InputOrigin::XTest(floating_id), false));
    assert!(!state.key_down_by_device.contains_key(&attached_id));
    assert!(
        !state
            .key_down_by_device
            .get(&floating_id)
            .is_some_and(|held| held.contains_key(&KEYCODE))
    );
}

#[test]
fn record_contains_only_accepted_master_key_and_button_transitions() {
    use yserver_core::{
        backend::Backend,
        core_loop::{HostInputEvent, InputOrigin},
        host_x11::HostKeyEvent,
        server::ServerState,
        xinput::InputSourceId,
    };

    const A_KBD: InputSourceId = InputSourceId(0xA206);
    const B_KBD: InputSourceId = InputSourceId(0xA207);
    const A_PTR: InputSourceId = InputSourceId(0xA208);
    const B_PTR: InputSourceId = InputSourceId(0xA209);
    const FLOATING_KBD: InputSourceId = InputSourceId(0xA20B);
    const GRAB_CLIENT: u32 = 76;
    const KEYCODE: u8 = 38;
    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    let mut peer = kbd_map_client_id(&mut state, 5);
    let mut grab_peer = kbd_map_client_id(&mut state, GRAB_CLIENT);

    // RECORD CreateContext(ctx=1, FutureClients, core KeyPress..ButtonRelease)
    // followed by EnableContext, through the normal request dispatcher.
    let mut create = Vec::new();
    for word in [1u32, 0, 1, 1, 2] {
        create.extend_from_slice(&word.to_le_bytes());
    }
    create.extend_from_slice(&[0; 18]);
    create.extend_from_slice(&[2, 5, 0, 0, 0, 0]);
    kbd_map_request(&mut state, &mut backend, 154, 1, &create);
    kbd_map_request(&mut state, &mut backend, 154, 5, &1u32.to_le_bytes());
    let _ = kbd_map_drain(&mut peer);

    for (source, keyboard, pointer) in [
        (A_KBD, true, false),
        (B_KBD, true, false),
        (A_PTR, false, true),
        (B_PTR, false, true),
        (FLOATING_KBD, true, false),
    ] {
        backend.on_host_input(
            &mut state,
            HostInputEvent::DeviceAdded(dynamic_test_device(source, keyboard, pointer)),
        );
    }
    let key = |source, pressed| {
        HostInputEvent::Key(HostKeyEvent {
            origin: InputOrigin::Physical(source),
            pressed,
            keycode: KEYCODE,
            time: 0,
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            state: 0,
        })
    };
    backend.on_host_input(&mut state, key(A_KBD, true));
    backend.on_host_input(&mut state, key(B_KBD, true));
    backend.on_host_input(&mut state, key(A_KBD, false));
    backend.on_host_input(&mut state, key(B_KBD, false));
    let bytes = kbd_map_drain(&mut peer);
    let mut key_records = Vec::new();
    let mut at = 0usize;
    while at + 32 <= bytes.len() {
        let words = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
        let len = 32 + words * 4;
        assert!(at + len <= bytes.len(), "complete RECORD stream element");
        if bytes[at] == 1 && bytes[at + 1] == 0 {
            let event = &bytes[at + 32..at + len];
            if matches!(event[0], 2 | 3) {
                key_records.push((event[0], event[1]));
            }
        }
        at += len;
    }
    assert_eq!(key_records, [(2, KEYCODE), (3, KEYCODE)]);

    let floating_id = state
        .xi_devices
        .facet(FLOATING_KBD, yserver_core::xinput::XiFacetKind::Keyboard)
        .unwrap();
    process_dynamic_test_keyboard_grab(&mut backend, &mut state, GRAB_CLIENT, floating_id, 1);
    let _ = kbd_map_drain(&mut grab_peer);
    for pressed in [true, false] {
        backend.on_host_input(
            &mut state,
            HostInputEvent::Key(HostKeyEvent {
                origin: InputOrigin::XTest(floating_id),
                pressed,
                keycode: KEYCODE,
                time: 0,
                root_x: 0,
                root_y: 0,
                event_x: 0,
                event_y: 0,
                state: 0,
            }),
        );
    }
    let bytes = kbd_map_drain(&mut peer);
    let mut floating_key_records = Vec::new();
    let mut at = 0usize;
    while at + 32 <= bytes.len() {
        let words = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
        let len = 32 + words * 4;
        assert!(at + len <= bytes.len(), "complete RECORD stream element");
        if bytes[at] == 1 && bytes[at + 1] == 0 {
            let event = &bytes[at + 32..at + len];
            if matches!(event[0], 2 | 3) {
                floating_key_records.push((event[0], event[1]));
            }
        }
        at += len;
    }
    assert!(
        floating_key_records.is_empty(),
        "floating slave key edges have no core RECORD forms",
    );

    let button = |source, pressed| HostInputEvent::PointerButton {
        origin: InputOrigin::Physical(source),
        button: 0x110,
        pressed,
        time: 0,
    };
    backend.on_host_input(&mut state, button(A_PTR, true));
    backend.on_host_input(&mut state, button(B_PTR, true));
    backend.on_host_input(&mut state, button(A_PTR, false));
    backend.on_host_input(&mut state, button(B_PTR, false));
    let bytes = kbd_map_drain(&mut peer);
    let mut button_records = Vec::new();
    let mut at = 0usize;
    while at + 32 <= bytes.len() {
        let words = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
        let len = 32 + words * 4;
        assert!(at + len <= bytes.len(), "complete RECORD stream element");
        if bytes[at] == 1 && bytes[at + 1] == 0 {
            let event = &bytes[at + 32..at + len];
            if matches!(event[0], 4 | 5) {
                button_records.push((event[0], event[1]));
            }
        }
        at += len;
    }
    assert_eq!(button_records, [(4, 1), (5, 1)]);
    assert_eq!(state.buttons_down, 0);
    assert!(state.key_down_by_device.is_empty());
    assert!(state.sync_pending.is_empty());
}

/// Nested host keys produce master-only XI2 raw/device events. Their
/// sourceid is the master keyboard itself (3), never the virtual XTEST
/// keyboard (5). Raw remains before the device event with the same
/// timestamp, matching Xorg's GetKeyboardEvents ordering.
#[test]
fn device_key_emits_raw_key_events_before_the_device_event() {
    use yserver_core::{
        core_loop::HostInputEvent, host_x11::HostKeyEvent, resources::ROOT_WINDOW,
        server::ServerState,
    };
    let mut b = KmsBackend::for_tests();
    let mut state = ServerState::new();
    let mut peer = kbd_map_client_id(&mut state, 9);
    let client = state.clients.get_mut(&9).unwrap();
    client
        .xi2_masks
        .insert((ROOT_WINDOW, 1), (1 << 13) | (1 << 14));
    client
        .xi2_masks
        .insert((ROOT_WINDOW, 3), (1 << 2) | (1 << 3));
    let key = |pressed| {
        HostInputEvent::Key(HostKeyEvent {
            origin: yserver_core::core_loop::InputOrigin::NestedHost,
            keycode: 71, // F5, as in the report
            pressed,
            state: 0,
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            time: 0,
        })
    };

    b.on_host_input(&mut state, key(true));
    b.on_host_input(&mut state, key(false));

    let events = xi2_events(&kbd_map_drain(&mut peer));
    let kinds: Vec<_> = events.iter().map(|e| (e.0, e.1, e.2, e.3)).collect();
    assert_eq!(
        kinds,
        vec![(13, 3, 3, 71), (2, 3, 3, 71), (14, 3, 3, 71), (3, 3, 3, 71)],
        "nested RawKeyPress, KeyPress, RawKeyRelease, KeyRelease"
    );
    assert_eq!(
        events[0].4, events[1].4,
        "raw press and KeyPress share one time"
    );
    assert_eq!(
        events[2].4, events[3].4,
        "raw release and KeyRelease share one time"
    );
}

#[test]
fn key_source_routing_kms_preserves_facets_and_drops_unknown_before_xkb() {
    use yserver_core::{
        backend::Backend,
        core_loop::{DeviceInfo, HostInputEvent, InputOrigin, message::LibinputConfigSnapshot},
        host_x11::HostKeyEvent,
        resources::ROOT_WINDOW,
        server::ServerState,
        xinput::{InputCapabilities, InputSourceId, XiFacetKind},
    };

    const RAZER: InputSourceId = InputSourceId(0xA11);
    const HYPERX: InputSourceId = InputSourceId(0xA12);
    const RETIRED: InputSourceId = InputSourceId(0xA13);

    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    state.core_focus.raw = ROOT_WINDOW.0;

    let key = |origin, keycode, pressed| {
        HostInputEvent::Key(HostKeyEvent {
            origin,
            keycode,
            pressed,
            time: 0,
            root_x: 10,
            root_y: 20,
            event_x: 10,
            event_y: 20,
            state: 0,
        })
    };
    let device_info = |source_id, name: &str| DeviceInfo {
        source_id,
        enabled: true,
        resume_key: None,
        capabilities: InputCapabilities {
            keyboard: true,
            pointer: false,
            touch: false,
        },
        name: name.to_owned(),
        device_node: format!("/dev/input/{name}"),
        sysname: name.to_owned(),
        vendor_id: 0,
        product_id: 0,
        is_touchpad: false,
        config: LibinputConfigSnapshot::default(),
    };

    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceAdded(device_info(RETIRED, "Retired keyboard")),
    );
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceRemoved { source_id: RETIRED },
    );

    // An unknown source must not reach raw fanout or advance KMS/XKB.
    let initial_modifiers = backend.serialize_modifiers();
    Backend::on_host_input(
        &mut backend,
        &mut state,
        key(InputOrigin::Physical(RETIRED), 50, true),
    );
    assert!(backend.core.down_keys.is_empty());
    assert!(state.keys_down.iter().all(|byte| *byte == 0));
    assert_eq!(backend.serialize_modifiers(), initial_modifiers);

    for (source_id, name) in [(RAZER, "Razer"), (HYPERX, "HyperX")] {
        Backend::on_host_input(
            &mut backend,
            &mut state,
            HostInputEvent::DeviceAdded(device_info(source_id, name)),
        );
    }
    let razer_id = state
        .xi_devices
        .facet(RAZER, XiFacetKind::Keyboard)
        .expect("Razer keyboard facet");
    let hyperx_id = state
        .xi_devices
        .facet(HYPERX, XiFacetKind::Keyboard)
        .expect("HyperX keyboard facet");
    assert_ne!(razer_id, hyperx_id);

    let suspended_info = device_info(HYPERX, "HyperX");
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceSuspended { source_id: HYPERX },
    );
    let suspended_modifiers = backend.serialize_modifiers();
    Backend::on_host_input(
        &mut backend,
        &mut state,
        key(InputOrigin::Physical(HYPERX), 50, true),
    );
    assert!(backend.core.down_keys.is_empty());
    assert!(state.keys_down.iter().all(|byte| *byte == 0));
    assert_eq!(backend.serialize_modifiers(), suspended_modifiers);
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceResumed(suspended_info),
    );
    assert!(state.xi_devices.source(HYPERX).unwrap().enabled);
    assert_eq!(
        state.xi_devices.facet(HYPERX, XiFacetKind::Keyboard),
        Some(hyperx_id),
        "resume preserves the same facet ID"
    );

    for source_id in [RAZER, HYPERX] {
        Backend::on_host_input(
            &mut backend,
            &mut state,
            key(InputOrigin::Physical(source_id), 38, true),
        );
        Backend::on_host_input(
            &mut backend,
            &mut state,
            key(InputOrigin::Physical(source_id), 38, false),
        );
    }

    for origin in [InputOrigin::XTest(5), InputOrigin::NestedHost] {
        Backend::on_host_input(&mut backend, &mut state, key(origin, 38, true));
        Backend::on_host_input(&mut backend, &mut state, key(origin, 38, false));
    }
    assert!(backend.core.down_keys.is_empty());
    assert!(state.keys_down.iter().all(|byte| *byte == 0));
    assert!(state.key_repeats.is_empty());
    assert!(state.sync_pending.is_empty());
    assert!(state.xi_devices.source(RETIRED).is_none());
    assert!(state.xi_devices.source(RAZER).is_some());
    assert!(state.xi_devices.source(HYPERX).is_some());
}

#[test]
fn key_source_selection_keeps_slave_holds_independent_from_master_transitions() {
    use yserver_core::{
        backend::Backend,
        core_loop::{DeviceInfo, HostInputEvent, InputOrigin, message::LibinputConfigSnapshot},
        host_x11::HostKeyEvent,
        resources::ROOT_WINDOW,
        server::ServerState,
        xinput::{InputCapabilities, InputSourceId, XiFacetKind},
    };

    const A: InputSourceId = InputSourceId(0xB11);
    const B: InputSourceId = InputSourceId(0xB12);
    const SHIFT_L: u8 = 50;
    const A_KEY: u8 = 38;
    const KEY_MASK: u64 = (1 << 2) | (1 << 3);
    const SHIFT_MASK: u16 = 1;

    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    state.core_focus.raw = ROOT_WINDOW.0;
    let mut peer_a = kbd_map_client_id(&mut state, 31);
    let mut peer_b = kbd_map_client_id(&mut state, 32);
    let mut master_peer = kbd_map_client_id(&mut state, 33);
    let mut core_peer = kbd_map_client_id(&mut state, 34);
    for (source, name) in [(A, "keyboard A"), (B, "keyboard B")] {
        backend.on_host_input(
            &mut state,
            HostInputEvent::DeviceAdded(DeviceInfo {
                source_id: source,
                enabled: true,
                resume_key: None,
                capabilities: InputCapabilities {
                    keyboard: true,
                    pointer: false,
                    touch: false,
                },
                name: name.to_owned(),
                device_node: format!("/dev/input/{name}"),
                sysname: name.to_owned(),
                vendor_id: 0,
                product_id: 0,
                is_touchpad: false,
                config: LibinputConfigSnapshot::default(),
            }),
        );
    }
    let a_id = state
        .xi_devices
        .facet(A, XiFacetKind::Keyboard)
        .expect("keyboard A facet");
    let b_id = state
        .xi_devices
        .facet(B, XiFacetKind::Keyboard)
        .expect("keyboard B facet");
    for (client, device) in [(31, a_id), (32, b_id)] {
        state
            .clients
            .get_mut(&client)
            .unwrap()
            .xi2_masks
            .insert((ROOT_WINDOW, device), KEY_MASK);
    }
    state
        .clients
        .get_mut(&33)
        .unwrap()
        .xi2_masks
        .insert((ROOT_WINDOW, 3), KEY_MASK);
    state
        .clients
        .get_mut(&34)
        .unwrap()
        .event_masks
        .insert(ROOT_WINDOW, 0x0000_0001 | 0x0000_0002);

    let key = |origin, keycode, pressed| {
        HostInputEvent::Key(HostKeyEvent {
            origin,
            pressed,
            keycode,
            time: 0,
            root_x: 10,
            root_y: 20,
            event_x: 10,
            event_y: 20,
            state: 0,
        })
    };
    let send = |backend: &mut KmsBackend, state: &mut ServerState, origin, keycode, pressed| {
        Backend::on_host_input(backend, state, key(origin, keycode, pressed));
    };

    send(
        &mut backend,
        &mut state,
        InputOrigin::Physical(A),
        SHIFT_L,
        true,
    );
    send(
        &mut backend,
        &mut state,
        InputOrigin::Physical(A),
        SHIFT_L,
        true,
    );
    send(
        &mut backend,
        &mut state,
        InputOrigin::Physical(B),
        SHIFT_L,
        true,
    );
    assert_ne!(backend.serialize_modifiers() & SHIFT_MASK, 0);
    let a_press = xi2_events(&kbd_map_drain(&mut peer_a));
    let b_press = xi2_events(&kbd_map_drain(&mut peer_b));
    let master_press = xi2_events(&kbd_map_drain(&mut master_peer));
    assert_eq!(
        a_press
            .iter()
            .filter(|event| event.0 == 2)
            .map(|event| event.1)
            .collect::<Vec<_>>(),
        vec![a_id],
        "keyboard A gets its slave KeyPress",
    );
    assert_eq!(
        b_press
            .iter()
            .filter(|event| event.0 == 2)
            .map(|event| event.1)
            .collect::<Vec<_>>(),
        vec![b_id],
        "keyboard B's slave KeyPress remains deliverable while the master is already down",
    );
    assert_eq!(
        master_press
            .iter()
            .filter(|event| event.0 == 2)
            .map(|event| (event.1, event.2))
            .collect::<Vec<_>>(),
        vec![(3, a_id)],
        "the master's own down guard suppresses keyboard B's duplicate press",
    );

    send(
        &mut backend,
        &mut state,
        InputOrigin::Physical(A),
        SHIFT_L,
        false,
    );
    assert_eq!(
        backend.serialize_modifiers() & SHIFT_MASK,
        0,
        "the first valid slave release clears master Shift while B still holds Shift",
    );
    let _ = kbd_map_drain(&mut peer_a);
    let a_release_master = xi2_events(&kbd_map_drain(&mut master_peer));
    assert_eq!(
        a_release_master
            .iter()
            .filter(|event| event.0 == 3)
            .map(|event| (event.1, event.2))
            .collect::<Vec<_>>(),
        vec![(3, a_id)],
    );

    send(
        &mut backend,
        &mut state,
        InputOrigin::Physical(A),
        A_KEY,
        true,
    );
    let core_key = kbd_map_drain(&mut core_peer);
    let core_press = core_key
        .chunks_exact(32)
        .find(|event| event[0] & 0x7f == 2 && event[1] == A_KEY)
        .expect("core A KeyPress");
    assert_eq!(
        u16::from_le_bytes([core_press[28], core_press[29]]) & SHIFT_MASK,
        0,
        "core key after A's release is unshifted although B still physically holds Shift",
    );
    send(
        &mut backend,
        &mut state,
        InputOrigin::Physical(A),
        A_KEY,
        false,
    );
    let _ = kbd_map_drain(&mut core_peer);

    send(
        &mut backend,
        &mut state,
        InputOrigin::Physical(B),
        SHIFT_L,
        false,
    );
    send(
        &mut backend,
        &mut state,
        InputOrigin::Physical(B),
        SHIFT_L,
        false,
    );
    let b_release = xi2_events(&kbd_map_drain(&mut peer_b));
    assert_eq!(
        b_release
            .iter()
            .filter(|event| event.0 == 3)
            .map(|event| event.1)
            .collect::<Vec<_>>(),
        vec![b_id],
        "keyboard B receives its own release after A already released the master key",
    );
    let b_master_tail = xi2_events(&kbd_map_drain(&mut master_peer));
    assert!(
        b_master_tail
            .iter()
            .all(|event| { !(event.0 == 3 && event.2 == b_id && event.3 == u32::from(SHIFT_L)) }),
        "B's release cannot release the already-up master Shift a second time: {b_master_tail:?}",
    );
    assert_eq!(backend.serialize_modifiers() & SHIFT_MASK, 0);
    assert!(backend.core.down_keys.is_empty());
    assert!(state.keys_down.iter().all(|byte| *byte == 0));
    assert!(state.key_down_by_device.is_empty());
    assert!(state.unpublished_keyboard_keys_down.is_empty());
    assert!(state.sync_pending.is_empty());
    assert_eq!(state.xi_devices.facet(A, XiFacetKind::Keyboard), Some(a_id));
    assert_eq!(state.xi_devices.facet(B, XiFacetKind::Keyboard), Some(b_id));
}

#[test]
fn key_source_selection_tracks_xtest_targeted_keyboard_facets() {
    use yserver_core::{
        backend::Backend,
        core_loop::{DeviceInfo, HostInputEvent, InputOrigin, message::LibinputConfigSnapshot},
        host_x11::HostKeyEvent,
        resources::ROOT_WINDOW,
        server::ServerState,
        xinput::{InputCapabilities, InputSourceId, XiFacetKind},
    };

    const PHYSICAL: InputSourceId = InputSourceId(0xB13);
    const KEY_MASK: u64 = (1 << 2) | (1 << 3);
    const KEYCODE: u8 = 38;
    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    state.core_focus.raw = ROOT_WINDOW.0;
    let mut xtest_peer = kbd_map_client_id(&mut state, 41);
    let mut physical_peer = kbd_map_client_id(&mut state, 42);
    let mut master_peer = kbd_map_client_id(&mut state, 43);
    backend.on_host_input(
        &mut state,
        HostInputEvent::DeviceAdded(DeviceInfo {
            source_id: PHYSICAL,
            enabled: true,
            resume_key: None,
            capabilities: InputCapabilities {
                keyboard: true,
                pointer: false,
                touch: false,
            },
            name: "targeted keyboard".to_owned(),
            device_node: "/dev/input/targeted".to_owned(),
            sysname: "targeted".to_owned(),
            vendor_id: 0,
            product_id: 0,
            is_touchpad: false,
            config: LibinputConfigSnapshot::default(),
        }),
    );
    let physical_id = state
        .xi_devices
        .facet(PHYSICAL, XiFacetKind::Keyboard)
        .unwrap();
    state
        .clients
        .get_mut(&41)
        .unwrap()
        .xi2_masks
        .insert((ROOT_WINDOW, 5), KEY_MASK);
    state
        .clients
        .get_mut(&42)
        .unwrap()
        .xi2_masks
        .insert((ROOT_WINDOW, physical_id), KEY_MASK);
    state
        .clients
        .get_mut(&43)
        .unwrap()
        .xi2_masks
        .insert((ROOT_WINDOW, 3), KEY_MASK);
    let key = |device_id, pressed| {
        HostInputEvent::Key(HostKeyEvent {
            origin: InputOrigin::XTest(device_id),
            pressed,
            keycode: KEYCODE,
            time: 0,
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            state: 0,
        })
    };
    backend.on_host_input(&mut state, key(5, true));
    backend.on_host_input(&mut state, key(physical_id, true));
    let xtest_events = xi2_events(&kbd_map_drain(&mut xtest_peer));
    let physical_events = xi2_events(&kbd_map_drain(&mut physical_peer));
    let master_events = xi2_events(&kbd_map_drain(&mut master_peer));
    assert!(
        xtest_events
            .iter()
            .any(|event| event.0 == 2 && event.1 == 5 && event.2 == 5)
    );
    assert!(
        physical_events
            .iter()
            .any(|event| event.0 == 2 && event.1 == physical_id && event.2 == physical_id),
        "XTEST explicitly targeting a physical keyboard preserves the targeted slave form",
    );
    assert_eq!(
        master_events
            .iter()
            .filter(|event| event.0 == 2)
            .map(|event| (event.1, event.2))
            .collect::<Vec<_>>(),
        vec![(3, 5)],
        "the XTEST target's slave state is independent while master still has one down transition",
    );
    backend.on_host_input(&mut state, key(physical_id, false));
    backend.on_host_input(&mut state, key(5, false));
    let physical_release = xi2_events(&kbd_map_drain(&mut physical_peer));
    let xtest_release = xi2_events(&kbd_map_drain(&mut xtest_peer));
    assert!(
        physical_release
            .iter()
            .any(|event| event.0 == 3 && event.1 == physical_id)
    );
    assert!(
        xtest_release
            .iter()
            .any(|event| event.0 == 3 && event.1 == 5)
    );
    let master_tail = xi2_events(&kbd_map_drain(&mut master_peer));
    assert_eq!(
        master_tail
            .iter()
            .filter(|event| event.0 == 3 && event.3 == u32::from(KEYCODE))
            .map(|event| event.2)
            .collect::<Vec<_>>(),
        vec![physical_id],
        "the first valid targeted-device release releases the master once; XTEST 5's later release is suppressed",
    );
    assert!(backend.core.down_keys.is_empty());
    assert!(state.keys_down.iter().all(|byte| *byte == 0));
    assert!(state.key_down_by_device.is_empty());
    assert!(state.unpublished_keyboard_keys_down.is_empty());
    assert_eq!(
        state.xi_devices.facet(PHYSICAL, XiFacetKind::Keyboard),
        Some(physical_id)
    );
    assert_eq!(state.xi_devices.devices().len(), 5);
}

#[test]
fn key_source_selection_delivers_physical_key_under_exact_device_grab() {
    use yserver_core::{
        backend::Backend,
        core_loop::{DeviceInfo, HostInputEvent, InputOrigin, message::LibinputConfigSnapshot},
        host_x11::HostKeyEvent,
        resources::ROOT_WINDOW,
        server::ServerState,
        xinput::{InputCapabilities, InputSourceId, XiFacetKind},
    };
    use yserver_protocol::x11::{ClientId, RequestHeader, SequenceNumber};

    const SOURCE: InputSourceId = InputSourceId(0xB14);
    const CLIENT: u32 = 51;
    const KEYCODE: u8 = 38;
    const KEY_MASK: u64 = (1 << 2) | (1 << 3);
    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    let mut peer = kbd_map_client_id(&mut state, CLIENT);
    backend.on_host_input(
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
            name: "grabbed keyboard".to_owned(),
            device_node: "/dev/input/grabbed".to_owned(),
            sysname: "grabbed".to_owned(),
            vendor_id: 0,
            product_id: 0,
            is_touchpad: false,
            config: LibinputConfigSnapshot::default(),
        }),
    );
    let device_id = state
        .xi_devices
        .facet(SOURCE, XiFacetKind::Keyboard)
        .unwrap();
    let mut body = Vec::with_capacity(24);
    body.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&device_id.to_le_bytes());
    body.extend_from_slice(&[1, 1, 0, 0]);
    body.extend_from_slice(&1u16.to_le_bytes());
    body.extend_from_slice(&(KEY_MASK as u32).to_le_bytes());
    yserver_core::core_loop::process_request::process_request(
        &mut state,
        &mut backend,
        ClientId(CLIENT),
        SequenceNumber(1),
        RequestHeader {
            opcode: 137,
            data: 51,
            length_units: 7,
        },
        &body,
        None,
    )
    .expect("XIGrabDevice on the physical keyboard facet");
    assert!(state.xi2_keyboard_grabs.contains_key(&device_id));
    assert_eq!(
        state.xi_devices.device(device_id).unwrap().attached_master,
        None
    );
    backend.on_host_input(
        &mut state,
        HostInputEvent::Key(HostKeyEvent {
            origin: InputOrigin::Physical(SOURCE),
            pressed: true,
            keycode: KEYCODE,
            time: 0,
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            state: 0,
        }),
    );
    let events = xi2_events(&kbd_map_drain(&mut peer));
    assert!(events.iter().any(|event| {
        event.0 == 2
            && event.1 == device_id
            && event.2 == device_id
            && event.3 == u32::from(KEYCODE)
    }));
    assert!(!events.iter().any(|event| event.1 == 3));
    assert!(state.sync_pending.is_empty());
    assert!(state.xi2_keyboard_grabs.contains_key(&device_id));
    assert_eq!(
        state.xi_devices.device(device_id).unwrap().source_id,
        Some(SOURCE)
    );
    backend.on_host_input(
        &mut state,
        HostInputEvent::Key(HostKeyEvent {
            origin: InputOrigin::Physical(SOURCE),
            pressed: false,
            keycode: KEYCODE,
            time: 0,
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            state: 0,
        }),
    );
    let _ = kbd_map_drain(&mut peer);
    assert!(
        !backend.floating_keyboard_states[&device_id]
            .down_keys
            .contains(&KEYCODE)
    );
    assert!(state.key_down_by_device.is_empty());
    assert!(backend.core.down_keys.is_empty());
    assert!(state.keys_down.iter().all(|byte| *byte == 0));
}

#[test]
fn key_source_selection_xi1_uses_the_physical_keyboard_facet() {
    use yserver_core::{
        backend::Backend,
        core_loop::{DeviceInfo, HostInputEvent, InputOrigin, message::LibinputConfigSnapshot},
        host_x11::HostKeyEvent,
        resources::ROOT_WINDOW,
        server::ServerState,
        xinput::{
            InputCapabilities, InputSourceId, XI_DEVICE_KEY_PRESS_OFFSET,
            XI_DEVICE_KEY_RELEASE_OFFSET, XiFacetKind,
        },
    };

    const SOURCE: InputSourceId = InputSourceId(0xB17);
    const CLIENT: u32 = 71;
    const KEYCODE: u8 = 38;
    const XI_FIRST_EVENT: u8 = 66;
    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    state.core_focus.raw = ROOT_WINDOW.0;
    let mut peer = kbd_map_client_id(&mut state, CLIENT);
    backend.on_host_input(
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
            name: "XI1 keyboard".to_owned(),
            device_node: "/dev/input/xi1-keyboard".to_owned(),
            sysname: "xi1-keyboard".to_owned(),
            vendor_id: 0,
            product_id: 0,
            is_touchpad: false,
            config: LibinputConfigSnapshot::default(),
        }),
    );
    let device_id = state
        .xi_devices
        .facet(SOURCE, XiFacetKind::Keyboard)
        .unwrap();
    let press_class =
        (u32::from(device_id) << 8) | u32::from(XI_FIRST_EVENT + XI_DEVICE_KEY_PRESS_OFFSET);
    let release_class =
        (u32::from(device_id) << 8) | u32::from(XI_FIRST_EVENT + XI_DEVICE_KEY_RELEASE_OFFSET);
    state
        .clients
        .get_mut(&CLIENT)
        .unwrap()
        .xi1_window_event_classes
        .insert(
            ROOT_WINDOW,
            [press_class, release_class].into_iter().collect(),
        );
    let key = |origin, pressed| {
        HostInputEvent::Key(HostKeyEvent {
            origin,
            pressed,
            keycode: KEYCODE,
            time: 0,
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            state: 0,
        })
    };
    backend.on_host_input(&mut state, key(InputOrigin::Physical(SOURCE), true));
    backend.on_host_input(&mut state, key(InputOrigin::Physical(SOURCE), false));
    let events = kbd_map_drain(&mut peer);
    assert_eq!(events.len(), 64);
    assert_eq!(events[0], XI_FIRST_EVENT + XI_DEVICE_KEY_PRESS_OFFSET);
    let wire_device_id = u8::try_from(device_id).expect("XI1 device ids fit in one byte");
    assert_eq!(events[31], wire_device_id);
    assert_eq!(events[32], XI_FIRST_EVENT + XI_DEVICE_KEY_RELEASE_OFFSET);
    assert_eq!(events[63], wire_device_id);
    assert_eq!(events[1], KEYCODE);
    assert_eq!(events[33], KEYCODE);

    backend.on_host_input(&mut state, key(InputOrigin::NestedHost, true));
    backend.on_host_input(&mut state, key(InputOrigin::NestedHost, false));
    assert!(
        kbd_map_drain(&mut peer).is_empty(),
        "nested input has no XI1 slave form"
    );
    assert!(backend.core.down_keys.is_empty());
    assert!(state.keys_down.iter().all(|byte| *byte == 0));
    assert!(state.key_down_by_device.is_empty());
    assert!(state.unpublished_keyboard_keys_down.is_empty());
    assert!(state.sync_pending.is_empty());
    assert_eq!(
        state.xi_devices.facet(SOURCE, XiFacetKind::Keyboard),
        Some(device_id)
    );
    assert_eq!(state.xi_devices.devices().len(), 5);
}

#[test]
fn key_source_selection_drains_one_keyboard_and_keeps_other_holds() {
    use yserver_core::{
        backend::Backend,
        core_loop::{DeviceInfo, HostInputEvent, InputOrigin, message::LibinputConfigSnapshot},
        host_x11::HostKeyEvent,
        resources::ROOT_WINDOW,
        server::ServerState,
        xinput::{InputCapabilities, InputSourceId, XiFacetKind},
    };

    const A: InputSourceId = InputSourceId(0xB15);
    const B: InputSourceId = InputSourceId(0xB16);
    const A_KEY: u8 = 38;
    const SHIFT_L: u8 = 50;
    const KEY_MASK: u64 = (1 << 2) | (1 << 3);
    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    state.core_focus.raw = ROOT_WINDOW.0;
    let mut a_peer = kbd_map_client_id(&mut state, 61);
    let mut b_peer = kbd_map_client_id(&mut state, 62);
    let mut master_peer = kbd_map_client_id(&mut state, 63);
    for (source, name) in [(A, "drain A"), (B, "drain B")] {
        backend.on_host_input(
            &mut state,
            HostInputEvent::DeviceAdded(DeviceInfo {
                source_id: source,
                enabled: true,
                resume_key: None,
                capabilities: InputCapabilities {
                    keyboard: true,
                    pointer: false,
                    touch: false,
                },
                name: name.to_owned(),
                device_node: format!("/dev/input/{name}"),
                sysname: name.to_owned(),
                vendor_id: 0,
                product_id: 0,
                is_touchpad: false,
                config: LibinputConfigSnapshot::default(),
            }),
        );
    }
    let a_id = state.xi_devices.facet(A, XiFacetKind::Keyboard).unwrap();
    let b_id = state.xi_devices.facet(B, XiFacetKind::Keyboard).unwrap();
    for (client, device) in [(61, a_id), (62, b_id)] {
        state
            .clients
            .get_mut(&client)
            .unwrap()
            .xi2_masks
            .insert((ROOT_WINDOW, device), KEY_MASK);
    }
    state
        .clients
        .get_mut(&63)
        .unwrap()
        .xi2_masks
        .insert((ROOT_WINDOW, 3), KEY_MASK);
    let key = |origin, keycode, pressed| {
        HostInputEvent::Key(HostKeyEvent {
            origin,
            pressed,
            keycode,
            time: 0,
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            state: 0,
        })
    };
    backend.on_host_input(&mut state, key(InputOrigin::Physical(A), A_KEY, true));
    backend.on_host_input(&mut state, key(InputOrigin::Physical(A), A_KEY, false));
    backend.on_host_input(&mut state, key(InputOrigin::XTest(a_id), SHIFT_L, true));
    backend.on_host_input(&mut state, key(InputOrigin::Physical(B), A_KEY, true));
    let a_before_drain = xi2_events(&kbd_map_drain(&mut a_peer));
    let b_before_drain = xi2_events(&kbd_map_drain(&mut b_peer));
    assert_eq!(
        a_before_drain
            .iter()
            .filter(|event| event.0 == 3 && event.3 == u32::from(A_KEY))
            .count(),
        1,
        "the earlier A release has already cleared that key",
    );
    assert!(
        b_before_drain
            .iter()
            .any(|event| event.0 == 2 && event.3 == u32::from(A_KEY))
    );
    let master_before_drain = xi2_events(&kbd_map_drain(&mut master_peer));

    backend.release_keyboard_source_keys(&mut state, A);

    let a_after_drain = xi2_events(&kbd_map_drain(&mut a_peer));
    let b_after_drain = xi2_events(&kbd_map_drain(&mut b_peer));
    let master_after_drain = xi2_events(&kbd_map_drain(&mut master_peer));
    assert_eq!(
        a_after_drain
            .iter()
            .filter(|event| event.0 == 3 && event.3 == u32::from(SHIFT_L))
            .count(),
        1,
        "the targeted XTEST Shift hold on A drains once",
    );
    assert!(
        b_after_drain.is_empty(),
        "unrelated keyboard B is untouched"
    );
    assert_eq!(
        master_after_drain
            .iter()
            .filter(|event| event.0 == 3 && event.3 == u32::from(SHIFT_L))
            .map(|event| (event.1, event.2))
            .collect::<Vec<_>>(),
        vec![(3, a_id)],
        "only A's held Shift transition releases the master",
    );
    assert_eq!(
        master_before_drain
            .iter()
            .chain(master_after_drain.iter())
            .filter(|event| event.0 == 3 && event.3 == u32::from(A_KEY))
            .count(),
        1,
        "A's earlier key release is not generated again during cleanup",
    );
    assert_eq!(backend.serialize_modifiers() & 1, 0);
    assert!(backend.core.down_keys.contains(&A_KEY));
    assert_eq!(
        state.keys_down[usize::from(A_KEY / 8)] & (1 << (A_KEY % 8)),
        1 << (A_KEY % 8)
    );
    assert!(state.sync_pending.is_empty());
    assert!(state.xi_devices.source(A).is_some());
    assert_eq!(state.xi_devices.facet(A, XiFacetKind::Keyboard), Some(a_id));
    assert_eq!(state.xi_devices.facet(B, XiFacetKind::Keyboard), Some(b_id));
    assert!(!state.key_down_by_device.contains_key(&a_id));
    assert_eq!(
        state.key_down_by_device.get(&b_id),
        Some(&std::collections::HashMap::from([(
            A_KEY,
            InputOrigin::Physical(B),
        )])),
        "B's unrelated key remains held after A cleanup",
    );
    assert!(state.unpublished_keyboard_keys_down.is_empty());
    assert_eq!(state.xi_devices.devices().len(), 6);
}

#[test]
fn xi_source_removal_releases_source_holds_before_unregister_and_drops_stale_reuse_input() {
    use yserver_core::{
        backend::Backend,
        core_loop::{DeviceInfo, HostInputEvent, InputOrigin, message::LibinputConfigSnapshot},
        host_x11::HostKeyEvent,
        resources::ROOT_WINDOW,
        server::ServerState,
        xinput::{InputCapabilities, InputSourceId, XiFacetKind},
    };

    const REMOVED: InputSourceId = InputSourceId(0xD140);
    const HYPERX: InputSourceId = InputSourceId(0xD141);
    const REPLACEMENT: InputSourceId = InputSourceId(0xD142);
    const CLIENT: u32 = 0xD14;
    const CTRL_L: u8 = 37;
    const ALT_L: u8 = 64;
    const SHIFT_L: u8 = 50;
    const SUPER_L: u8 = 133;
    const HYPERX_KEY: u8 = 30;
    const KEY_MASK: u64 = (1 << 2) | (1 << 3);
    const BUTTON_MASK: u64 = (1 << 4) | (1 << 5);

    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    backend
        .core
        .xid_map
        .insert(backend.core.window_id, ROOT_WINDOW);
    state.core_focus.raw = ROOT_WINDOW.0;
    let mut peer = kbd_map_client_id(&mut state, CLIENT);
    let make_info = |source_id: InputSourceId, name: &str| DeviceInfo {
        source_id,
        enabled: true,
        resume_key: None,
        capabilities: InputCapabilities {
            keyboard: true,
            pointer: true,
            touch: false,
        },
        name: name.to_owned(),
        device_node: format!("/dev/input/{}", source_id.0),
        sysname: format!("event{}", source_id.0),
        vendor_id: 0,
        product_id: source_id.0 as u32,
        is_touchpad: false,
        config: LibinputConfigSnapshot::default(),
    };
    for (source, name) in [(REMOVED, "Razer"), (HYPERX, "HyperX")] {
        backend.on_host_input(
            &mut state,
            HostInputEvent::DeviceAdded(make_info(source, name)),
        );
    }
    let removed_keyboard = state
        .xi_devices
        .facet(REMOVED, XiFacetKind::Keyboard)
        .unwrap();
    let removed_pointer = state
        .xi_devices
        .facet(REMOVED, XiFacetKind::PointerTouch)
        .unwrap();
    let hyperx_keyboard = state
        .xi_devices
        .facet(HYPERX, XiFacetKind::Keyboard)
        .unwrap();
    let hyperx_pointer = state
        .xi_devices
        .facet(HYPERX, XiFacetKind::PointerTouch)
        .unwrap();
    let removed_keyboard_properties = state
        .xi_devices
        .device(removed_keyboard)
        .unwrap()
        .properties
        .clone();
    let removed_pointer_properties = state
        .xi_devices
        .device(removed_pointer)
        .unwrap()
        .properties
        .clone();
    let hyperx_pointer_properties = state
        .xi_devices
        .device(hyperx_pointer)
        .unwrap()
        .properties
        .clone();
    for (device_id, mask) in [
        (removed_keyboard, KEY_MASK),
        (hyperx_keyboard, KEY_MASK),
        (removed_pointer, BUTTON_MASK),
        (hyperx_pointer, BUTTON_MASK),
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
            .insert((ROOT_WINDOW, device_id), mask);
    }
    state
        .clients
        .get_mut(&CLIENT)
        .unwrap()
        .xi2_masks
        .insert((ROOT_WINDOW, 0), KEY_MASK | BUTTON_MASK);

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
                .expect("flush buffered test events");
            assert_ne!(
                outcome,
                yserver_core::core_loop::client_io::WriteOutcome::Disconnect,
                "the event-capture peer remains connected",
            );
        }
        bytes.extend(kbd_map_drain(peer));
        bytes
    };

    // The source is a mixed keyboard/pointer facet. Explicit XTEST holds
    // aimed at its physical XI facets belong to those facets and drain
    // with them; virtual XTEST 4/5 holds remain independent.
    backend.on_host_input(
        &mut state,
        key(InputOrigin::Physical(REMOVED), CTRL_L, true),
    );
    backend.on_host_input(&mut state, key(InputOrigin::Physical(REMOVED), ALT_L, true));
    backend.on_host_input(
        &mut state,
        key(InputOrigin::XTest(removed_keyboard), SUPER_L, true),
    );
    backend.on_host_input(
        &mut state,
        key(InputOrigin::Physical(HYPERX), HYPERX_KEY, true),
    );
    backend.on_host_input(&mut state, key(InputOrigin::XTest(5), SHIFT_L, true));
    backend.on_host_input(
        &mut state,
        button(InputOrigin::Physical(REMOVED), 0x110, true),
    );
    backend.on_host_input(
        &mut state,
        button(InputOrigin::XTest(removed_pointer), 0x112, true),
    );
    backend.on_host_input(
        &mut state,
        button(InputOrigin::Physical(HYPERX), 0x110, true),
    );
    backend.on_host_input(&mut state, button(InputOrigin::XTest(4), 0x112, true));
    let press_bytes = drain_peer(&mut state, &mut peer);
    let press_events = xi2_events(&press_bytes);
    assert!(
        press_events.iter().any(|event| {
            event.0 == 4 && event.1 == removed_pointer && event.2 == removed_pointer
        }),
        "production pointer press reaches the selected source facet: events={press_events:?}; bytes={}; buffered={}; held={}",
        press_bytes.len(),
        state.clients[&CLIENT].outbound.len(),
        state
            .xi_devices
            .device(removed_pointer)
            .unwrap()
            .buttons_down,
    );
    assert_eq!(
        state
            .xi_devices
            .device(removed_pointer)
            .unwrap()
            .buttons_down,
        3,
        "the removed facet holds its physical and explicit-XTEST buttons",
    );

    assert_eq!(
        state
            .xi_devices
            .device(removed_keyboard)
            .unwrap()
            .properties,
        removed_keyboard_properties,
        "keyboard facet properties are intact immediately before removal",
    );
    assert_eq!(
        state.xi_devices.device(removed_pointer).unwrap().properties,
        removed_pointer_properties,
        "pointer facet properties are intact immediately before removal",
    );
    backend.on_host_input(
        &mut state,
        HostInputEvent::DeviceRemoved { source_id: REMOVED },
    );
    assert!(
        state.pending_xi_device_removals.is_empty(),
        "the production removal dispatch consumes descriptor snapshots for XI2 Removed",
    );
    let removal_bytes = drain_peer(&mut state, &mut peer);
    let removal_events = xi2_events(&removal_bytes);
    let releases: Vec<_> = removal_events
        .iter()
        .enumerate()
        .filter(|(_, event)| matches!(event.0, 3 | 5))
        .collect();

    // Xorg dix/devices.c::ReleaseButtonsAndKeys runs its
    // `/* Release all buttons */` loop before `/* Release all keys */`.
    let last_button_release = releases
        .iter()
        .filter(|(_, event)| event.0 == 5)
        .map(|(index, _)| *index)
        .max()
        .unwrap_or_else(|| {
            panic!(
                "removed source emits button releases; decoded={removal_events:?}; bytes={}; buffered={}",
                removal_bytes.len(),
                state.clients[&CLIENT].outbound.len(),
            )
        });
    let first_key_release = releases
        .iter()
        .filter(|(_, event)| event.0 == 3)
        .map(|(index, _)| *index)
        .min()
        .expect("removed source emits key releases");
    assert!(last_button_release < first_key_release);

    for detail in [1, 2] {
        assert_eq!(
            removal_events
                .iter()
                .filter(|event| {
                    event.0 == 5
                        && event.1 == removed_pointer
                        && event.2 == removed_pointer
                        && event.3 == detail
                })
                .count(),
            1,
            "each removed-facet ButtonRelease must be delivered once",
        );
    }
    assert_eq!(
        removal_events
            .iter()
            .filter(|event| event.0 == 5 && event.1 == 2 && event.3 == 1)
            .count(),
        0,
        "HyperX still holds button 1, so its master release is suppressed",
    );
    assert_eq!(
        removal_events
            .iter()
            .filter(|event| event.0 == 5 && event.1 == 2 && event.3 == 2)
            .count(),
        0,
        "virtual XTEST still holds button 2, so its master release is suppressed",
    );
    assert!(
        removal_events
            .iter()
            .all(|event| { !(matches!(event.0, 3 | 5) && matches!(event.1, 4 | 5)) }),
        "virtual XTEST facets do not receive removal releases"
    );
    for (keycode, expected_slave_events) in [(CTRL_L, 1), (ALT_L, 1), (SUPER_L, 1)] {
        let slave_release = removal_events
            .iter()
            .position(|event| {
                event.0 == 3
                    && event.1 == removed_keyboard
                    && event.2 == removed_keyboard
                    && event.3 == u32::from(keycode)
            })
            .expect("removed keyboard facet gets its release");
        assert_eq!(
            removal_events
                .iter()
                .filter(|event| {
                    event.0 == 3
                        && event.1 == removed_keyboard
                        && event.2 == removed_keyboard
                        && event.3 == u32::from(keycode)
                })
                .count(),
            expected_slave_events,
        );
        let master_release = removal_events
            .iter()
            .position(|event| {
                event.0 == 3
                    && event.1 == 3
                    && event.2 == removed_keyboard
                    && event.3 == u32::from(keycode)
            })
            .expect("accepted slave release reaches the keyboard master");
        assert!(
            slave_release < master_release,
            "slave form precedes master form"
        );
        assert_eq!(
            removal_events
                .iter()
                .filter(|event| {
                    event.0 == 3
                        && event.1 == 3
                        && event.2 == removed_keyboard
                        && event.3 == u32::from(keycode)
                })
                .count(),
            1,
            "each removed-source master KeyRelease is emitted once",
        );
    }

    assert!(state.xi_devices.source(REMOVED).is_none());
    assert!(
        state
            .xi_devices
            .devices()
            .iter()
            .all(|device| device.source_id != Some(REMOVED))
    );
    assert!(state.xi_devices.device(removed_keyboard).is_none());
    assert!(state.xi_devices.device(removed_pointer).is_none());
    assert!(!state.key_down_by_device.contains_key(&removed_keyboard));
    assert!(
        !state
            .key_repeats
            .contains_key(&InputOrigin::Physical(REMOVED))
    );
    assert!(!state.unpublished_keyboard_keys_down.contains_key(&REMOVED));
    assert!(
        !state
            .unpublished_pointer_buttons_down
            .contains_key(&REMOVED)
    );
    assert!(state.sync_pending.is_empty());
    assert!(backend.core.pending_pointer_events.is_empty());
    assert!(!state.xi1_frozen.contains_key(&removed_keyboard));
    assert!(!state.xi1_frozen.contains_key(&removed_pointer));
    assert!(!state.xi2_keyboard_grabs.contains_key(&removed_keyboard));
    assert!(!state.xi2_pointer_grabs.contains_key(&removed_pointer));
    assert!(!state.xi2_detached_masters.contains_key(&removed_keyboard));
    assert!(!state.xi2_detached_masters.contains_key(&removed_pointer));
    backend.on_host_input(
        &mut state,
        HostInputEvent::DeviceRemoved { source_id: REMOVED },
    );
    assert!(drain_peer(&mut state, &mut peer).is_empty());
    assert!(state.pending_xi_device_removals.is_empty());
    assert!(state.take_xi_removed_device_descriptors().is_empty());
    assert!(backend.core.down_keys.contains(&HYPERX_KEY));
    assert!(backend.core.down_keys.contains(&SHIFT_L));
    assert!(!backend.core.down_keys.contains(&CTRL_L));
    assert!(!backend.core.down_keys.contains(&ALT_L));
    assert!(!backend.core.down_keys.contains(&SUPER_L));
    assert_eq!(
        state.key_down_by_device.get(&hyperx_keyboard),
        Some(&std::collections::HashMap::from([(
            HYPERX_KEY,
            InputOrigin::Physical(HYPERX),
        )])),
    );
    assert_eq!(
        state.key_down_by_device.get(&5),
        Some(&std::collections::HashMap::from([(
            SHIFT_L,
            InputOrigin::XTest(5),
        )])),
        "virtual XTEST keyboard 5 keeps its independent Shift hold",
    );
    assert_eq!(
        state
            .xi_devices
            .device(hyperx_pointer)
            .unwrap()
            .buttons_down,
        1,
        "HyperX keeps its button held",
    );
    assert_eq!(
        state.xi_devices.device(hyperx_pointer).unwrap().properties,
        hyperx_pointer_properties,
        "HyperX's independent property state remains intact",
    );
    assert_eq!(state.xi_devices.device(4).unwrap().buttons_down, 2);
    assert_eq!(
        state.xi_devices.device(5).unwrap().source_id,
        None,
        "virtual XTEST keyboard identity remains present",
    );
    assert_eq!(state.buttons_down, 3);
    assert_eq!(backend.core.button_mask, 0x0300);
    let mut expected_keys = [0u8; 32];
    for keycode in [HYPERX_KEY, SHIFT_L] {
        expected_keys[usize::from(keycode / 8)] |= 1 << (keycode % 8);
    }
    assert_eq!(state.keys_down, expected_keys);
    assert_eq!(
        state
            .xi_devices
            .device(hyperx_pointer)
            .unwrap()
            .scroll_axis_values,
        [0, 0],
        "unrelated pointer valuators remain unchanged",
    );

    // A newly added source can reuse the XI IDs, but events remain bound
    // to their old runtime SourceId and cannot mutate/fan out into it.
    backend.on_host_input(
        &mut state,
        HostInputEvent::DeviceAdded(make_info(REPLACEMENT, "replacement")),
    );
    assert_eq!(
        state.xi_devices.facet(REPLACEMENT, XiFacetKind::Keyboard),
        Some(removed_keyboard),
    );
    assert_eq!(
        state
            .xi_devices
            .facet(REPLACEMENT, XiFacetKind::PointerTouch),
        Some(removed_pointer),
    );
    let before = (
        backend.core.cursor_x,
        backend.core.cursor_y,
        backend.core.button_mask,
        backend.serialize_modifiers(),
        state.pointer_root,
        state.buttons_down,
        state.keys_down,
        state
            .xi_devices
            .device(removed_pointer)
            .unwrap()
            .buttons_down,
    );
    backend.on_host_input(
        &mut state,
        key(InputOrigin::Physical(REMOVED), CTRL_L, true),
    );
    backend.on_host_input(
        &mut state,
        HostInputEvent::PointerMotion {
            origin: InputOrigin::Physical(REMOVED),
            x: 500,
            y: 400,
            time: 3,
            relative: false,
            dx: 0,
            dy: 0,
            motion_delta: None,
        },
    );
    backend.on_host_input(
        &mut state,
        button(InputOrigin::Physical(REMOVED), 0x111, true),
    );
    assert_eq!(
        (
            backend.core.cursor_x,
            backend.core.cursor_y,
            backend.core.button_mask,
            backend.serialize_modifiers(),
            state.pointer_root,
            state.buttons_down,
            state.keys_down,
            state
                .xi_devices
                .device(removed_pointer)
                .unwrap()
                .buttons_down,
        ),
        before,
        "old-source input cannot change KMS, master, or reused-facet state",
    );
    assert!(drain_peer(&mut state, &mut peer).is_empty());
    assert!(state.xi_devices.source(HYPERX).unwrap().enabled);
    assert!(state.xi_devices.source(REPLACEMENT).unwrap().enabled);
}

#[test]
fn key_source_selection_guards_unpublished_physical_keyboard_holds() {
    use yserver_core::{
        backend::Backend,
        core_loop::{DeviceInfo, HostInputEvent, InputOrigin, message::LibinputConfigSnapshot},
        host_x11::HostKeyEvent,
        resources::ROOT_WINDOW,
        server::ServerState,
        xinput::{InputCapabilities, InputSourceId, XiFacetKind},
    };
    const A: InputSourceId = InputSourceId(0xB18);
    const B: InputSourceId = InputSourceId(0xB19);
    const SHIFT_L: u8 = 50;
    const KEY_MASK: u64 = (1 << 2) | (1 << 3);

    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    state.core_focus.raw = ROOT_WINDOW.0;
    let mut master_peer = kbd_map_client_id(&mut state, 81);
    let mut core_peer = kbd_map_client_id(&mut state, 82);
    state
        .clients
        .get_mut(&81)
        .unwrap()
        .xi2_masks
        .insert((ROOT_WINDOW, 3), KEY_MASK);
    state
        .clients
        .get_mut(&82)
        .unwrap()
        .event_masks
        .insert(ROOT_WINDOW, 0x0000_0001 | 0x0000_0002);

    let info = |source_id: InputSourceId| DeviceInfo {
        source_id,
        enabled: true,
        resume_key: None,
        capabilities: InputCapabilities {
            keyboard: true,
            pointer: false,
            touch: false,
        },
        name: format!("unpublished keyboard {}", source_id.0),
        device_node: format!("/dev/input/unpublished{}", source_id.0),
        sysname: format!("unpublished{}", source_id.0),
        vendor_id: 0,
        product_id: 0,
        is_touchpad: false,
        config: LibinputConfigSnapshot::default(),
    };
    // Fill every physical XI slot through the same KMS DeviceAdded entry
    // used by production input discovery.
    for id in 1..=122 {
        backend.on_host_input(
            &mut state,
            HostInputEvent::DeviceAdded(info(InputSourceId(id))),
        );
    }
    for source in [A, B] {
        backend.on_host_input(&mut state, HostInputEvent::DeviceAdded(info(source)));
    }
    assert!(state.xi_devices.facet(A, XiFacetKind::Keyboard).is_none());
    assert!(state.xi_devices.facet(B, XiFacetKind::Keyboard).is_none());

    let key = |source, pressed| {
        HostInputEvent::Key(HostKeyEvent {
            origin: InputOrigin::Physical(source),
            pressed,
            keycode: SHIFT_L,
            time: 0,
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            state: 0,
        })
    };
    backend.on_host_input(&mut state, key(A, true));
    backend.on_host_input(&mut state, key(B, true));
    assert_eq!(
        state.unpublished_keyboard_keys_down.get(&A),
        Some(&std::collections::HashMap::from([(
            SHIFT_L,
            InputOrigin::Physical(A),
        )])),
    );
    assert_eq!(
        state.unpublished_keyboard_keys_down.get(&B),
        Some(&std::collections::HashMap::from([(
            SHIFT_L,
            InputOrigin::Physical(B),
        )])),
    );
    assert_ne!(backend.serialize_modifiers() & 1, 0);
    let master_press = xi2_events(&kbd_map_drain(&mut master_peer));
    assert_eq!(
        master_press
            .iter()
            .filter(|event| event.0 == 2)
            .map(|event| (event.1, event.2, event.3))
            .collect::<Vec<_>>(),
        vec![(3, 3, u32::from(SHIFT_L))],
        "an unpublished physical source still obeys the master's independent guard",
    );

    backend.release_keyboard_source_keys(&mut state, A);
    assert_eq!(backend.serialize_modifiers() & 1, 0);
    assert!(backend.core.down_keys.is_empty());
    assert!(!state.unpublished_keyboard_keys_down.contains_key(&A));
    assert_eq!(
        state.unpublished_keyboard_keys_down.get(&B),
        Some(&std::collections::HashMap::from([(
            SHIFT_L,
            InputOrigin::Physical(B),
        )])),
        "draining A leaves B's unpublished physical key hold intact",
    );
    let master_release = xi2_events(&kbd_map_drain(&mut master_peer));
    assert_eq!(
        master_release
            .iter()
            .filter(|event| event.0 == 3)
            .map(|event| (event.1, event.2, event.3))
            .collect::<Vec<_>>(),
        vec![(3, 3, u32::from(SHIFT_L))],
    );

    backend.on_host_input(&mut state, key(B, false));
    assert!(xi2_events(&kbd_map_drain(&mut master_peer)).is_empty());
    let core_bytes = kbd_map_drain(&mut core_peer);
    assert_eq!(
        core_bytes
            .chunks_exact(32)
            .filter(|event| matches!(event[0] & 0x7f, 2 | 3) && event[1] == SHIFT_L)
            .map(|event| event[0] & 0x7f)
            .collect::<Vec<_>>(),
        vec![2, 3],
        "core sees only the master's accepted press and first release",
    );
    assert!(backend.core.down_keys.is_empty());
    assert!(state.keys_down.iter().all(|byte| *byte == 0));
    assert!(state.key_down_by_device.is_empty());
    assert!(state.unpublished_keyboard_keys_down.is_empty());
    assert!(state.sync_pending.is_empty());
    assert!(state.xi_devices.source(A).is_some());
    assert!(state.xi_devices.source(B).is_some());
    assert_eq!(state.xi_devices.devices().len(), 126);
}

/// The duplicate guard (#168) drops a press of a key already down and
/// a release of a key that is not down before any delivery — but the
/// raw event is generated ahead of it, as on Xorg (Xvfb capture, XTEST):
/// `p38 p38` → two RawKeyPress; `p50 p50` (Shift_L, a modifier) → one;
/// a lone `r38` → one RawKeyRelease. No device event for the dropped ones.
#[test]
fn duplicate_guard_keeps_xorg_raw_key_events() {
    use yserver_core::{
        core_loop::HostInputEvent, host_x11::HostKeyEvent, resources::ROOT_WINDOW,
        server::ServerState,
    };
    const A: u8 = 38;
    const SHIFT_L: u8 = 50;
    let mut b = KmsBackend::for_tests();
    assert_ne!(
        b.core.xkb_desc.modmap[usize::from(SHIFT_L)],
        0,
        "precondition: Shift_L is a modifier"
    );
    let mut state = ServerState::new();
    let mut peer = kbd_map_client_id(&mut state, 9);
    let client = state.clients.get_mut(&9).unwrap();
    client
        .xi2_masks
        .insert((ROOT_WINDOW, 1), (1 << 13) | (1 << 14));
    client
        .xi2_masks
        .insert((ROOT_WINDOW, 3), (1 << 2) | (1 << 3));
    let key = |keycode, pressed| {
        HostInputEvent::Key(HostKeyEvent {
            origin: yserver_core::core_loop::InputOrigin::NestedHost,
            keycode,
            pressed,
            state: 0,
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            time: 0,
        })
    };

    for (keycode, pressed) in [
        (A, true),
        (A, true),
        (A, false),
        (SHIFT_L, true),
        (SHIFT_L, true),
        (SHIFT_L, false),
        (A, false),
    ] {
        b.on_host_input(&mut state, key(keycode, pressed));
    }

    let kinds: Vec<_> = xi2_events(&kbd_map_drain(&mut peer))
        .iter()
        .map(|e| (e.0, e.3))
        .collect();
    assert_eq!(
        kinds,
        vec![
            (13, 38),
            (2, 38),
            (13, 38), // duplicate press: raw only
            (14, 38),
            (3, 38),
            (13, 50),
            (2, 50),
            // duplicate modifier press: nothing
            (14, 50),
            (3, 50),
            (14, 38), // release of a key that is not down: raw only
        ]
    );
}

/// Software auto-repeat is not device input: Xorg's XKB repeat builds
/// the device event directly (AccessXKeyboardEvent) and never passes
/// GetKeyboardEvents, so a repeat pair produces no raw key events.
#[test]
fn key_repeat_emits_no_raw_key_events() {
    use yserver_core::{
        core_loop::HostInputEvent, host_x11::HostKeyEvent, resources::ROOT_WINDOW,
        server::ServerState,
    };
    let mut b = KmsBackend::for_tests();
    let mut state = ServerState::new();
    let mut peer = kbd_map_client_id(&mut state, 9);
    let client = state.clients.get_mut(&9).unwrap();
    client
        .xi2_masks
        .insert((ROOT_WINDOW, 1), (1 << 13) | (1 << 14));
    client
        .xi2_masks
        .insert((ROOT_WINDOW, 3), (1 << 2) | (1 << 3));
    let ev = |pressed| HostKeyEvent {
        origin: yserver_core::core_loop::InputOrigin::NestedHost,
        keycode: 38,
        pressed,
        state: 0,
        root_x: 0,
        root_y: 0,
        event_x: 0,
        event_y: 0,
        time: 0,
    };

    b.on_host_input(&mut state, HostInputEvent::Key(ev(true)));
    b.on_host_input(&mut state, HostInputEvent::KeyRepeat(ev(false)));
    b.on_host_input(&mut state, HostInputEvent::KeyRepeat(ev(true)));
    b.on_host_input(&mut state, HostInputEvent::Key(ev(false)));

    let kinds: Vec<_> = xi2_events(&kbd_map_drain(&mut peer))
        .iter()
        .map(|e| e.0)
        .collect();
    assert_eq!(kinds, vec![13, 2, 3, 2, 14, 3]);
}

/// RECORD records an autorepeat as Xorg generates it: a press flagged
/// in the sequence field and no release (`record-probe repeat l` on
/// Xvfb: `02260000`, then presses with seqfield 1, then `03260000`).
#[test]
fn record_sees_autorepeat_as_flagged_presses() {
    use yserver_core::{core_loop::HostInputEvent, host_x11::HostKeyEvent, server::ServerState};
    let mut b = KmsBackend::for_tests();
    let mut state = ServerState::new();
    let mut peer = kbd_map_client_id(&mut state, 5);
    // CreateContext(ctx 1, FutureClients, device events 2..3), then
    // EnableContext on the same connection.
    let mut create = Vec::new();
    for word in [1u32, 0, 1, 1, 2] {
        create.extend_from_slice(&word.to_le_bytes());
    }
    create.extend_from_slice(&[0; 18]);
    create.extend_from_slice(&[2, 3, 0, 0, 0, 0]);
    kbd_map_request(&mut state, &mut b, 154, 1, &create);
    kbd_map_request(&mut state, &mut b, 154, 5, &1u32.to_le_bytes());
    let ev = |pressed| HostKeyEvent {
        origin: yserver_core::core_loop::InputOrigin::NestedHost,
        keycode: 38,
        pressed,
        state: 0,
        root_x: 0,
        root_y: 0,
        event_x: 0,
        event_y: 0,
        time: 0,
    };
    b.on_host_input(&mut state, HostInputEvent::Key(ev(true)));
    b.on_host_input(&mut state, HostInputEvent::KeyRepeat(ev(false)));
    b.on_host_input(&mut state, HostInputEvent::KeyRepeat(ev(true)));
    b.on_host_input(&mut state, HostInputEvent::Key(ev(false)));

    let bytes = kbd_map_drain(&mut peer);
    let mut recorded = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let len = 32 + 4 * u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
        if bytes[at] == 1 && bytes[at + 1] == 0 {
            let event = &bytes[at + 32..at + len];
            recorded.push((event[0], event[1], u16::from_le_bytes([event[2], event[3]])));
        }
        at += len;
    }
    assert_eq!(recorded, [(2, 38, 0), (2, 38, 1), (3, 38, 0)]);
}

/// Test A: a compiled `grp:alt_shift_toggle` option makes the
/// xkb_state effective layout advance 0 -> 1 when Alt_L then
/// Shift_L are pressed. This locks in the xkbcommon behaviour the
/// Driver-2 detection in `cook_host_key` relies on. (Confirmed
/// locally before writing the feature.)
#[test]
fn grp_alt_shift_toggle_advances_xkb_layout() {
    let mut b = KmsBackend::for_tests();
    let changed = b.core.recompile_keymap(&crate::kms::core::XkbRmlvo {
        rules: "evdev".into(),
        model: "pc105".into(),
        layout: "us,be".into(),
        variant: String::new(),
        options: Some("grp:alt_shift_toggle".into()),
    });
    assert!(
        changed.is_some(),
        "us,be + grp:alt_shift_toggle must compile"
    );

    // Fresh keymap starts on group 0.
    assert_eq!(
        b.core
            .xkb_state
            .0
            .serialize_layout(xkbcommon::xkb::STATE_LAYOUT_EFFECTIVE),
        0,
        "fresh keymap is on group 0"
    );

    // Alt_L = evdev KEY_LEFTALT 56 + 8 = keycode 64.
    // Shift_L = evdev KEY_LEFTSHIFT 42 + 8 = keycode 50.
    b.core.xkb_state.0.update_key(
        xkbcommon::xkb::Keycode::new(64),
        xkbcommon::xkb::KeyDirection::Down,
    );
    b.core.xkb_state.0.update_key(
        xkbcommon::xkb::Keycode::new(50),
        xkbcommon::xkb::KeyDirection::Down,
    );

    assert_eq!(
        b.core
            .xkb_state
            .0
            .serialize_layout(xkbcommon::xkb::STATE_LAYOUT_EFFECTIVE),
        1,
        "Alt+Shift must advance the effective layout to group 1 (be)"
    );
}

/// Test B (make-or-break): feeding the Alt+Shift sequence through
/// `cook_host_key` (Driver 2) must (a) sync the authoritative
/// `core.locked_group` to 1, and (b) stamp group 1 into the cooked
/// event's `state` group bits (13-14) on the very key that
/// triggered the switch, so clients see the new group immediately.
#[test]
fn cook_host_key_grp_shortcut_syncs_locked_group_and_stamps_group_bits() {
    use yserver_core::host_x11::HostKeyEvent;
    let mut b = KmsBackend::for_tests();
    let changed = b.core.recompile_keymap(&crate::kms::core::XkbRmlvo {
        rules: "evdev".into(),
        model: "pc105".into(),
        layout: "us,be".into(),
        variant: String::new(),
        options: Some("grp:alt_shift_toggle".into()),
    });
    assert!(
        changed.is_some(),
        "us,be + grp:alt_shift_toggle must compile"
    );
    assert_eq!(b.core.locked_group, 0, "fresh state is on group 0");

    let key = |keycode, pressed| HostKeyEvent {
        origin: yserver_core::core_loop::InputOrigin::NestedHost,
        keycode,
        pressed,
        state: 0,
        root_x: 0,
        root_y: 0,
        event_x: 0,
        event_y: 0,
        time: 0,
    };

    // Alt_L down (keycode 64): no group change yet.
    let _ = b.cook_host_key(key(64, true));
    assert_eq!(
        b.core.locked_group, 0,
        "Alt alone must not advance the group"
    );

    // Shift_L down (keycode 50): the toggle fires — group 0 -> 1.
    let cooked = b.cook_host_key(key(50, true));
    assert_eq!(
        b.core.locked_group, 1,
        "Alt+Shift must sync the authoritative locked_group to 1"
    );
    // Group bits 13-14 (XkbGroupForCoreState): group 1 == 0x2000.
    assert_eq!(
        cooked.state & 0x6000,
        0x2000,
        "the trigger key must carry the NEW group in its state bits"
    );
}

/// Lock-LED mask tracks the XKB lock state: a Caps Lock toggle
/// (press + release through `cook_host_key`) raises the CAPSLOCK
/// LED bit; a second toggle clears it. This mask is what
/// `sync_keyboard_leds` pushes to the keyboards via libinput —
/// the caps-LED-never-lights bug (typed the lock-screen password
/// wrong repeatedly because the LED gave no feedback).
#[test]
fn caps_lock_toggle_drives_led_bits() {
    use yserver_core::host_x11::HostKeyEvent;
    let mut b = KmsBackend::for_tests();
    assert_eq!(b.current_led_bits(), 0, "fresh state: all lock LEDs off");
    // 66 == X keycode for Caps Lock (evdev KEY_CAPSLOCK 58 + 8).
    let key = |pressed| HostKeyEvent {
        origin: yserver_core::core_loop::InputOrigin::NestedHost,
        keycode: 66,
        pressed,
        state: 0,
        root_x: 0,
        root_y: 0,
        event_x: 0,
        event_y: 0,
        time: 0,
    };
    let _ = b.cook_host_key(key(true));
    let _ = b.cook_host_key(key(false));
    assert_eq!(
        b.current_led_bits(),
        input::Led::CAPSLOCK.bits(),
        "caps toggle must raise the CAPSLOCK LED bit",
    );
    let _ = b.cook_host_key(key(true));
    let _ = b.cook_host_key(key(false));
    assert_eq!(b.current_led_bits(), 0, "second toggle clears the LED bit");
}

fn caps_key(pressed: bool) -> yserver_core::host_x11::HostKeyEvent {
    // 66 == X keycode for Caps Lock (evdev KEY_CAPSLOCK 58 + 8).
    yserver_core::host_x11::HostKeyEvent {
        origin: yserver_core::core_loop::InputOrigin::NestedHost,
        keycode: 66,
        pressed,
        state: 0,
        root_x: 0,
        root_y: 0,
        event_x: 0,
        event_y: 0,
        time: 0,
    }
}

/// xdotool through the core loop, after the xmodmap caps-as-control
/// script (the vng scenario `xmodmap-caps-control.sh`): libxdo's
/// GetMap(XkbAllClientInfoMask) + GetNames(KeyTypeNames|KTLevelNames|
/// VirtualModNames) decode as libX11 does, and every name its type
/// entries need resolves with GetAtomName (Xorg: `xorg-xkb-pristine.txt`
/// names every type, level and vmod).
#[test]
fn xdotool_names_resolve_after_xmodmap() {
    let cases = parse_xkb_smm_golden(include_str!(
        "../../../testdata/xorg-xkb-set-modifier-mapping.txt"
    ));
    for case in cases.iter().filter(|c| c.name == "xmodmap-replay") {
        let mut backend = KmsBackend::for_tests();
        let mut state = yserver_core::server::ServerState::new();
        let mut peer = kbd_map_client(&mut state);
        for step in &case.steps {
            match &step.request {
                SmmRequest::Ckm {
                    first,
                    kpk,
                    count,
                    syms,
                } => kbd_map_request(
                    &mut state,
                    &mut backend,
                    100,
                    *count,
                    &change_kbd_map_body(*first, *kpk, syms),
                ),
                SmmRequest::Smm { kpm, keys } => {
                    kbd_map_request(&mut state, &mut backend, 118, *kpm, keys);
                }
                other => panic!("xmodmap-replay step {other:?}"),
            }
        }
        // libX11's XkbUseExtension first (every other XKB request
        // draws BadAccess without it).
        kbd_map_request(&mut state, &mut backend, 136, 0, &[1, 0, 0, 0]);
        let _ = kbd_map_drain(&mut peer);
        let mut map_req = [0u8; 20];
        map_req[0..2].copy_from_slice(&0x0100u16.to_le_bytes());
        map_req[2] = 0x07;
        kbd_map_request(&mut state, &mut backend, 136, 8, &map_req);
        let map = kbd_map_drain(&mut peer);
        assert_eq!(map[0], 1, "GetMap reply: {:02x?}", &map[..8.min(map.len())]);
        // libX11 sends the device id GetMap reported.
        let mut names_req = [0u8; 8];
        names_req[0..2].copy_from_slice(&u16::from(map[1]).to_le_bytes());
        // XkbKeyTypeNamesMask | XkbKTLevelNamesMask | XkbVirtualModNamesMask.
        names_req[4..8].copy_from_slice(&0x08c0u32.to_le_bytes());
        kbd_map_request(&mut state, &mut backend, 136, 17, &names_req);
        let names = kbd_map_drain(&mut peer);
        assert_eq!(
            names[0],
            1,
            "GetNames reply: {:02x?}",
            &names[..8.min(names.len())]
        );
        let (types, levels, vmods) =
            crate::kms::xkb_desc::tests::libx11_names(&map, &names).expect("libX11 decode");
        let desc = &backend.core.xkb_desc;
        let mut needed: Vec<u32> = types.clone();
        for (i, t) in desc.types.iter().enumerate() {
            needed.extend(&levels[i]);
            let used = t.mods.vmods | t.map.iter().fold(0, |m, e| m | e.mods.vmods);
            needed.extend((0..16).filter(|v| used & (1 << v) != 0).map(|v| vmods[v]));
        }
        for atom in needed {
            kbd_map_request(&mut state, &mut backend, 17, 0, &atom.to_le_bytes());
            let r = kbd_map_drain(&mut peer);
            assert_eq!(
                r[0],
                1,
                "{}: GetAtomName({atom:#x}) → {:02x?}",
                case.layout,
                &r[..8]
            );
        }
    }
}

/// The xmodmap caps-as-control script on the keyboard description:
/// `<CAPS>` rebound to `Control_L` (ChangeKeyboardMapping) and moved from
/// Lock to Control (SetModifierMapping): nothing can hold Lock any more.
/// (Rebinding alone isn't enough: a key left in Lock matches
/// `Any+Exactly(Lock)`, which locks Lock, on Xorg as here.)
fn caps_as_control(b: &mut KmsBackend) {
    let _ = b.apply_keyboard_mapping(66, 1, &[xkbcommon::xkb::keysyms::KEY_Control_L]);
    let mut modmap = [0u8; 256];
    modmap.copy_from_slice(&b.core.xkb_desc.modmap);
    modmap[66] = 0x04;
    let _ = b.apply_modifier_mapping(&modmap);
}

/// A full RMLVO reload starts from a fresh state (Caps Lock off), so
/// the keyboard LEDs must follow right away, not on the next key.
#[test]
fn rmlvo_reload_resyncs_the_lock_leds() {
    use yserver_core::backend::Backend;
    let mut b = KmsBackend::for_tests();
    let _ = b.cook_host_key(caps_key(true));
    let _ = b.cook_host_key(caps_key(false));
    assert_eq!(b.leds_sent, input::Led::CAPSLOCK.bits(), "precondition");
    assert!(
        b.set_keymap_rmlvo("evdev", "pc105", "de", "", None)
            .is_some()
    );
    assert_eq!(b.current_led_bits(), 0, "fresh state");
    assert_eq!(b.leds_sent, 0, "LEDs resynced by the reload");
}

/// An edit carries Caps Lock across, but when the edit takes away the
/// only key that locks it, the lock is released and the LED must go off.
#[test]
fn keymap_edit_resyncs_the_lock_leds() {
    let mut b = KmsBackend::for_tests();
    let _ = b.cook_host_key(caps_key(true));
    let _ = b.cook_host_key(caps_key(false));
    assert_eq!(b.leds_sent, input::Led::CAPSLOCK.bits(), "precondition");

    // Same description: Caps Lock and its LED stay on.
    let same = b.core.xkb_desc.clone();
    b.install_desc(same).expect("compiles");
    assert_eq!(b.leds_sent, input::Led::CAPSLOCK.bits(), "lock carried");

    caps_as_control(&mut b);
    assert_eq!(b.current_led_bits(), 0, "no key locks Lock any more");
    assert_eq!(b.leds_sent, 0, "LEDs resynced by the edit");
}

/// GetKbdByName for the base layout after an edit reloads it (Xorg
/// reloads on every load), where a pristine keymap reports no change.
#[test]
fn get_kbd_by_name_reloads_an_edited_keymap() {
    use yserver_core::backend::{Backend, KeymapLoad};
    let mut b = KmsBackend::for_tests();
    let symbols = "pc+us+de:2+us:3+inet(evdev)";
    assert!(matches!(
        b.load_keymap_by_components(symbols),
        KeymapLoad::Loaded { changed: true, .. }
    ));
    assert!(matches!(
        b.load_keymap_by_components(symbols),
        KeymapLoad::Loaded { changed: false, .. }
    ));
    caps_as_control(&mut b);
    assert!(matches!(
        b.load_keymap_by_components(symbols),
        KeymapLoad::Loaded { changed: true, .. }
    ));
    assert_eq!(
        b.core
            .xkb_keymap
            .0
            .key_get_syms_by_level(xkbcommon::xkb::Keycode::new(66), 0, 0)[0]
            .raw(),
        xkbcommon::xkb::keysyms::KEY_Caps_Lock,
        "the reload dropped the edit"
    );
}

/// `_XKB_RULES_NAMES` keeps naming the base RMLVO through an edit, and
/// `setxkbmap` with that same RMLVO then reloads.
#[test]
fn set_keymap_rmlvo_reloads_an_edited_keymap() {
    use yserver_core::backend::Backend;
    let mut b = KmsBackend::for_tests();
    let names = b.current_xkb_rules_names();
    caps_as_control(&mut b);
    assert_eq!(b.current_xkb_rules_names(), names, "base RMLVO reported");
    let [r, m, l, v, o] = names.expect("names");
    let opts = (!o.is_empty()).then_some(o.as_str());
    assert!(
        b.set_keymap_rmlvo(&r, &m, &l, &v, opts).is_some(),
        "reloads"
    );
    assert_eq!(b.set_keymap_rmlvo(&r, &m, &l, &v, opts), None, "pristine");
}

#[test]
fn xi_dynamic_grabs_kms_slave_cleanup_does_not_freeze_paired_master() {
    use yserver_core::{
        backend::Backend,
        core_loop::{DeviceInfo, HostInputEvent, message::LibinputConfigSnapshot},
        host_x11::HostKeyEvent,
        server::ServerState,
        xinput::{InputCapabilities, InputSourceId, XiFacetKind},
    };

    const CLIENT: u32 = 7;
    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    let _peer = kbd_map_client_id(&mut state, CLIENT);
    let source_id = InputSourceId(0xA11);
    backend.on_host_input(
        &mut state,
        HostInputEvent::DeviceAdded(DeviceInfo {
            source_id,
            enabled: true,
            resume_key: None,
            capabilities: InputCapabilities {
                keyboard: false,
                pointer: true,
                touch: false,
            },
            name: "floating pointer".to_owned(),
            device_node: "/dev/input/test-floating-pointer".to_owned(),
            sysname: "test-floating-pointer".to_owned(),
            vendor_id: 0,
            product_id: 0,
            is_touchpad: false,
            config: LibinputConfigSnapshot::default(),
        }),
    );
    let device_id = state
        .xi_devices
        .facet(source_id, XiFacetKind::PointerTouch)
        .expect("DeviceAdded publishes the pointer facet");

    let mut grab_body = Vec::with_capacity(20);
    grab_body.extend_from_slice(&yserver_core::resources::ROOT_WINDOW.0.to_le_bytes());
    grab_body.extend_from_slice(&0u32.to_le_bytes());
    grab_body.extend_from_slice(&0u32.to_le_bytes());
    grab_body.extend_from_slice(&device_id.to_le_bytes());
    grab_body.extend_from_slice(&[1, 0, 0, 0]); // async, requested paired sync, owner_events=false
    grab_body.extend_from_slice(&0u16.to_le_bytes());
    yserver_core::core_loop::process_request::process_request(
        &mut state,
        &mut backend,
        yserver_protocol::x11::ClientId(CLIENT),
        yserver_protocol::x11::SequenceNumber(1),
        yserver_protocol::x11::RequestHeader {
            opcode: 137,
            data: 51,
            length_units: 6,
        },
        &grab_body,
        None,
    )
    .expect("XIGrabDevice on the source's pointer facet");
    assert!(state.xi2_pointer_grabs.contains_key(&device_id));
    assert_eq!(
        state.xi_devices.device(device_id).unwrap().attached_master,
        None
    );

    assert!(state.floating_pointer_positions.contains_key(&device_id));
    assert_eq!(
        state
            .xi1_frozen
            .get(&yserver_core::xinput::DEVICEID_MASTER_KEYBOARD)
            .and_then(|freeze| freeze.other),
        None,
        "Xorg forces a slave grab's paired mode to Async",
    );
    backend.on_host_input(
        &mut state,
        HostInputEvent::Key(HostKeyEvent {
            origin: yserver_core::core_loop::InputOrigin::NestedHost,
            pressed: true,
            keycode: 30,
            time: 0x1234,
            root_x: 10,
            root_y: 20,
            event_x: 10,
            event_y: 20,
            state: 0,
        }),
    );
    assert!(
        state.sync_pending.is_empty(),
        "the paired master keyboard is not frozen"
    );

    backend.on_host_input(&mut state, HostInputEvent::DeviceRemoved { source_id });
    assert!(state.xi_devices.source(source_id).is_none());
    assert!(state.xi_devices.device(device_id).is_none());
    assert!(!state.xi2_pointer_grabs.contains_key(&device_id));
    assert!(!state.xi2_detached_masters.contains_key(&device_id));
    assert!(!state.floating_pointer_positions.contains_key(&device_id));
    assert!(!state.xi1_frozen.contains_key(&device_id));
    assert!(state.sync_pending.is_empty());
    assert!(
        !state
            .xi1_frozen
            .get(&yserver_core::xinput::DEVICEID_MASTER_KEYBOARD)
            .is_some_and(yserver_core::server::Xi1Freeze::frozen),
        "source removal leaves the paired master keyboard unfrozen",
    );
}

#[test]
fn key_source_selection_repeat_after_another_keyboard_becomes_active() {
    use yserver_core::{
        core_loop::{DeviceInfo, HostInputEvent, InputOrigin, message::LibinputConfigSnapshot},
        host_x11::HostKeyEvent,
        server::ServerState,
        xinput::{InputCapabilities, InputSourceId, XiFacetKind},
    };

    const A: InputSourceId = InputSourceId(0xA31);
    const B: InputSourceId = InputSourceId(0xA32);

    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    for (source_id, name) in [(A, "Keyboard A"), (B, "Keyboard B")] {
        backend.on_host_input(
            &mut state,
            HostInputEvent::DeviceAdded(DeviceInfo {
                source_id,
                enabled: true,
                resume_key: None,
                capabilities: InputCapabilities {
                    keyboard: true,
                    pointer: false,
                    touch: false,
                },
                name: name.to_owned(),
                device_node: format!("/dev/input/{name}"),
                sysname: name.to_owned(),
                vendor_id: 0,
                product_id: 0,
                is_touchpad: false,
                config: LibinputConfigSnapshot::default(),
            }),
        );
    }
    let a_id = state
        .xi_devices
        .facet(A, XiFacetKind::Keyboard)
        .expect("Keyboard A facet");
    let b_id = state
        .xi_devices
        .facet(B, XiFacetKind::Keyboard)
        .expect("Keyboard B facet");
    let key = |source_id, keycode, pressed| {
        HostInputEvent::Key(HostKeyEvent {
            origin: InputOrigin::Physical(source_id),
            pressed,
            keycode,
            time: 0,
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            state: 0,
        })
    };

    yserver_core::core_loop::run::handle_host_input(&mut state, &mut backend, key(A, 38, true));
    yserver_core::core_loop::run::handle_host_input(&mut state, &mut backend, key(B, 39, true));

    assert_eq!(
        state
            .key_repeats
            .get(&InputOrigin::Physical(A))
            .map(|repeat| repeat.event.origin),
        Some(InputOrigin::Physical(A)),
        "Keyboard B's activity must not steal Keyboard A's repeat attribution",
    );
    assert!(state.key_down_by_device[&a_id].contains_key(&38));
    assert!(state.key_down_by_device[&b_id].contains_key(&39));
    let forced_deadline = std::time::Instant::now() - std::time::Duration::from_millis(1);
    state
        .key_repeats
        .get_mut(&InputOrigin::Physical(A))
        .expect("Keyboard A repeat remains armed")
        .next_fire = forced_deadline;
    assert!(yserver_core::core_loop::run::fire_pending_repeats(
        &mut state,
        &mut backend,
    ));
    assert_eq!(
        state.key_repeats[&InputOrigin::Physical(A)].event.origin,
        InputOrigin::Physical(A),
        "the repeat remains attributed to A after B becomes active",
    );
    assert!(state.key_repeats[&InputOrigin::Physical(A)].next_fire > forced_deadline);
    assert_eq!(
        state.key_repeats[&InputOrigin::Physical(B)].event.origin,
        InputOrigin::Physical(B),
        "B's independent repeat remains attributed to B",
    );
    assert!(
        state.key_repeats.contains_key(&InputOrigin::Physical(B)),
        "B's held key retains its own timer after A repeats",
    );
    assert_eq!(state.key_repeats.len(), 2);
    assert_eq!(state.key_down_by_device.len(), 2);
    assert_eq!(
        state.key_down_by_device[&a_id]
            .keys()
            .copied()
            .collect::<Vec<_>>(),
        [38],
    );
    assert_eq!(
        state.key_down_by_device[&b_id]
            .keys()
            .copied()
            .collect::<Vec<_>>(),
        [39],
    );
    assert_eq!(
        backend.core.down_keys,
        std::collections::HashSet::from([38, 39])
    );
    assert!(state.unpublished_keyboard_keys_down.is_empty());
}

#[test]
fn key_source_selection_repeat_stops_after_source_key_is_drained() {
    use yserver_core::{
        core_loop::{DeviceInfo, HostInputEvent, InputOrigin, message::LibinputConfigSnapshot},
        host_x11::HostKeyEvent,
        server::ServerState,
        xinput::{InputCapabilities, InputSourceId, XiFacetKind},
    };

    const A: InputSourceId = InputSourceId(0xA34);
    const B: InputSourceId = InputSourceId(0xA35);

    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    for (source_id, name) in [(A, "Drain A"), (B, "Drain B")] {
        backend.on_host_input(
            &mut state,
            HostInputEvent::DeviceAdded(DeviceInfo {
                source_id,
                enabled: true,
                resume_key: None,
                capabilities: InputCapabilities {
                    keyboard: true,
                    pointer: false,
                    touch: false,
                },
                name: name.to_owned(),
                device_node: format!("/dev/input/{name}"),
                sysname: name.to_owned(),
                vendor_id: 0,
                product_id: 0,
                is_touchpad: false,
                config: LibinputConfigSnapshot::default(),
            }),
        );
    }
    let a_id = state
        .xi_devices
        .facet(A, XiFacetKind::Keyboard)
        .expect("A keyboard facet");
    let b_id = state
        .xi_devices
        .facet(B, XiFacetKind::Keyboard)
        .expect("B keyboard facet");
    let key = |source_id, keycode, pressed| {
        HostInputEvent::Key(HostKeyEvent {
            origin: InputOrigin::Physical(source_id),
            pressed,
            keycode,
            time: 0,
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            state: 0,
        })
    };
    yserver_core::core_loop::run::handle_host_input(&mut state, &mut backend, key(A, 38, true));
    yserver_core::core_loop::run::handle_host_input(&mut state, &mut backend, key(B, 39, true));
    backend.release_keyboard_source_keys(&mut state, A);
    state
        .key_repeats
        .get_mut(&InputOrigin::Physical(A))
        .expect("A repeat remains pending until the next timer check")
        .next_fire = std::time::Instant::now() - std::time::Duration::from_millis(1);

    assert!(!yserver_core::core_loop::run::fire_pending_repeats(
        &mut state,
        &mut backend,
    ));
    assert!(
        !state.key_repeats.contains_key(&InputOrigin::Physical(A)),
        "draining A's held key retires A's repeat",
    );
    assert!(state.key_repeats.contains_key(&InputOrigin::Physical(B)));
    assert_eq!(state.key_repeats.len(), 1);
    assert!(!state.key_down_by_device.contains_key(&a_id));
    assert_eq!(state.key_down_by_device.len(), 1);
    assert_eq!(
        state
            .key_down_by_device
            .get(&b_id)
            .map(|keys| keys.keys().copied().collect::<Vec<_>>()),
        Some(vec![39]),
        "B's held key remains untouched",
    );
    assert!(state.unpublished_keyboard_keys_down.is_empty());
    assert!(!backend.core.down_keys.contains(&38));
    assert!(backend.core.down_keys.contains(&39));
}

#[test]
fn key_source_selection_repeat_floating_keyboard_arms_slave_repeat() {
    use yserver_core::{
        core_loop::{DeviceInfo, HostInputEvent, InputOrigin, message::LibinputConfigSnapshot},
        host_x11::HostKeyEvent,
        resources::ROOT_WINDOW,
        server::ServerState,
        xinput::{InputCapabilities, InputSourceId, XiFacetKind},
    };
    use yserver_protocol::x11::{ClientId, RequestHeader, SequenceNumber};

    const SOURCE: InputSourceId = InputSourceId(0xA33);
    const CLIENT: u32 = 8;

    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    state.core_focus.raw = ROOT_WINDOW.0;
    let mut peer = kbd_map_client_id(&mut state, CLIENT);
    backend.on_host_input(
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
            name: "Floating keyboard".into(),
            device_node: "/dev/input/floating".into(),
            sysname: "floating".into(),
            vendor_id: 0,
            product_id: 0,
            is_touchpad: false,
            config: LibinputConfigSnapshot::default(),
        }),
    );
    let device_id = state
        .xi_devices
        .facet(SOURCE, XiFacetKind::Keyboard)
        .expect("keyboard facet");
    state
        .clients
        .get_mut(&CLIENT)
        .unwrap()
        .xi2_masks
        .insert((ROOT_WINDOW, device_id), (1 << 2) | (1 << 3));
    state
        .clients
        .get_mut(&CLIENT)
        .unwrap()
        .xi2_masks
        .insert((ROOT_WINDOW, 0), (1 << 2) | (1 << 3));
    let mut grab = Vec::with_capacity(24);
    grab.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    grab.extend_from_slice(&0u32.to_le_bytes());
    grab.extend_from_slice(&0u32.to_le_bytes());
    grab.extend_from_slice(&device_id.to_le_bytes());
    grab.extend_from_slice(&[1, 1, 0, 0]);
    grab.extend_from_slice(&1u16.to_le_bytes());
    grab.extend_from_slice(&((1u32 << 2) | (1u32 << 3)).to_le_bytes());
    yserver_core::core_loop::process_request::process_request(
        &mut state,
        &mut backend,
        ClientId(CLIENT),
        SequenceNumber(1),
        RequestHeader {
            opcode: 137,
            data: 51,
            length_units: 7,
        },
        &grab,
        None,
    )
    .expect("exact keyboard grab floats this slave");
    let _ = kbd_map_drain(&mut peer);
    assert_eq!(
        state.xi_devices.device(device_id).unwrap().attached_master,
        None
    );

    yserver_core::core_loop::run::handle_host_input(
        &mut state,
        &mut backend,
        HostInputEvent::Key(HostKeyEvent {
            origin: InputOrigin::Physical(SOURCE),
            pressed: true,
            keycode: 38,
            time: 0,
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            state: 0,
        }),
    );

    assert_eq!(
        state
            .key_repeats
            .get(&InputOrigin::Physical(SOURCE))
            .map(|repeat| repeat.event.origin),
        Some(InputOrigin::Physical(SOURCE)),
        "a floating held key repeats in its slave view",
    );
    assert!(
        backend.floating_keyboard_states[&device_id]
            .down_keys
            .contains(&38)
    );
    assert!(!backend.core.down_keys.contains(&38));
    assert!(state.keys_down.iter().all(|byte| *byte == 0));
    let master_modifiers = backend.serialize_modifiers();
    let forced_deadline = std::time::Instant::now() - std::time::Duration::from_millis(1);
    state
        .key_repeats
        .get_mut(&InputOrigin::Physical(SOURCE))
        .expect("floating key has a source-local timer")
        .next_fire = forced_deadline;
    assert!(yserver_core::core_loop::run::fire_pending_repeats(
        &mut state,
        &mut backend,
    ));
    assert_eq!(
        state.key_repeats[&InputOrigin::Physical(SOURCE)]
            .event
            .origin,
        InputOrigin::Physical(SOURCE),
        "a floating repeat stays attributed to its slave source",
    );
    assert!(state.key_repeats[&InputOrigin::Physical(SOURCE)].next_fire > forced_deadline);
    assert!(
        backend.floating_keyboard_states[&device_id]
            .down_keys
            .contains(&38)
    );
    assert!(!backend.core.down_keys.contains(&38));
    assert!(state.keys_down.iter().all(|byte| *byte == 0));
    assert_eq!(state.key_down_by_device.len(), 1);
    assert_eq!(
        state.key_down_by_device[&device_id]
            .keys()
            .copied()
            .collect::<Vec<_>>(),
        [38]
    );
    assert_eq!(state.key_repeats.len(), 1);
    assert!(state.unpublished_keyboard_keys_down.is_empty());
    assert_eq!(backend.serialize_modifiers(), master_modifiers);
    assert!(
        state
            .key_repeats
            .contains_key(&InputOrigin::Physical(SOURCE))
    );
}

#[test]
fn xi_dynamic_grabs_floating_keyboard() {
    use yserver_core::{
        backend::Backend,
        core_loop::{DeviceInfo, HostInputEvent, message::LibinputConfigSnapshot},
        host_x11::HostKeyEvent,
        resources::ROOT_WINDOW,
        server::ServerState,
        xinput::{InputCapabilities, InputSourceId, XiFacetKind},
    };
    use yserver_protocol::x11::{ClientId, RequestHeader, SequenceNumber};

    const GRAB_CLIENT: u32 = 7;
    const XI_MASTER_CLIENT: u32 = 8;
    const CORE_CLIENT: u32 = 9;
    const RAZER: InputSourceId = InputSourceId(0xA11);
    const HYPERX: InputSourceId = InputSourceId(0xA12);
    const SHIFT_L: u8 = 50;
    const A_KEY: u8 = 38;
    const CAPS_LOCK: u8 = 66;
    const SHIFT_MASK: u16 = 0x01;
    const LOCK_MASK: u16 = 0x02;

    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    let mut grab_peer = kbd_map_client_id(&mut state, GRAB_CLIENT);
    let mut xi_master_peer = kbd_map_client_id(&mut state, XI_MASTER_CLIENT);
    let mut core_peer = kbd_map_client_id(&mut state, CORE_CLIENT);
    state.core_focus.raw = ROOT_WINDOW.0;

    for (source_id, name) in [(RAZER, "Razer"), (HYPERX, "HyperX")] {
        backend.on_host_input(
            &mut state,
            HostInputEvent::DeviceAdded(DeviceInfo {
                source_id,
                enabled: true,
                resume_key: None,
                capabilities: InputCapabilities {
                    keyboard: true,
                    pointer: false,
                    touch: false,
                },
                name: name.to_owned(),
                device_node: format!("/dev/input/{name}"),
                sysname: name.to_owned(),
                vendor_id: 0,
                product_id: 0,
                is_touchpad: false,
                config: LibinputConfigSnapshot::default(),
            }),
        );
    }
    let razer_id = state
        .xi_devices
        .facet(RAZER, XiFacetKind::Keyboard)
        .expect("Razer keyboard facet");
    let hyperx_id = state
        .xi_devices
        .facet(HYPERX, XiFacetKind::Keyboard)
        .expect("HyperX keyboard facet");

    // Select the attached master forms independently: XI2 on one
    // connection, core key events on another.
    state
        .clients
        .get_mut(&XI_MASTER_CLIENT)
        .unwrap()
        .xi2_masks
        .insert(
            (ROOT_WINDOW, yserver_core::xinput::DEVICEID_MASTER_KEYBOARD),
            (1 << 2) | (1 << 3),
        );
    state
        .clients
        .get_mut(&CORE_CLIENT)
        .unwrap()
        .event_masks
        .insert(ROOT_WINDOW, 0x0000_0001 | 0x0000_0002);

    let key = |source_id, keycode, pressed| {
        HostInputEvent::Key(HostKeyEvent {
            origin: yserver_core::core_loop::InputOrigin::Physical(source_id),
            pressed,
            keycode,
            time: 0x1234,
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            state: 0,
        })
    };
    let send_key =
        |backend: &mut KmsBackend, state: &mut ServerState, source_id, keycode, pressed| {
            yserver_core::core_loop::run::handle_host_input(
                state,
                backend,
                key(source_id, keycode, pressed),
            );
        };

    // A slave attached before it floats already has the master's locked
    // state, as Xorg's XkbPushLockedStateToSlaves keeps it synchronized.
    for pressed in [true, false] {
        send_key(&mut backend, &mut state, HYPERX, CAPS_LOCK, pressed);
    }
    let _ = kbd_map_drain(&mut xi_master_peer);
    let _ = kbd_map_drain(&mut core_peer);
    assert_ne!(backend.current_led_bits(), 0);

    let xi_grab = |device_id: u16| {
        let mut body = Vec::with_capacity(24);
        body.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&device_id.to_le_bytes());
        body.extend_from_slice(&[1, 1, 0, 0]); // async grab and paired device, no owner events
        body.extend_from_slice(&1u16.to_le_bytes());
        body.extend_from_slice(&((1u32 << 2) | (1u32 << 3)).to_le_bytes());
        body
    };
    let request = |minor_opcode, length_units| RequestHeader {
        opcode: 137,
        data: minor_opcode,
        length_units,
    };
    yserver_core::core_loop::process_request::process_request(
        &mut state,
        &mut backend,
        ClientId(GRAB_CLIENT),
        SequenceNumber(1),
        request(51, 7),
        &xi_grab(razer_id),
        None,
    )
    .expect("XIGrabDevice on Razer's keyboard facet");
    let _ = kbd_map_drain(&mut grab_peer);
    assert_eq!(
        state.xi_devices.device(razer_id).unwrap().attached_master,
        None,
        "the explicit slave grab floats only Razer",
    );
    assert!(
        backend.floating_keyboard_states.contains_key(&razer_id),
        "detaching Razer allocates its independent XKB state",
    );
    assert_ne!(
        backend.serialize_modifiers() & LOCK_MASK,
        0,
        "HyperX's Caps Lock establishes a master locked modifier before Razer floats",
    );
    assert_eq!(
        KmsBackend::serialize_xkb_modifiers(
            &backend.floating_keyboard_states[&razer_id].xkb_state.0,
            backend.floating_keyboard_states[&razer_id].locked_group,
            backend.keymap_group_count(),
        ) & LOCK_MASK,
        backend.serialize_modifiers() & LOCK_MASK,
        "Razer inherits the master's locked modifier when its floating state is created",
    );
    assert_eq!(
        state.xi_devices.device(hyperx_id).unwrap().attached_master,
        Some(yserver_core::xinput::DEVICEID_MASTER_KEYBOARD),
        "HyperX remains attached to the master keyboard",
    );

    let master_mods_before = backend.serialize_modifiers();
    let master_leds_before = backend.current_led_bits();
    let mut razer_bytes = Vec::new();
    send_key(&mut backend, &mut state, RAZER, SHIFT_L, true);
    razer_bytes.extend(kbd_map_drain(&mut grab_peer));
    assert!(
        backend.floating_keyboard_states[&razer_id]
            .down_keys
            .contains(&SHIFT_L),
        "the first floating Shift press reaches the local XKB state; facet={:?}, attached={:?}, active={:?}, master_active={:?}, bytes={}, outbound={}",
        state
            .xi_devices
            .facet(RAZER, yserver_core::xinput::XiFacetKind::Keyboard),
        state.xi_devices.device(razer_id).unwrap().attached_master,
        state.xi2_keyboard_grabs.get(&razer_id),
        state.active_keyboard_grab,
        razer_bytes.len(),
        state.clients[&GRAB_CLIENT].outbound.len(),
    );
    assert_eq!(backend.serialize_modifiers(), master_mods_before);
    assert_eq!(backend.current_led_bits(), master_leds_before);
    send_key(&mut backend, &mut state, RAZER, SHIFT_L, true);
    razer_bytes.extend(kbd_map_drain(&mut grab_peer));
    send_key(&mut backend, &mut state, RAZER, A_KEY, true);
    razer_bytes.extend(kbd_map_drain(&mut grab_peer));
    assert!(
        state
            .key_repeats
            .contains_key(&yserver_core::core_loop::InputOrigin::Physical(RAZER)),
        "a floating held key arms repeat for its slave view",
    );
    assert_eq!(backend.serialize_modifiers(), master_mods_before);
    assert_eq!(backend.current_led_bits(), master_leds_before);
    send_key(&mut backend, &mut state, RAZER, A_KEY, false);
    razer_bytes.extend(kbd_map_drain(&mut grab_peer));
    assert_eq!(backend.serialize_modifiers(), master_mods_before);
    assert_eq!(backend.current_led_bits(), master_leds_before);
    send_key(&mut backend, &mut state, RAZER, SHIFT_L, false);
    razer_bytes.extend(kbd_map_drain(&mut grab_peer));
    assert_eq!(backend.serialize_modifiers(), master_mods_before);
    assert_eq!(backend.current_led_bits(), master_leds_before);
    assert!(
        backend.floating_keyboard_states[&razer_id]
            .down_keys
            .is_empty()
    );
    let razer_events = xi2_events(&razer_bytes);
    assert_eq!(
        razer_events
            .iter()
            .filter(|(kind, device, _, detail, _)| {
                *kind == 2 && *device == razer_id && *detail == u32::from(SHIFT_L)
            })
            .count(),
        1,
        "the floating keyboard's duplicate guard emits one Shift press: events={razer_events:?}, pending={}, xi1_frozen={:?}, active={:?}, outbound={}",
        state.sync_pending.len(),
        state.xi1_frozen.get(&razer_id),
        state.xi2_keyboard_grabs.get(&razer_id),
        state.clients[&GRAB_CLIENT].outbound.len(),
    );
    assert!(
        razer_events.iter().any(|(kind, device, _, detail, _)| {
            *kind == 2 && *device == razer_id && *detail == u32::from(A_KEY)
        }),
        "floating key events are delivered through Razer's XI2 slave grab: {razer_events:?}",
    );
    let razer_shifted_a = {
        let bytes = &razer_bytes;
        let mut offset = 0usize;
        let mut shifted = false;
        while offset + 32 <= bytes.len() {
            if bytes[offset] == 35 {
                let length =
                    u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
                let event_len = 32 + length * 4;
                if offset + event_len > bytes.len() {
                    break;
                }
                let kind = u16::from_le_bytes([bytes[offset + 8], bytes[offset + 9]]);
                let device = u16::from_le_bytes([bytes[offset + 10], bytes[offset + 11]]);
                let detail =
                    u32::from_le_bytes(bytes[offset + 16..offset + 20].try_into().unwrap());
                let effective_mods =
                    u32::from_le_bytes(bytes[offset + 72..offset + 76].try_into().unwrap()) as u16;
                shifted |= kind == 2
                    && device == razer_id
                    && detail == u32::from(A_KEY)
                    && effective_mods & SHIFT_MASK != 0;
                offset += event_len;
            } else {
                offset += 32;
            }
        }
        shifted
    };
    assert!(
        razer_shifted_a,
        "Razer's own XI2 key event must report its floating Shift modifier",
    );

    // Caps Lock on floating Razer updates only its slave view; it does
    // not change the attached master's modifier state or LED snapshot.
    let mut razer_caps_bytes = Vec::new();
    for _ in 0..2 {
        for pressed in [true, false] {
            send_key(&mut backend, &mut state, RAZER, CAPS_LOCK, pressed);
        }
        send_key(&mut backend, &mut state, RAZER, A_KEY, true);
        assert!(
            state
                .key_repeats
                .contains_key(&yserver_core::core_loop::InputOrigin::Physical(RAZER)),
            "a floating held key arms repeat for its slave view",
        );
        send_key(&mut backend, &mut state, RAZER, A_KEY, false);
        assert_eq!(backend.serialize_modifiers(), master_mods_before);
        assert_eq!(backend.current_led_bits(), master_leds_before);
        razer_caps_bytes.extend(kbd_map_drain(&mut grab_peer));
    }
    let razer_caps_events = xi2_events(&razer_caps_bytes);
    let caps_lock_states = {
        let mut offset = 0usize;
        let mut states = Vec::new();
        while offset + 32 <= razer_caps_bytes.len() {
            if razer_caps_bytes[offset] == 35 {
                let length = u32::from_le_bytes(
                    razer_caps_bytes[offset + 4..offset + 8].try_into().unwrap(),
                ) as usize;
                let event_len = 32 + length * 4;
                if offset + event_len > razer_caps_bytes.len() {
                    break;
                }
                let kind = u16::from_le_bytes([
                    razer_caps_bytes[offset + 8],
                    razer_caps_bytes[offset + 9],
                ]);
                let device = u16::from_le_bytes([
                    razer_caps_bytes[offset + 10],
                    razer_caps_bytes[offset + 11],
                ]);
                let detail = u32::from_le_bytes(
                    razer_caps_bytes[offset + 16..offset + 20]
                        .try_into()
                        .unwrap(),
                );
                let effective_mods = u32::from_le_bytes(
                    razer_caps_bytes[offset + 72..offset + 76]
                        .try_into()
                        .unwrap(),
                ) as u16;
                if kind == 2 && device == razer_id && detail == u32::from(A_KEY) {
                    states.push(effective_mods & LOCK_MASK);
                }
                offset += event_len;
            } else {
                offset += 32;
            }
        }
        states
    };
    assert!(
        razer_caps_events
            .iter()
            .any(|(kind, device, _, detail, _)| {
                *kind == 2 && *device == razer_id && *detail == u32::from(A_KEY)
            }),
        "floating Razer key events continue to target its own slave grab",
    );
    assert_eq!(
        caps_lock_states,
        vec![0, LOCK_MASK],
        "Razer's own XI2 events report its independent Caps transitions",
    );
    assert!(
        kbd_map_drain(&mut xi_master_peer).is_empty(),
        "floating Razer key events have no master XI2 delivery",
    );
    assert!(
        kbd_map_drain(&mut core_peer).is_empty(),
        "floating Razer key events have no core delivery",
    );

    // The independently attached HyperX keyboard still delivers plain,
    // unshifted input to both the master XI2 and core views.
    send_key(&mut backend, &mut state, HYPERX, A_KEY, true);
    send_key(&mut backend, &mut state, HYPERX, A_KEY, false);
    let xi_master_bytes = kbd_map_drain(&mut xi_master_peer);
    let xi_master_events = xi2_events(&xi_master_bytes);
    assert!(
        xi_master_events.iter().any(|(kind, device, _, detail, _)| {
            *kind == 2
                && *device == yserver_core::xinput::DEVICEID_MASTER_KEYBOARD
                && *detail == u32::from(A_KEY)
        }),
        "attached HyperX input reaches the master XI2 selector: {xi_master_events:?}",
    );
    let master_a = xi_master_events.iter().any(|(kind, device, _, detail, _)| {
        *kind == 2
            && *device == yserver_core::xinput::DEVICEID_MASTER_KEYBOARD
            && *detail == u32::from(A_KEY)
    });
    assert!(master_a, "master XI2 includes HyperX's A key");
    let master_xi2_a_state = {
        let mut offset = 0usize;
        let mut key_state = None;
        while offset + 32 <= xi_master_bytes.len() {
            if xi_master_bytes[offset] == 35 {
                let length =
                    u32::from_le_bytes(xi_master_bytes[offset + 4..offset + 8].try_into().unwrap())
                        as usize;
                let event_len = 32 + length * 4;
                if offset + event_len > xi_master_bytes.len() {
                    break;
                }
                let kind =
                    u16::from_le_bytes([xi_master_bytes[offset + 8], xi_master_bytes[offset + 9]]);
                let device = u16::from_le_bytes([
                    xi_master_bytes[offset + 10],
                    xi_master_bytes[offset + 11],
                ]);
                let detail = u32::from_le_bytes(
                    xi_master_bytes[offset + 16..offset + 20]
                        .try_into()
                        .unwrap(),
                );
                if kind == 2
                    && device == yserver_core::xinput::DEVICEID_MASTER_KEYBOARD
                    && detail == u32::from(A_KEY)
                {
                    key_state = Some(u32::from_le_bytes(
                        xi_master_bytes[offset + 72..offset + 76]
                            .try_into()
                            .unwrap(),
                    ) as u16);
                }
                offset += event_len;
            } else {
                offset += 32;
            }
        }
        key_state
    };
    assert_eq!(
        master_xi2_a_state.map(|mask| mask & (SHIFT_MASK | LOCK_MASK)),
        Some(master_mods_before & (SHIFT_MASK | LOCK_MASK)),
        "master XI2 A event from HyperX is unshifted and keeps the master Caps lock",
    );
    assert_eq!(
        backend.serialize_modifiers() & (SHIFT_MASK | LOCK_MASK),
        master_mods_before & (SHIFT_MASK | LOCK_MASK),
        "floating Razer modifiers must not change the master modifier state",
    );
    let core_events = kbd_map_drain(&mut core_peer);
    assert!(
        core_events
            .chunks_exact(32)
            .any(|event| event[0] == 2 && event[1] == A_KEY),
        "attached HyperX input reaches the core key selector",
    );
    let core_a_state = core_events
        .chunks_exact(32)
        .find(|event| event[0] == 2 && event[1] == A_KEY)
        .map(|event| u16::from_le_bytes([event[28], event[29]]));
    assert_eq!(
        core_a_state.map(|mask| mask & (SHIFT_MASK | LOCK_MASK)),
        Some(master_mods_before & (SHIFT_MASK | LOCK_MASK)),
        "core A event from HyperX is unshifted and keeps the master Caps lock",
    );

    // Explicit grab reattachment follows Xorg's ReattachToOldMaster →
    // AttachDevice path: it does not call ReleaseButtonsAndKeys. A key
    // still physically held at ungrab is not synthesized as a release.
    send_key(&mut backend, &mut state, RAZER, SHIFT_L, true);
    let _ = kbd_map_drain(&mut grab_peer);
    // XIUngrabDevice restores Razer's original master; its floating
    // XKB state is retired and does not become master state.
    let mut ungrab_body = Vec::with_capacity(8);
    ungrab_body.extend_from_slice(&0u32.to_le_bytes());
    ungrab_body.extend_from_slice(&razer_id.to_le_bytes());
    ungrab_body.extend_from_slice(&[0u8; 2]);
    yserver_core::core_loop::process_request::process_request(
        &mut state,
        &mut backend,
        ClientId(GRAB_CLIENT),
        SequenceNumber(2),
        request(52, 3),
        &ungrab_body,
        None,
    )
    .expect("XIUngrabDevice reattaches Razer");
    let _ = kbd_map_drain(&mut grab_peer);
    assert_eq!(
        state.xi_devices.device(razer_id).unwrap().attached_master,
        Some(yserver_core::xinput::DEVICEID_MASTER_KEYBOARD),
    );
    assert!(
        !backend.floating_keyboard_states.contains_key(&razer_id),
        "reattachment retires Razer's floating XKB state",
    );
    assert!(!state.xi2_keyboard_grabs.contains_key(&razer_id));
    assert!(!state.xi2_detached_masters.contains_key(&razer_id));
    assert_eq!(backend.serialize_modifiers(), master_mods_before);
    assert_eq!(backend.current_led_bits(), master_leds_before);
    send_key(&mut backend, &mut state, RAZER, SHIFT_L, false);
    assert_eq!(backend.serialize_modifiers(), master_mods_before);
    assert!(
        kbd_map_drain(&mut core_peer).is_empty(),
        "Xorg's explicit-grab reattachment does not synthesize a core key release",
    );
    assert!(
        kbd_map_drain(&mut xi_master_peer).is_empty(),
        "Xorg's explicit-grab reattachment does not synthesize a master XI2 release",
    );
    assert!(backend.core.down_keys.is_empty());
    assert!(state.keys_down.iter().all(|&byte| byte == 0));
    assert!(state.sync_pending.is_empty());
    assert!(
        !state
            .xi1_frozen
            .get(&razer_id)
            .is_some_and(yserver_core::server::Xi1Freeze::frozen)
    );

    // Float it again and remove it while a local key is held. The state
    // is retired with the source and the unrelated HyperX facet survives.
    yserver_core::core_loop::process_request::process_request(
        &mut state,
        &mut backend,
        ClientId(GRAB_CLIENT),
        SequenceNumber(3),
        request(51, 7),
        &xi_grab(razer_id),
        None,
    )
    .expect("second XIGrabDevice on Razer");
    let _ = kbd_map_drain(&mut grab_peer);
    assert_eq!(
        KmsBackend::serialize_xkb_modifiers(
            &backend.floating_keyboard_states[&razer_id].xkb_state.0,
            backend.floating_keyboard_states[&razer_id].locked_group,
            backend.keymap_group_count(),
        ) & LOCK_MASK,
        master_mods_before & LOCK_MASK,
        "a new floating interval starts from the master's locked Caps state",
    );
    send_key(&mut backend, &mut state, RAZER, CAPS_LOCK, true);
    send_key(&mut backend, &mut state, RAZER, CAPS_LOCK, false);
    send_key(&mut backend, &mut state, RAZER, SHIFT_L, true);
    assert_eq!(backend.serialize_modifiers(), master_mods_before);
    backend.on_host_input(
        &mut state,
        HostInputEvent::DeviceRemoved { source_id: RAZER },
    );
    assert!(state.xi_devices.source(RAZER).is_none());
    assert!(state.xi_devices.device(razer_id).is_none());
    assert!(state.xi_devices.source(HYPERX).is_some());
    assert_eq!(
        state.xi_devices.device(hyperx_id).unwrap().attached_master,
        Some(yserver_core::xinput::DEVICEID_MASTER_KEYBOARD),
    );
    assert!(!state.xi2_keyboard_grabs.contains_key(&razer_id));
    assert!(!state.xi2_detached_masters.contains_key(&razer_id));
    assert!(!state.xi1_frozen.contains_key(&razer_id));
    assert!(backend.core.down_keys.is_empty());
    assert!(state.keys_down.iter().all(|&byte| byte == 0));
    assert!(state.sync_pending.is_empty());
    assert!(state.key_repeats.is_empty());
    assert!(backend.floating_keyboard_states.is_empty());
    assert_eq!(backend.serialize_modifiers(), master_mods_before);
    assert_eq!(backend.current_led_bits(), master_leds_before);
    assert!(backend.floating_keyboard_states.is_empty());
}
