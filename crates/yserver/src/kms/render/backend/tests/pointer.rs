use super::*;

/// Cinnamon alt-tab regression (2026-06-10): `query_pointer`'s
/// KeyButMask must include the LIVE keyboard modifier state, not
/// only the held buttons. `XIQueryPointer`'s ModifierInfo is
/// filled from this mask (Xorg fills the reply from the paired
/// MASTER_KEYBOARD's XKB state — Xi/xiquerypointer.c:120,139);
/// with the modifiers missing, cinnamon's switcher polled
/// `global.get_pointer()` right after pushModal, read "Alt not
/// held", and instantly cancelled the popup (instant window
/// switch, no switcher UI).
#[test]
fn query_pointer_mask_includes_live_keyboard_modifiers() {
    use yserver_core::{backend::Backend, host_x11::HostKeyEvent};
    let mut b = KmsBackend::for_tests();
    // 50 == evdev KEY_LEFTSHIFT — proven to flip a modifier bit in
    // the test keymap by `cook_host_key_fills_coords_and_modifier_state`.
    let raw = HostKeyEvent {
        origin: yserver_core::core_loop::InputOrigin::NestedHost,
        keycode: 50,
        pressed: true,
        state: 0,
        root_x: 0,
        root_y: 0,
        event_x: 0,
        event_y: 0,
        time: 0,
    };
    let _ = b.cook_host_key(raw);
    let p = Backend::query_pointer(&mut b, None).expect("query_pointer");
    assert_ne!(
        p.mask & 0x00ff,
        0,
        "KeyButMask from query_pointer must include the held modifier \
             — XIQueryPointer's ModifierInfo (cinnamon alt-tab) reads it",
    );
}

/// `process_pointer_button` honours the X11 spec's pre-press
/// `state` field: on ButtonPress the button bit is NOT yet
/// set in `state`, on ButtonRelease it IS still set.
/// `button_mask` is updated AFTER the event so the next
/// motion sees the new mask.
#[test]
fn process_pointer_button_state_field_is_pre_press() {
    use yserver_core::{host_x11::PointerEventKind, server::ServerState};
    let mut b = KmsBackend::for_tests();
    let state = ServerState::new();
    // BTN_LEFT press → detail=1, button bit = 0x0100.
    b.process_pointer_button(
        0x110,
        true,
        &state,
        yserver_core::core_loop::InputOrigin::XTest(4),
    );
    let press = b
        .core
        .pending_pointer_events
        .iter()
        .find(|e| matches!(e.kind, PointerEventKind::ButtonPress))
        .expect("ButtonPress emitted");
    assert_eq!(press.detail, 1);
    assert_eq!(
        press.state & 0x0100,
        0,
        "Button1 bit must NOT be set in ButtonPress.state (pre-press)"
    );
    assert_eq!(
        b.core.button_mask & 0x0100,
        0x0100,
        "button_mask updated post-event"
    );

    b.core.pending_pointer_events.clear();
    b.process_pointer_button(
        0x110,
        false,
        &state,
        yserver_core::core_loop::InputOrigin::XTest(4),
    );
    let release = b
        .core
        .pending_pointer_events
        .iter()
        .find(|e| matches!(e.kind, PointerEventKind::ButtonRelease))
        .expect("ButtonRelease emitted");
    assert_eq!(
        release.state & 0x0100,
        0x0100,
        "Button1 bit MUST be set in ButtonRelease.state (still held)"
    );
    assert_eq!(
        b.core.button_mask & 0x0100,
        0,
        "button_mask cleared post-release"
    );
}

#[test]
fn pointer_source_routing_kms_preserves_origin_and_drops_unknown_before_cursor_motion() {
    use yserver_core::{
        backend::Backend,
        core_loop::{DeviceInfo, HostInputEvent, InputOrigin, message::LibinputConfigSnapshot},
        server::ServerState,
        xinput::{InputCapabilities, InputSourceId},
    };

    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    backend
        .core
        .xid_map
        .insert(backend.core.window_id, yserver_core::resources::ROOT_WINDOW);
    let initial_cursor = (backend.core.cursor_x, backend.core.cursor_y);
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::PointerMotion {
            origin: InputOrigin::Physical(InputSourceId(999)),
            x: 123,
            y: 45,
            time: 1,
            relative: false,
            dx: 0,
            dy: 0,
            motion_delta: None,
        },
    );
    assert_eq!(
        (backend.core.cursor_x, backend.core.cursor_y),
        initial_cursor
    );
    assert_eq!(state.pointer_root, (0, 0));
    assert_eq!(backend.core.pending_pointer_events.len(), 0);
    assert_eq!(state.xi_devices.devices().len(), 4);

    let source = InputSourceId(201);
    state.xi_register_source(&DeviceInfo {
        source_id: source,
        enabled: true,
        resume_key: None,
        capabilities: InputCapabilities {
            keyboard: false,
            pointer: true,
            touch: false,
        },
        name: "test mouse".into(),
        device_node: "/dev/input/event201".into(),
        sysname: "event201".into(),
        vendor_id: 1,
        product_id: 2,
        is_touchpad: false,
        config: LibinputConfigSnapshot::default(),
    });
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::PointerMotion {
            origin: InputOrigin::Physical(source),
            x: 20,
            y: 30,
            time: 2,
            relative: false,
            dx: 0,
            dy: 0,
            motion_delta: None,
        },
    );
    assert_eq!((backend.core.cursor_x, backend.core.cursor_y), (20.0, 30.0));
    assert_eq!(state.pointer_root, (20, 30));
    assert_eq!(backend.core.pending_pointer_events.len(), 0);
    assert_eq!(state.xi_devices.devices().len(), 5);
    assert_eq!(
        state
            .xi_devices
            .facet(source, yserver_core::xinput::XiFacetKind::PointerTouch),
        Some(6)
    );
    assert_eq!(state.buttons_down, 0);

    // Exercise the KMS event builder directly as well: on_host_input
    // drains this production queue immediately into core fanout, while
    // process_pointer_absolute exposes the HostPointerEvents it builds.
    backend.process_pointer_absolute(
        &mut state,
        21.0,
        31.0,
        false,
        0,
        0,
        InputOrigin::Physical(source),
    );
    assert_eq!((backend.core.cursor_x, backend.core.cursor_y), (21.0, 31.0));
    let emitted = std::mem::take(&mut backend.core.pending_pointer_events);
    assert_eq!(
        emitted.iter().map(|event| event.kind).collect::<Vec<_>>(),
        vec![yserver_core::host_x11::PointerEventKind::MotionNotify],
    );
    assert!(
        emitted
            .iter()
            .all(|event| event.origin == InputOrigin::Physical(source))
    );
    assert!(backend.core.pending_pointer_events.is_empty());

    let mut disabled = state.xi_devices.source(source).unwrap().clone();
    disabled.enabled = false;
    state.xi_register_source(&disabled);
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::PointerMotion {
            origin: InputOrigin::Physical(source),
            x: 500,
            y: 400,
            time: 3,
            relative: false,
            dx: 0,
            dy: 0,
            motion_delta: None,
        },
    );
    assert_eq!((backend.core.cursor_x, backend.core.cursor_y), (21.0, 31.0));
    assert_eq!(state.pointer_root, (20, 30));
    assert!(backend.core.pending_pointer_events.is_empty());
    assert!(!state.xi_devices.device(6).unwrap().enabled);
    assert_eq!(state.xi_devices.devices().len(), 5);
}

