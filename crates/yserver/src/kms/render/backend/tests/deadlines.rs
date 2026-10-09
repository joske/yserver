use super::*;

/// Spec: "the first paint op produces a logged 'v2 not yet
/// implemented' gap." Verify dedup — same op logs once even
/// when called multiple times.
///
/// Stage 2c wired fill_rectangle / put_image to real engine
/// calls; against `for_tests` (no Vk) those reach the engine,
/// surface `NoVk`, and log under a different name. The dedup
/// behaviour is unchanged: each gap-name fires once per
/// session. copy_area is still a logged-gap stub (Stage 2d
/// territory).
#[test]
fn paint_stub_returns_ok_and_dedups_gap() {
    let mut b = KmsBackend::for_tests();
    // First call logs (xid is unknown → `*_unknown_xid` gap).
    assert!(b.put_image(None, 0x1234, 24, 16, 16, 0, 0, &[0; 4]).is_ok());
    // Subsequent calls also return Ok and don't crash.
    for _ in 0..5 {
        assert!(b.put_image(None, 0x1234, 24, 16, 16, 0, 0, &[0; 4]).is_ok());
        assert!(b.copy_area(None, 0x1234, 0x5678, 0, 0, 0, 0, 4, 4).is_ok());
        assert!(b.fill_rectangle(None, 0x1234, 0, 0, 0, 4, 4).is_ok());
    }
    let logged = b.logged_gaps.borrow();
    // Unknown-xid path for the wired ops; all three log the
    // `_unknown_xid` variant since the test xids aren't in
    // the store fixture.
    assert!(logged.contains("put_image_unknown_xid"));
    assert!(logged.contains("fill_rectangle_unknown_xid"));
    assert!(logged.contains("copy_area_unknown_xid"));
}

#[test]
fn pending_present_completion_sets_poll_deadline() {
    let mut b = KmsBackend::for_tests();
    b.scene.scene_structure_dirty = false;
    assert!(b.next_wakeup().is_none());

    b.enqueue_present_completion(
        yserver_core::backend::CompletedPresentEvent {
            client_id: yserver_protocol::x11::ClientId(0),
            serial: 1,
            host_xid: 0x1000,
            dst_host_xid: 0x1001,
            options: 0,
            present_id: 0,
            window_generation: 0,
            crtc_id: 0,
            crtc_epoch: 0,
            msc_offset: 0,
            completion_clock: None,
            wake: yserver_core::backend::PresentWake::Pixmap { idle_fence_xid: 0 },
            completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
            emit_idle: true,
        },
        0x1001,
    );

    let deadline = b
        .next_wakeup()
        .expect("pending PRESENT completion must wake polling fallback");
    assert!(
        deadline <= std::time::Instant::now() + std::time::Duration::from_millis(2),
        "pending PRESENT deadline should be near-term"
    );
}

/// Regression (xfce "submenu painted but not shown until you
/// move"): a client painting a mapped, scene-participating window
/// AFTER its map-compose already cleared `scene_structure_dirty`
/// must still arm a compose. Content (presentation) damage — not
/// only structural map/unmap/restack — has to wake the present
/// loop, otherwise the paint is stranded until an unrelated event
/// (pointer motion, a keypress) happens to wake it. The submenu
/// maps (a compose runs against the still-empty backing), GTK then
/// paints it, and nothing re-arms a compose → the fully-rendered
/// menu sits in its backing, off-screen, until the loop is poked.
#[test]
fn presentation_damage_arms_scene_compose() {
    let mut b = KmsBackend::for_tests();
    let w_id = seed_window(&mut b, 0x100, None, 0, 0);

    // Model the map-compose having already run and drained: clean
    // scene, no pending damage → the loop is legitimately parked.
    if let Some(snap) = b.store.peek_presentation_damage(w_id) {
        b.store.ack_presentation_damage(snap);
    }
    b.scene.scene_structure_dirty = false;
    assert!(
        b.next_wakeup().is_none(),
        "precondition: a quiescent scene with no pending damage must park the loop",
    );

    // The client now paints the visible window (the submenu backing).
    b.store.damage(
        w_id,
        ash::vk::Rect2D {
            offset: ash::vk::Offset2D::default(),
            extent: ash::vk::Extent2D {
                width: 8,
                height: 8,
            },
        },
    );

    let wake = b.next_wakeup().expect(
        "content paint on a scene-participating window must arm a compose, \
             not leave the present loop parked",
    );
    assert!(
        wake <= std::time::Instant::now(),
        "pending presentation damage must arm an immediate compose",
    );
}