#[test]
fn pointer_source_selection_kms_rejects_unknown_removed_and_suspended_button_sources() {
    use yserver_core::{
        backend::Backend,
        core_loop::{DeviceInfo, HostInputEvent, InputOrigin, message::LibinputConfigSnapshot},
        server::{ActivePointerGrab, ServerState},
        xinput::{InputCapabilities, InputSourceId},
    };

    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    backend
        .core
        .xid_map
        .insert(backend.core.window_id, yserver_core::resources::ROOT_WINDOW);
    let active_grab = ActivePointerGrab {
        owner: yserver_protocol::x11::ClientId(1),
        grab_window: yserver_core::resources::ROOT_WINDOW,
        event_mask: 0,
        cursor: yserver_protocol::x11::ResourceId(0),
        time: 7,
        owner_events: false,
        via_xi2: false,
        implicit: false,
        passive: false,
        xi2_mask: 0,
    };
    state.set_pointer_grab(active_grab);

    let make_info = |source_id: InputSourceId| DeviceInfo {
        source_id,
        enabled: true,
        resume_key: None,
        capabilities: InputCapabilities {
            keyboard: false,
            pointer: true,
            touch: false,
        },
        name: format!("button source {}", source_id.0),
        device_node: format!("/dev/input/event{}", source_id.0),
        sysname: format!("event{}", source_id.0),
        vendor_id: 1,
        product_id: source_id.0 as u32,
        is_touchpad: false,
        config: LibinputConfigSnapshot::default(),
    };
    let removed = InputSourceId(401);
    let suspended = InputSourceId(402);
    state.xi_register_source(&make_info(removed));
    state.xi_register_source(&make_info(suspended));
    assert_eq!(state.xi_unregister_source(removed), vec![6]);
    let mut suspended_info = state.xi_devices.source(suspended).unwrap().clone();
    suspended_info.enabled = false;
    state.xi_register_source(&suspended_info);

    let initial_cursor = (backend.core.cursor_x, backend.core.cursor_y);
    let initial_devices = state.xi_devices.devices().to_vec();
    let initial_device_input_state = initial_devices
        .iter()
        .map(|device| {
            (
                device.id,
                device.enabled,
                device.source_id,
                device.facet,
                device.attached_master,
                device.buttons_down,
                device.scroll_axis_values,
            )
        })
        .collect::<Vec<_>>();
    let grab_state = |grab: ActivePointerGrab| {
        (
            grab.owner,
            grab.grab_window,
            grab.event_mask,
            grab.cursor,
            grab.time,
            grab.owner_events,
            grab.via_xi2,
            grab.implicit,
            grab.passive,
            grab.xi2_mask,
        )
    };
    let initial_grab = state.active_pointer_grab.map(grab_state);
    for origin in [
        InputOrigin::Physical(InputSourceId(999)),
        InputOrigin::Physical(removed),
        InputOrigin::Physical(suspended),
    ] {
        Backend::on_host_input(
            &mut backend,
            &mut state,
            HostInputEvent::PointerButton {
                origin,
                button: 0x110,
                pressed: true,
                time: 10,
            },
        );
        assert_eq!(
            backend.core.button_mask, 0,
            "rejected input changes no KMS hold"
        );
        assert_eq!(
            (backend.core.cursor_x, backend.core.cursor_y),
            initial_cursor
        );
        assert!(backend.core.pending_pointer_events.is_empty());
        assert_eq!(
            state.buttons_down, 0,
            "rejected input changes no master hold"
        );
        assert_eq!(state.pointer_root, (0, 0));
        assert!(state.pointer_motion_history.is_empty());
        assert!(state.sync_pending.is_empty());
        assert!(state.xi1_device_input_state.is_empty());
        assert_eq!(state.active_pointer_grab.map(grab_state), initial_grab);
        assert_eq!(state.xi_devices.devices().len(), initial_devices.len());
        assert_eq!(
            state
                .xi_devices
                .devices()
                .iter()
                .map(|device| {
                    (
                        device.id,
                        device.enabled,
                        device.source_id,
                        device.facet,
                        device.attached_master,
                        device.buttons_down,
                        device.scroll_axis_values,
                    )
                })
                .collect::<Vec<_>>(),
            initial_device_input_state,
        );
        assert!(!state.xi_devices.source(suspended).unwrap().enabled);
    }
    assert!(state.xi_devices.source(removed).is_none());
    assert!(state.xi_devices.device(6).is_none());
    assert_eq!(state.xi_devices.device(7).unwrap().buttons_down, 0);
    assert_eq!(
        state.xi_devices.device(7).unwrap().scroll_axis_values,
        [0, 0]
    );
}

/// `warp_pointer_root` (the WarpPointer path on KMS) must move
/// the tracked cursor and fan the resulting motion out — the
/// fanout caches the position in `state.pointer_root`. Pre-fix
/// `warp_pointer` was a `log_render_gap` stub, so XWarpPointer never
/// moved the pointer and every xts5 Xlib11 event-delivery test
/// pressed buttons at the stale center position, missing its
/// test window ("Expected event not received" en masse).
#[test]
fn warp_pointer_root_moves_cursor_and_fans_out_motion() {
    use yserver_core::server::ServerState;
    let mut b = KmsBackend::for_tests();
    let mut state = ServerState::new();
    Backend::warp_pointer_root(&mut b, &mut state, 123, 45);
    assert_eq!(b.core.cursor_x, 123.0);
    assert_eq!(b.core.cursor_y, 45.0);
    assert_eq!(
        state.pointer_root,
        (123, 45),
        "the warp motion must reach the pointer fanout",
    );
}

/// `process_pointer_absolute` clamps to the output extent and
/// updates `cursor_x` / `cursor_y`. Single-output test fixture
/// reports 800×600 from PlatformBackend::for_tests.
#[test]
fn process_pointer_absolute_clamps_to_output() {
    use yserver_core::server::ServerState;
    let mut b = KmsBackend::for_tests();
    let mut state = ServerState::new();
    // Inside extent.
    b.process_pointer_absolute(
        &mut state,
        100.0,
        200.0,
        true,
        0,
        0,
        yserver_core::core_loop::InputOrigin::XTest(4),
    );
    assert_eq!(b.core.cursor_x, 100.0);
    assert_eq!(b.core.cursor_y, 200.0);
    // Past extent → clamped to (extent - 1).
    b.process_pointer_absolute(
        &mut state,
        5000.0,
        5000.0,
        true,
        0,
        0,
        yserver_core::core_loop::InputOrigin::XTest(4),
    );
    assert_eq!(b.core.cursor_x, 799.0);
    assert_eq!(b.core.cursor_y, 599.0);
}