#[test]
fn next_wakeup_suppresses_scene_deadline_when_scanout_disallowed() {
    use crate::vt::state::VtState;

    let mut b = KmsBackend::for_tests();
    b.scene.scene_structure_dirty = true;
    assert!(
        b.next_wakeup()
            .is_some_and(|wake| wake <= std::time::Instant::now()),
        "dirty scene should wake immediately while scanout is allowed",
    );

    b.vt_state = VtState::Suspended;
    assert!(
        b.next_wakeup().is_none(),
        "dirty scene must not busy-wake while scanout is disallowed",
    );
}

/// #177: with the display dark — DPMS off, or the VT handed away — the
/// paint frame clients keep drawing into must still close on its 16 ms
/// timeout, and `next_wakeup` must still schedule that close. Closing a
/// frame submits paint work on the render queue only; the compose tick
/// (scene compose, page-flips, cursor) stays gated. Before the fix both
/// gates returned ahead of the timeout close, so the frame stayed open
/// until the 1024-pin ceiling forced it shut and freed everything it
/// pinned at once.
fn assert_frame_timeout_serviced_while_dark(
    darken: fn(&mut KmsBackend),
    undo: fn(&mut KmsBackend),
    what: &str,
) {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    // init_root_storage's fill leaves the construction frame open.
    if b.frame_builder_is_open_for_tests() {
        b.engine_close_open_frame_for_timeout_for_tests()
            .expect("close construction frame");
    }

    let pix = b.create_pixmap(None, 32, 4, 4).expect("pixmap");
    b.fill_rectangle(None, pix.as_raw(), 0xFF00_00FF, 0, 0, 4, 4)
        .expect("fill_rectangle");
    assert!(
        b.frame_builder_is_open_for_tests(),
        "fixture sanity: fill_rectangle records into an open frame"
    );

    darken(&mut b);
    // A compose is owed; only the dark gate may keep the tick from it.
    b.scene.scene_structure_dirty = true;

    let deadline = b
        .engine
        .open_frame_timeout_deadline()
        .expect("open frame has a timeout deadline");
    let wake = b
        .next_wakeup()
        .unwrap_or_else(|| panic!("{what}: next_wakeup must schedule the frame timeout"));
    assert!(
        wake <= deadline,
        "{what}: next_wakeup {wake:?} must not sleep past the frame timeout {deadline:?}"
    );

    let now = std::time::Instant::now();
    if deadline > now {
        std::thread::sleep(deadline - now + std::time::Duration::from_millis(2));
    }
    // Baseline after draining the construction-frame close.
    b.drain_paint_submit_telemetry();
    let frame_id_before = b.telemetry.frame_id();
    let compose_closes_before = b
        .telemetry
        .lifetime
        .frame_builder_close_reason_legacy_sc_compose;
    let timeout_closes_before = b.telemetry.lifetime.frame_builder_close_reason_timeout;
    let flushes_before = b.telemetry.lifetime.submit_group_flushes;

    b.tick_maybe_composite_for_tests();

    assert!(
        !b.frame_builder_is_open_for_tests(),
        "{what}: the paint frame must close on its timeout"
    );
    assert_eq!(
        b.telemetry.lifetime.frame_builder_close_reason_timeout,
        timeout_closes_before + 1,
        "{what}: the close is a timeout close, drained into telemetry"
    );
    assert!(
        b.telemetry.lifetime.submit_group_flushes > flushes_before,
        "{what}: the frame's submit is drained into telemetry"
    );
    assert_eq!(
        b.telemetry.frame_id(),
        frame_id_before,
        "{what}: no compose tick while dark"
    );
    assert_eq!(
        b.telemetry
            .lifetime
            .frame_builder_close_reason_legacy_sc_compose,
        compose_closes_before,
        "{what}: no compose-boundary close while dark"
    );
    assert!(
        b.next_wakeup().is_none(),
        "{what}: nothing left to wake for once the frame closed"
    );

    // Control: with the display back, the same tick enters the compose
    // path, which closes a fresh frame at the compose boundary. So the
    // no-compose assertions above are the dark gate, not the fixture.
    b.fill_rectangle(None, pix.as_raw(), 0xFF00_FF00, 0, 0, 4, 4)
        .expect("fill_rectangle");
    undo(&mut b);
    b.tick_maybe_composite_for_tests();
    assert_ne!(
        b.telemetry.frame_id(),
        frame_id_before,
        "{what}: control — the compose tick runs once the display is back"
    );
    assert_eq!(
        b.telemetry
            .lifetime
            .frame_builder_close_reason_legacy_sc_compose,
        compose_closes_before + 1,
        "{what}: control — the compose boundary closes the frame"
    );
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn frame_timeout_closes_and_is_scheduled_with_dpms_off() {
    assert_frame_timeout_serviced_while_dark(
        |b| b.kms_outputs_active = false,
        |b| b.kms_outputs_active = true,
        "DPMS off",
    );
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn frame_timeout_closes_and_is_scheduled_with_vt_away() {
    use crate::vt::state::VtState;
    assert_frame_timeout_serviced_while_dark(
        |b| b.vt_state = VtState::Suspended,
        |b| b.vt_state = VtState::Active,
        "VT away",
    );
}

#[test]
fn present_poll_deadline_survives_outputs_off() {
    let mut b = KmsBackend::for_tests();
    b.kms_outputs_active = false;
    b.scene.scene_structure_dirty = true;

    b.enqueue_present_completion(
        yserver_core::backend::CompletedPresentEvent {
            client_id: yserver_protocol::x11::ClientId(0),
            serial: 1,
            host_xid: 0x1000,
            dst_host_xid: 0x1001,
            options: 0,
            present_id: 0,
            window_generation: 0,
            crtc_id: 0,
            crtc_epoch: 0,
            msc_offset: 0,
            completion_clock: None,
            wake: yserver_core::backend::PresentWake::Pixmap { idle_fence_xid: 0 },
            completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
            emit_idle: true,
        },
        0x1001,
    );

    let deadline = b
        .next_wakeup()
        .expect("present poll deadline must survive DPMS-off gating");
    assert!(
        deadline <= std::time::Instant::now() + std::time::Duration::from_millis(2),
        "pending PRESENT deadline should still be near-term with outputs off",
    );
}

#[test]
fn display_hotplug_arms_rescan_deadline_and_next_wakeup() {
    let mut b = KmsBackend::for_tests();
    let now = std::time::Instant::now();
    b.hotplug_rescan_deadline = Some(now + std::time::Duration::from_millis(150));
    let wake = b
        .next_wakeup()
        .expect("an armed hotplug rescan deadline must wake polling");
    assert!(
        wake <= now + std::time::Duration::from_millis(160),
        "hotplug rescan deadline should be near-term"
    );
}

#[test]
fn poll_deferred_input_clears_expired_rescan_deadline() {
    let mut b = KmsBackend::for_tests();
    let mut state = ServerState::new();
    b.hotplug_rescan_deadline =
        Some(std::time::Instant::now() - std::time::Duration::from_millis(1));
    b.poll_deferred_input(&mut state);
    assert!(
        b.hotplug_rescan_deadline.is_none(),
        "an elapsed hotplug rescan deadline must be cleared so the loop can idle",
    );
}