/// Confinement must happen before KMS chooses the pointer window.
/// Otherwise motion beyond the right edge queues EnterNotify for an
/// overlapping window below before the core fanout clamps MotionNotify.
/// dwm focuses that entered window, making SDL release/reacquire its grab
/// and flicker the visible cursor continuously (#99).
#[test]
fn confined_motion_does_not_cross_into_window_below() {
    use yserver_core::{
        backend::WindowHandle,
        resources::{ROOT_VISUAL, ROOT_WINDOW},
        server::ServerState,
    };
    use yserver_protocol::x11::{ClientId, CreateWindowRequest, ResourceId};

    const BELOW: ResourceId = ResourceId(0x0020_0001);
    const GAME: ResourceId = ResourceId(0x0030_0001);
    const BELOW_HOST: u32 = 0x8000_0001;
    const GAME_HOST: u32 = 0x8000_0002;

    let mut b = KmsBackend::for_tests();
    let mut state = ServerState::new();
    for (xid, x, y, width, height, host) in [
        (BELOW, 0, 0, 800, 600, BELOW_HOST),
        (GAME, 100, 100, 100, 100, GAME_HOST),
    ] {
        state.resources.create_window(
            ClientId(1),
            CreateWindowRequest {
                depth: 24,
                window: xid,
                parent: ROOT_WINDOW,
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
        state.resources.window_mut(xid).unwrap().host_xid = WindowHandle::from_raw(host);
        assert!(state.resources.map_window(xid).mapping_changed);
        b.windows.insert(
            host,
            crate::kms::render::backend::WindowGeometry {
                border_width: 0,
                border_pixel: None,
                border_pixmap: None,
                x,
                y,
                width,
                height,
                depth: 24,
                mapped: true,
                viewable: true,
                parent: None,
                stack_rank: 0,
                bg_pixel: None,
                bg_pixmap: None,
                cursor: None,
            },
        );
        b.core.xid_map.insert(host, xid);
        b.core.top_level_order.push(host);
    }

    state.pointer_confine_to = GAME;
    state.pointer_root = (199, 150);
    b.core.cursor_x = 199.0;
    b.core.cursor_y = 150.0;
    b.core.prev_pointer_window = Some(GAME_HOST);

    b.process_pointer_absolute(
        &mut state,
        220.0,
        150.0,
        true,
        21,
        0,
        yserver_core::core_loop::InputOrigin::XTest(4),
    );

    assert_eq!((b.core.cursor_x, b.core.cursor_y), (199.0, 150.0));
    assert_eq!(state.pointer_root, (199, 150));
    assert_eq!(b.core.prev_pointer_window, Some(GAME_HOST));
}

/// Multi-output regression: the pointer clamp must use the
/// union framebuffer extent (`PlatformBackend.fb_w/fb_h`),
/// NOT `outputs.first().width/height`. Pre-fix the clamp
/// consulted only the first output, so the cursor could never
/// cross from monitor 0 onto monitor 1 in a side-by-side
/// layout.
///
/// Two side-by-side 2560×1440 monitors and the `fb_w` of 5120 that
/// `core_platform_init` computes as `max(x + width)` across them
/// (`kms/backend.rs:1063-1072`). The input thread already targets that
/// union extent at thread spawn, so v2 receives `PointerMotion { x, y }`
/// already in virtual-screen coords; the only divergence was v2's
/// re-clamp.
#[test]
fn process_pointer_absolute_uses_union_fb_extent_for_multi_output() {
    use yserver_core::server::ServerState;
    let mut b = KmsBackend::for_tests();
    push_test_output(&mut b, 2);
    for (i, output) in b.platform.outputs.iter_mut().enumerate() {
        output.x = 2560 * i32::try_from(i).unwrap();
        output.width = 2560;
        output.height = 1440;
    }
    b.platform.fb_w = 5120;
    b.platform.fb_h = 1440;
    let mut state = ServerState::new();
    // Point on monitor 1 (x=4000 is past output[0]'s 800-wide
    // fixture extent but well within the 5120 union extent).
    b.process_pointer_absolute(
        &mut state,
        4000.0,
        1000.0,
        true,
        0,
        0,
        yserver_core::core_loop::InputOrigin::XTest(4),
    );
    assert_eq!(
        b.core.cursor_x, 4000.0,
        "pointer must be able to cross past the first output's \
             extent; pre-fix this clamps to 799 and the cursor is \
             stuck on monitor 0",
    );
    assert_eq!(b.core.cursor_y, 1000.0);
    // Past the union extent → clamped to (union - 1).
    b.process_pointer_absolute(
        &mut state,
        9999.0,
        9999.0,
        true,
        0,
        0,
        yserver_core::core_loop::InputOrigin::XTest(4),
    );
    assert_eq!(b.core.cursor_x, 5119.0);
    assert_eq!(b.core.cursor_y, 1439.0);
}

/// Absolute motion must bypass pointer barriers. This mirrors the
/// touch/tablet path: the backend delivers the motion with
/// `relative=false`, so the core motion fanout keeps the absolute
/// coordinates untouched even when a solid barrier sits in the path.
#[test]
fn process_pointer_absolute_skips_pointer_barriers_when_absolute() {
    use yserver_core::{
        core_loop::HostInputEvent,
        server::{PointerBarrier, ServerState},
    };
    use yserver_protocol::x11::ClientId;

    let mut b = KmsBackend::for_tests();
    let mut state = ServerState::new();
    state.pointer_root = (90, 50);
    state.pointer_barriers.insert(
        0x0050_0001,
        PointerBarrier {
            owner: ClientId(1),
            window: yserver_core::resources::ROOT_WINDOW,
            x1: 100,
            y1: 0,
            x2: 100,
            y2: 200,
            directions: 0,
            devices: Vec::new(),
            hit: false,
            seen: false,
            event_id: 1,
            release_event_id: 0,
            last_timestamp: 0,
        },
    );

    b.on_host_input(
        &mut state,
        HostInputEvent::PointerMotion {
            origin: yserver_core::core_loop::InputOrigin::NestedHost,
            x: 110,
            y: 50,
            time: 0,
            relative: false,
            dx: 0,
            dy: 0,
            motion_delta: None,
        },
    );

    assert_eq!(b.core.cursor_x, 110.0);
    assert_eq!(b.core.cursor_y, 50.0);
    assert_eq!(state.pointer_root, (110, 50));
}

#[test]
fn kms_pointer_authority_integrates_relative_motion_against_pointer_barriers() {
    use yserver_core::{
        backend::Backend,
        core_loop::{DeviceInfo, HostInputEvent, InputOrigin, message::LibinputConfigSnapshot},
        server::{PointerBarrier, ServerState},
        xinput::{InputCapabilities, InputSourceId},
    };
    use yserver_protocol::x11::{ClientId, ResourceId};

    const SOURCE: InputSourceId = InputSourceId(0xB11);
    const BARRIER: ResourceId = ResourceId(0x0050_0001);

    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    backend.on_host_input(
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
            name: "barrier test pointer".to_owned(),
            device_node: "/dev/input/barrier-test".to_owned(),
            sysname: "barrier-test".to_owned(),
            vendor_id: 0,
            product_id: 0,
            is_touchpad: false,
            config: LibinputConfigSnapshot::default(),
        }),
    );
    state.pointer_barriers.insert(
        BARRIER.0,
        PointerBarrier {
            owner: ClientId(1),
            window: yserver_core::resources::ROOT_WINDOW,
            x1: 100,
            y1: 0,
            x2: 100,
            y2: 200,
            directions: 0,
            devices: Vec::new(),
            hit: false,
            seen: false,
            event_id: 1,
            release_event_id: 0,
            last_timestamp: 0,
        },
    );

    // Use the same solid barrier setup as the existing absolute-motion
    // test: release_event_id != event_id keeps it solid.
    Backend::warp_pointer_root(&mut backend, &mut state, 90, 50);
    let physical_motion = |time, x, dx, relative, motion_delta| HostInputEvent::PointerMotion {
        origin: InputOrigin::Physical(SOURCE),
        x,
        y: 50,
        time,
        relative,
        dx,
        dy: 0,
        motion_delta,
    };

    // Physical absolute input represents touch/tablet motion and bypasses
    // this barrier, as in the existing absolute-motion regression.
    backend.on_host_input(&mut state, physical_motion(1, 110, 11, false, None));
    assert_eq!(
        (backend.core.cursor_x, backend.core.cursor_y),
        (110.0, 50.0)
    );
    assert_eq!(state.pointer_root, (110, 50));
    Backend::warp_pointer_root(&mut backend, &mut state, 90, 50);

    // Consecutive physical relative events approach and cross x=100.
    // KMS must integrate each from its current (barrier-clamped) cursor.
    backend.on_host_input(
        &mut state,
        physical_motion(2, 700, 8, true, Some([8.0, 0.0])),
    );
    assert_eq!((backend.core.cursor_x, backend.core.cursor_y), (98.0, 50.0));
    assert_eq!(state.pointer_root, (98, 50));

    backend.on_host_input(
        &mut state,
        physical_motion(3, 700, 10, true, Some([10.0, 0.0])),
    );
    assert_eq!(
        (backend.core.cursor_x, backend.core.cursor_y),
        (99.0, 50.0),
        "relative motion must stop immediately before the solid barrier",
    );
    assert_eq!(state.pointer_root, (99, 50));

    backend.on_host_input(
        &mut state,
        physical_motion(4, 700, -5, true, Some([-5.0, 0.0])),
    );
    assert_eq!(
        (backend.core.cursor_x, backend.core.cursor_y),
        (94.0, 50.0),
        "the next relative delta away from the wall starts at the clamped cursor",
    );
    assert_eq!(state.pointer_root, (94, 50));

    assert_eq!(backend.core.cursor_x, state.pointer_root.0 as f32);
    assert_eq!(backend.core.cursor_y, state.pointer_root.1 as f32);
    assert!(backend.core.pending_pointer_events.is_empty());
    assert!(state.sync_pending.is_empty());
}

/// `window_under_cursor` returns the topmost mapped top-level
/// containing the cursor. Walks `core.top_level_order` back-to-
/// front so the most-recently-stacked window wins. Unmapped
/// windows skipped.
#[test]
fn window_under_cursor_finds_topmost_mapped() {
    let mut b = KmsBackend::for_tests();
    b.windows.insert(
        0x1000,
        crate::kms::render::backend::WindowGeometry {
            border_width: 0,
            border_pixel: None,
            border_pixmap: None,
            x: 0,
            y: 0,
            width: 100,
            height: 100,
            depth: 32,
            mapped: true,
            viewable: true,
            parent: None,
            stack_rank: 0,
            bg_pixel: None,
            bg_pixmap: None,
            cursor: None,
        },
    );
    b.windows.insert(
        0x2000,
        crate::kms::render::backend::WindowGeometry {
            border_width: 0,
            border_pixel: None,
            border_pixmap: None,
            x: 50,
            y: 50,
            width: 100,
            height: 100,
            depth: 32,
            mapped: true,
            viewable: true,
            parent: None,
            stack_rank: 1,
            bg_pixel: None,
            bg_pixmap: None,
            cursor: None,
        },
    );
    b.core.top_level_order.push(0x1000);
    b.core.top_level_order.push(0x2000);

    // Cursor in overlap (50..100, 50..100): 0x2000 wins (topmost).
    b.core.cursor_x = 75.0;
    b.core.cursor_y = 75.0;
    assert_eq!(b.window_under_cursor(), Some(0x2000));

    // Cursor outside overlap, only in 0x1000.
    b.core.cursor_x = 25.0;
    b.core.cursor_y = 25.0;
    assert_eq!(b.window_under_cursor(), Some(0x1000));

    // Cursor outside both — root-fallback handled at caller.
    b.core.cursor_x = 300.0;
    b.core.cursor_y = 300.0;
    assert_eq!(b.window_under_cursor(), None);

    // Unmapping the topmost — next match wins.
    b.windows.get_mut(&0x2000).unwrap().mapped = false;
    b.core.cursor_x = 75.0;
    b.core.cursor_y = 75.0;
    assert_eq!(b.window_under_cursor(), Some(0x1000));
}

/// `window_under_cursor` descends into mapped sub-windows so the
/// returned xid is the deepest match. xfwm4 attaches resize-edge
/// cursors to frame sub-windows; without descent the cursor walk
/// stops at the (cursor=None) frame top-level and the resize
/// sprites never become effective on hover. Pinned: top-edge
/// child wins when pointer is in the edge band; the frame
/// top-level wins in the interior; topmost sibling wins on
/// overlap; unmapped sub-windows are skipped (parent wins).
#[test]
fn window_under_cursor_descends_into_subwindow_tree() {
    let mut b = KmsBackend::for_tests();
    // Frame top-level at (100,100, 800x600), no cursor.
    b.windows.insert(
        0x1000,
        crate::kms::render::backend::WindowGeometry {
            border_width: 0,
            border_pixel: None,
            border_pixmap: None,
            x: 100,
            y: 100,
            width: 800,
            height: 600,
            depth: 24,
            mapped: true,
            viewable: true,
            parent: None,
            stack_rank: 0,
            bg_pixel: None,
            bg_pixmap: None,
            cursor: None,
        },
    );
    b.core.top_level_order.push(0x1000);
    // Top-edge resize sub-window at parent-local (0,0, 800x10),
    // i.e. screen (100,100, 800x10). Has its own resize cursor.
    b.windows.insert(
        0x1001,
        crate::kms::render::backend::WindowGeometry {
            border_width: 0,
            border_pixel: None,
            border_pixmap: None,
            x: 0,
            y: 0,
            width: 800,
            height: 10,
            depth: 24,
            mapped: true,
            viewable: true,
            parent: Some(0x1000),
            stack_rank: 0,
            bg_pixel: None,
            bg_pixmap: None,
            cursor: Some(0xdead_0001),
        },
    );
    // Bottom-edge resize sub-window at parent-local (0,590, 800x10),
    // screen (100,690, 800x10). Different cursor.
    b.windows.insert(
        0x1002,
        crate::kms::render::backend::WindowGeometry {
            border_width: 0,
            border_pixel: None,
            border_pixmap: None,
            x: 0,
            y: 590,
            width: 800,
            height: 10,
            depth: 24,
            mapped: true,
            viewable: true,
            parent: Some(0x1000),
            stack_rank: 1,
            bg_pixel: None,
            bg_pixmap: None,
            cursor: Some(0xdead_0002),
        },
    );

    // Cursor in the top-edge band: deepest hit is the top sub-window.
    b.core.cursor_x = 150.0;
    b.core.cursor_y = 105.0;
    assert_eq!(b.window_under_cursor(), Some(0x1001));

    // Cursor in the bottom-edge band: bottom sub-window.
    b.core.cursor_x = 150.0;
    b.core.cursor_y = 695.0;
    assert_eq!(b.window_under_cursor(), Some(0x1002));

    // Cursor in the frame interior (not in any edge band): the
    // frame top-level itself.
    b.core.cursor_x = 400.0;
    b.core.cursor_y = 300.0;
    assert_eq!(b.window_under_cursor(), Some(0x1000));

    // Overlap test — add a second top-edge child at the same
    // location with higher stack_rank; topmost wins.
    b.windows.insert(
        0x1003,
        crate::kms::render::backend::WindowGeometry {
            border_width: 0,
            border_pixel: None,
            border_pixmap: None,
            x: 0,
            y: 0,
            width: 800,
            height: 10,
            depth: 24,
            mapped: true,
            viewable: true,
            parent: Some(0x1000),
            stack_rank: 99,
            bg_pixel: None,
            bg_pixmap: None,
            cursor: Some(0xdead_0003),
        },
    );
    b.core.cursor_x = 150.0;
    b.core.cursor_y = 105.0;
    assert_eq!(b.window_under_cursor(), Some(0x1003));

    // Unmap the topmost overlap entry — sibling beneath wins.
    b.windows.get_mut(&0x1003).unwrap().mapped = false;
    assert_eq!(b.window_under_cursor(), Some(0x1001));
}

/// `on_host_input` no longer logs the `v2: on_host_input not
/// yet implemented` gap that fired before 3f.7. Key events
/// drain through xkb cooking; pointer events drain to the
/// pointer fanout.
#[test]
fn on_host_input_does_not_log_gap() {
    use yserver_core::{core_loop::HostInputEvent, server::ServerState};
    let mut b = KmsBackend::for_tests();
    let mut state = ServerState::new();
    // PointerMotion → process_pointer_absolute → no panic, no gap.
    b.on_host_input(
        &mut state,
        HostInputEvent::PointerMotion {
            origin: yserver_core::core_loop::InputOrigin::NestedHost,
            x: 10,
            y: 20,
            time: 0,
            relative: false,
            dx: 0,
            dy: 0,
            motion_delta: None,
        },
    );
    assert!(
        !b.logged_gaps.borrow().contains("on_host_input"),
        "on_host_input must not log a gap post-3f.7"
    );
    assert_eq!(b.core.cursor_x, 10.0);
    assert_eq!(b.core.cursor_y, 20.0);
}

#[test]
fn kms_pointer_authority_integrates_physical_relative_motion_in_kms() {
    use yserver_core::{
        backend::Backend,
        core_loop::{DeviceInfo, HostInputEvent, message::LibinputConfigSnapshot},
        resources::{ROOT_VISUAL, ROOT_WINDOW},
        server::ServerState,
        xinput::{InputCapabilities, InputSourceId, XiFacetKind},
    };
    use yserver_protocol::x11::{ClientId, CreateWindowRequest, ResourceId};

    const CLIENT: u32 = 7;
    const RAZER: InputSourceId = InputSourceId(0xA11);
    const HYPERX: InputSourceId = InputSourceId(0xA12);

    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    let mut grab_peer = kbd_map_client_id(&mut state, CLIENT);
    for (source_id, name) in [(RAZER, "Razer"), (HYPERX, "HyperX")] {
        backend.on_host_input(
            &mut state,
            HostInputEvent::DeviceAdded(DeviceInfo {
                source_id,
                enabled: true,
                resume_key: None,
                capabilities: InputCapabilities {
                    keyboard: source_id == RAZER,
                    pointer: true,
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
        .facet(RAZER, XiFacetKind::PointerTouch)
        .expect("Razer pointer facet");
    let razer_keyboard_id = state
        .xi_devices
        .facet(RAZER, XiFacetKind::Keyboard)
        .expect("Razer keyboard facet");
    state
        .clients
        .get_mut(&CLIENT)
        .unwrap()
        .xi2_masks
        .insert((ROOT_WINDOW, razer_keyboard_id), (1 << 13) | (1 << 14));
    state.xi2_client_versions.insert(ClientId(CLIENT), (2, 2));
    assert!(
        state
            .xi_devices
            .facet(HYPERX, XiFacetKind::PointerTouch)
            .is_some()
    );

    // Establish the master cursor through the regular warp path.
    Backend::warp_pointer_root(&mut backend, &mut state, 100, 300);
    assert_eq!(
        (backend.core.cursor_x, backend.core.cursor_y),
        (100.0, 300.0)
    );

    // An explicit slave grab floats Razer while HyperX remains attached.
    let mut grab_body = Vec::with_capacity(24);
    grab_body.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    grab_body.extend_from_slice(&0u32.to_le_bytes());
    grab_body.extend_from_slice(&0u32.to_le_bytes());
    grab_body.extend_from_slice(&razer_id.to_le_bytes());
    grab_body.extend_from_slice(&[1, 1, 0, 0]);
    grab_body.extend_from_slice(&0u16.to_le_bytes());
    grab_body.extend_from_slice(&[0u8; 2]);
    yserver_core::core_loop::process_request::process_request(
        &mut state,
        &mut backend,
        ClientId(CLIENT),
        yserver_protocol::x11::SequenceNumber(1),
        yserver_protocol::x11::RequestHeader {
            opcode: 137,
            data: 51,
            length_units: 7,
        },
        &grab_body,
        None,
    )
    .expect("XIGrabDevice on Razer pointer");
    assert_eq!(
        state.xi_devices.device(razer_id).unwrap().attached_master,
        None
    );

    let physical_motion =
        |source_id, x, y, dx, dy, motion_delta, relative| HostInputEvent::PointerMotion {
            origin: yserver_core::core_loop::InputOrigin::Physical(source_id),
            x,
            y,
            time: 1,
            relative,
            dx,
            dy,
            motion_delta,
        };

    // The input thread's x=650 is stale for the master and differs from
    // the floating slave's authoritative 100 + 500 position.
    backend.on_host_input(
        &mut state,
        physical_motion(RAZER, 650, 300, 500, 0, Some([500.0, 0.0]), true),
    );
    assert_eq!(
        (backend.core.cursor_x, backend.core.cursor_y),
        (100.0, 300.0)
    );
    let floating_position = state.floating_pointer_positions[&razer_id];
    assert_eq!(floating_position.0, 600.0);
    assert_eq!(floating_position.1, 300.0);
    assert_eq!(state.pointer_root, (100, 300));
    for _ in 0..3 {
        backend.on_host_input(
            &mut state,
            physical_motion(RAZER, 650, 300, 0, 0, Some([0.4, 0.0]), true),
        );
    }
    let floating_position = state.floating_pointer_positions[&razer_id];
    assert!((floating_position.0 - 601.2).abs() < 0.001);
    assert_eq!(state.pointer_root, (100, 300));

    // HyperX is still attached: its +1 delta advances the master by
    // exactly one pixel, regardless of the stale input-thread x value.
    backend.on_host_input(
        &mut state,
        physical_motion(HYPERX, 650, 300, 1, 0, Some([1.0, 0.0]), true),
    );
    assert_eq!(
        (backend.core.cursor_x, backend.core.cursor_y),
        (101.0, 300.0)
    );
    assert_eq!(state.pointer_root, (101, 300));

    // Fractional motion is retained across separate events; it is not
    // rounded to zero per event or treated as three integer pixels.
    backend.on_host_input(
        &mut state,
        physical_motion(HYPERX, 650, 300, 0, 0, Some([1.2, 0.0]), true),
    );
    assert!((backend.core.cursor_x - 102.2).abs() < 0.001);

    // Warps reset the KMS position directly. Relative motion starts at
    // that warped point even though the host absolute coordinate lags.
    Backend::warp_pointer_root(&mut backend, &mut state, 250, 80);
    backend.on_host_input(
        &mut state,
        physical_motion(HYPERX, 102, 300, 2, 0, Some([2.0, 0.0]), true),
    );
    assert_eq!(
        (backend.core.cursor_x, backend.core.cursor_y),
        (252.0, 80.0)
    );

    // Absolute tablet/touch-style input remains authoritative. A later
    // relative event continues from that point, not the input thread's
    // last relative accumulator.
    backend.on_host_input(
        &mut state,
        physical_motion(HYPERX, 305, 100, 205, 20, None, false),
    );
    backend.on_host_input(
        &mut state,
        physical_motion(HYPERX, 252, 80, 5, 0, Some([5.0, 0.0]), true),
    );
    assert_eq!(
        (backend.core.cursor_x, backend.core.cursor_y),
        (310.0, 100.0)
    );

    // Motion confinement clamps the KMS-integrated result. The stale
    // host coordinate remains inside the window, while the relative
    // delta attempts to move beyond its right and bottom edges.
    const CONFINED: ResourceId = ResourceId(0x0050_0001);
    const CONFINED_HOST: u32 = 0x8000_0001;
    state.resources.create_window(
        ClientId(CLIENT),
        CreateWindowRequest {
            depth: 24,
            window: CONFINED,
            parent: ROOT_WINDOW,
            x: 300,
            y: 90,
            width: 10,
            height: 20,
            border_width: 0,
            class: 1,
            visual: ROOT_VISUAL,
            ..Default::default()
        },
    );
    state.resources.window_mut(CONFINED).unwrap().host_xid =
        yserver_core::backend::WindowHandle::from_raw(CONFINED_HOST);
    assert!(state.resources.map_window(CONFINED).mapping_changed);
    backend.windows.insert(
        CONFINED_HOST,
        crate::kms::render::backend::WindowGeometry {
            border_width: 0,
            border_pixel: None,
            border_pixmap: None,
            x: 300,
            y: 90,
            width: 10,
            height: 20,
            depth: 24,
            mapped: true,
            viewable: true,
            parent: None,
            stack_rank: 0,
            bg_pixel: None,
            bg_pixmap: None,
            cursor: None,
        },
    );
    backend.core.xid_map.insert(CONFINED_HOST, CONFINED);
    backend.core.top_level_order.push(CONFINED_HOST);
    state.pointer_confine_to = CONFINED;
    let history_len = state.pointer_motion_history.len();
    backend.on_host_input(
        &mut state,
        physical_motion(HYPERX, 310, 100, 100, 100, Some([100.0, 100.0]), true),
    );
    assert_eq!(
        (backend.core.cursor_x, backend.core.cursor_y),
        (309.0, 109.0)
    );
    assert_eq!(state.pointer_root, (309, 109));
    assert_eq!(
        state
            .pointer_motion_history
            .iter()
            .skip(history_len)
            .map(|motion| (motion.root_x, motion.root_y))
            .collect::<Vec<_>>(),
        vec![(309, 109)],
        "KMS confines the integrated cursor before emitting motion"
    );

    // Float and synchronously grab Razer's keyboard facet, then send its
    // press/release through KMS. Raw slave records arrive immediately;
    // the device events remain queued with their generating source.
    let mut key_grab_body = Vec::with_capacity(24);
    key_grab_body.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    key_grab_body.extend_from_slice(&0u32.to_le_bytes());
    key_grab_body.extend_from_slice(&0u32.to_le_bytes());
    key_grab_body.extend_from_slice(&razer_keyboard_id.to_le_bytes());
    key_grab_body.extend_from_slice(&[0, 1, 0, 0]); // sync this, async paired
    key_grab_body.extend_from_slice(&0u16.to_le_bytes());
    key_grab_body.extend_from_slice(&[0u8; 2]);
    yserver_core::core_loop::process_request::process_request(
        &mut state,
        &mut backend,
        ClientId(CLIENT),
        yserver_protocol::x11::SequenceNumber(2),
        yserver_protocol::x11::RequestHeader {
            opcode: 137,
            data: 51,
            length_units: 7,
        },
        &key_grab_body,
        None,
    )
    .expect("XIGrabDevice on Razer keyboard");
    assert!(
        state
            .xi1_frozen
            .get(&razer_keyboard_id)
            .is_some_and(yserver_core::server::Xi1Freeze::frozen)
    );
    for (pressed, time) in [(true, 3), (false, 4)] {
        backend.on_host_input(
            &mut state,
            HostInputEvent::Key(yserver_core::host_x11::HostKeyEvent {
                origin: yserver_core::core_loop::InputOrigin::Physical(RAZER),
                pressed,
                keycode: 38,
                time,
                root_x: -1,
                root_y: -1,
                event_x: -1,
                event_y: -1,
                state: 0,
            }),
        );
    }
    assert!(backend.core.down_keys.is_empty());
    let (deferred_raw, deferred_keys): (Vec<_>, Vec<_>) =
        state
            .sync_pending
            .iter()
            .fold((Vec::new(), Vec::new()), |(mut raw, mut keys), pending| {
                match &pending.event {
                    yserver_core::server::QueuedInputEvent::RawKey(event) => raw.push(*event),
                    yserver_core::server::QueuedInputEvent::HostKey(event) => keys.push(*event),
                    yserver_core::server::QueuedInputEvent::HostKeyTransition(event, _) => {
                        keys.push(*event);
                    }
                    yserver_core::server::QueuedInputEvent::Xi1Routed(_) => {}
                    other => panic!("unexpected deferred input, got {other:?}"),
                }
                (raw, keys)
            });
    assert_eq!(state.sync_pending.len(), 2);
    assert_eq!(
        state
            .sync_pending
            .iter()
            .filter(|pending| matches!(
                pending.event,
                yserver_core::server::QueuedInputEvent::HostKey(_)
                    | yserver_core::server::QueuedInputEvent::HostKeyTransition(_, _)
            ))
            .count(),
        2
    );
    assert!(deferred_raw.is_empty());
    assert_eq!(deferred_keys.len(), 2);
    assert_eq!(
        deferred_keys
            .iter()
            .map(|event| (event.origin, event.keycode, event.pressed))
            .collect::<Vec<_>>(),
        vec![
            (
                yserver_core::core_loop::InputOrigin::Physical(RAZER),
                38,
                true
            ),
            (
                yserver_core::core_loop::InputOrigin::Physical(RAZER),
                38,
                false
            ),
        ]
    );
    assert_eq!(
        xi2_events(&kbd_map_drain(&mut grab_peer))
            .into_iter()
            .filter(|event| matches!(event.0, 13 | 14))
            .map(|event| (event.0, event.1, event.2, event.3))
            .collect::<Vec<_>>(),
        vec![
            (13, razer_keyboard_id, razer_keyboard_id, 38),
            (14, razer_keyboard_id, razer_keyboard_id, 38),
        ],
        "a floating keyboard emits only its source slave raw form"
    );
    assert_eq!(state.xi_devices.source(RAZER).unwrap().name, "Razer");
    assert_eq!(state.xi_devices.source(HYPERX).unwrap().name, "HyperX");
    assert_eq!(
        state.xi_devices.device(razer_id).unwrap().attached_master,
        None
    );
    assert_eq!(
        state
            .xi_devices
            .facet(HYPERX, XiFacetKind::PointerTouch)
            .and_then(|id| state.xi_devices.device(id))
            .unwrap()
            .attached_master,
        Some(yserver_core::xinput::DEVICEID_MASTER_POINTER)
    );
    assert_eq!(
        state
            .xi_devices
            .devices()
            .iter()
            .map(|device| device.id)
            .collect::<Vec<_>>(),
        vec![
            2,
            3,
            4,
            5,
            razer_keyboard_id,
            razer_id,
            state
                .xi_devices
                .facet(HYPERX, XiFacetKind::PointerTouch)
                .unwrap()
        ]
    );
    assert_eq!(
        state.xi2_pointer_grabs.keys().copied().collect::<Vec<_>>(),
        vec![razer_id]
    );
    assert_eq!(
        state.xi2_keyboard_grabs.keys().copied().collect::<Vec<_>>(),
        vec![razer_keyboard_id]
    );
    assert_eq!(
        state
            .xi_devices
            .device(razer_keyboard_id)
            .unwrap()
            .attached_master,
        None
    );
    assert_eq!(
        state
            .floating_pointer_positions
            .keys()
            .copied()
            .collect::<Vec<_>>(),
        vec![razer_id]
    );
    let floating_position = state.floating_pointer_positions[&razer_id];
    assert!((floating_position.0 - 601.2).abs() < 0.001);
    assert_eq!(floating_position.1, 300.0);
    assert!(backend.core.pending_pointer_events.is_empty());
    assert_eq!(state.sync_pending.len(), 2);
}
