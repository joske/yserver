use super::*;

/// Stage 5 Task 4 layer 1 telemetry primer gate: after a single
/// render_composite call the backend telemetry must reflect ≥ 1
/// descriptor_pool_creates lifetime. Without backend wiring the
/// ring's lifetime counter increments but Telemetry stays at zero.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn render_composite_bumps_pool_create_telemetry() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let dst_pix = b.create_pixmap(None, 32, 4, 4).expect("create_pixmap");
    let dst_xid = dst_pix.as_raw();
    b.fill_rectangle(None, dst_xid, 0xFF0000FF, 0, 0, 4, 4)
        .expect("pre-fill");
    let src_pic = b
        .render_create_solid_fill(None, [0xFF, 0xFF, 0, 0, 0, 0, 0xFF, 0xFF])
        .expect("solid")
        .expect("Some");
    let dst_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(dst_pix), 0, 0, &[])
        .expect("pic")
        .expect("Some");

    b.render_composite(
        None,
        3,
        src_pic.as_raw(),
        0,
        dst_pic.as_raw(),
        0,
        0,
        0,
        0,
        0,
        0,
        4,
        4,
    )
    .expect("composite");

    let t = b.telemetry();
    assert!(
        t.lifetime.descriptor_pool_creates >= 1,
        "expected ≥ 1 pool create, got {}",
        t.lifetime.descriptor_pool_creates,
    );
}

/// Stage 5 Task 4 layer 1 acceptance: N render_composite ops with
/// bounded in-flight depth must (1) bound pool creates, (2) actually
/// recycle pools (resets observed), (3) keep pool residency small.
/// Spec § 'Integration tests'.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn render_composite_pool_creates_bounded_after_warmup() {
    const N: u32 = 2000;
    // 256 sets per pool inside the ring (mirrors SETS_PER_POOL).
    const SETS_PER_POOL: u32 = 256;
    const WARMUP_SLACK: u64 = 4;
    let expected_creates_upper = u64::from(N / SETS_PER_POOL) + WARMUP_SLACK;
    let expected_resets_lower = u64::from(N / SETS_PER_POOL).saturating_sub(WARMUP_SLACK);

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let dst_pix = b.create_pixmap(None, 32, 4, 4).expect("dst pixmap");
    let dst_xid = dst_pix.as_raw();
    b.fill_rectangle(None, dst_xid, 0xFF0000FF, 0, 0, 4, 4)
        .expect("pre-fill blue");
    let src_pic = b
        .render_create_solid_fill(None, [0xFF, 0xFF, 0, 0, 0, 0, 0xFF, 0xFF])
        .expect("solid red")
        .expect("Some");
    let dst_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(dst_pix), 0, 0, &[])
        .expect("dst pic")
        .expect("Some");

    for i in 0..N {
        b.render_composite(
            None,
            3,
            src_pic.as_raw(),
            0,
            dst_pic.as_raw(),
            0,
            0,
            0,
            0,
            0,
            0,
            4,
            4,
        )
        .unwrap_or_else(|e| panic!("composite #{i} failed: {e:?}"));
        // Retire often — every 32 ops drives the ring through full
        // recycle cycles. Without retirement the ring just grows
        // InFlight pools and never resets.
        if i % 32 == 31 {
            // Force fence completion via a sync get_image, then
            // drive the retirement loop explicitly (page flips don't
            // run in the pixmap-only fixture).
            let _ = b
                .get_image_pixels_for_tests(dst_xid, 2, 0, 0, 4, 4, !0)
                .expect("get_image");
            b.for_tests_poll_retired();
        }
    }
    // Final retirement to flush any remaining in-flight ops.
    let _ = b
        .get_image_pixels_for_tests(dst_xid, 2, 0, 0, 4, 4, !0)
        .expect("final get_image");
    b.for_tests_poll_retired();

    let t = b.telemetry();
    let creates = t.lifetime.descriptor_pool_creates;
    let resets = t.lifetime.descriptor_pool_resets;
    let residency = b.descriptor_pool_ring_pool_count();

    assert!(
        creates <= expected_creates_upper,
        "creates={creates}, expected <= {expected_creates_upper} (N={N})",
    );
    assert!(
        resets >= expected_resets_lower,
        "resets={resets}, expected >= {expected_resets_lower} \
         — recycle path didn't run; pools may be leaking as InFlight",
    );
    assert!(
        residency <= 4,
        "pool_count={residency} after warm-up; expected <= 4",
    );
}

/// Stage 5 Task 4 layer 1 acceptance for the traps call site. Same
/// three-assertion shape as render_composite — landing both makes
/// the regression surface explicit since the two engine paths share
/// the ring acquire helper.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn render_traps_pool_creates_bounded_after_warmup() {
    const N: u32 = 2000;
    const SETS_PER_POOL: u32 = 256;
    const WARMUP_SLACK: u64 = 4;
    let expected_creates_upper = u64::from(N / SETS_PER_POOL) + WARMUP_SLACK;
    let expected_resets_lower = u64::from(N / SETS_PER_POOL).saturating_sub(WARMUP_SLACK);

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let dst_pix = b.create_pixmap(None, 32, 8, 8).expect("dst pixmap");
    let dst_xid = dst_pix.as_raw();
    b.fill_rectangle(None, dst_xid, 0xFF0000FF, 0, 0, 8, 8)
        .expect("pre-fill blue");
    let src_pic = b
        .render_create_solid_fill(None, [0xFF, 0xFF, 0, 0, 0, 0, 0xFF, 0xFF])
        .expect("solid red")
        .expect("Some");
    let dst_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(dst_pix), 0, 0, &[])
        .expect("dst pic")
        .expect("Some");

    // Same axis-aligned 4×4 trap used by
    // render_trapezoids_renders_filled_rect.
    let mut traps: Vec<u8> = Vec::with_capacity(40);
    let fields: [i32; 10] = [
        2 << 16,
        6 << 16,
        2 << 16,
        2 << 16,
        2 << 16,
        6 << 16,
        6 << 16,
        2 << 16,
        6 << 16,
        6 << 16,
    ];
    for v in fields {
        traps.extend_from_slice(&v.to_le_bytes());
    }

    for i in 0..N {
        b.render_trapezoids(
            None,
            3,
            src_pic.as_raw(),
            dst_pic.as_raw(),
            0,
            0,
            0,
            &traps,
            0,
            0,
        )
        .unwrap_or_else(|e| panic!("trap #{i} failed: {e:?}"));
        if i % 32 == 31 {
            let _ = b
                .get_image_pixels_for_tests(dst_xid, 2, 0, 0, 8, 8, !0)
                .expect("get_image");
            b.for_tests_poll_retired();
        }
    }
    let _ = b
        .get_image_pixels_for_tests(dst_xid, 2, 0, 0, 8, 8, !0)
        .expect("final get_image");
    b.for_tests_poll_retired();

    let t = b.telemetry();
    let creates = t.lifetime.descriptor_pool_creates;
    let resets = t.lifetime.descriptor_pool_resets;
    let residency = b.descriptor_pool_ring_pool_count();

    assert!(
        creates <= expected_creates_upper,
        "creates={creates}, expected <= {expected_creates_upper}",
    );
    assert!(
        resets >= expected_resets_lower,
        "resets={resets}, expected >= {expected_resets_lower}",
    );
    assert!(
        residency <= 4,
        "pool_count={residency} after warm-up; expected <= 4",
    );
}

/// Phase A T8 successor: the SubmitGroup must never accumulate
/// unsubmitted paint across frame closes.
///
/// History: T8 originally pinned "16 paint ops park 16 CBs; the 16th
/// append crosses cap=16 and auto-flushes". Since B.3 ported the paint
/// surface to the frame builder, paint ops coalesce into ONE frame CB
/// and `close_open_frame` ends with an unconditional
/// `flush_submit_group(FrameBuilder)` — group entries can no longer
/// grow toward the cap through the Backend surface at all (the
/// `MaxSize` auto-flush boundary itself is unit-covered in
/// `submit_group.rs` / `maybe_auto_flush_submit_group`).
///
/// Invariant pinned now: with the cap raised far above the workload
/// (16), 17 paint+close cycles still leave the group empty and closed
/// after EVERY close — the close-time flush is reason-driven, not
/// cap-driven, so no growth path exists for parked paint.
#[test]
#[ignore = "lavapipe vk"]
fn submit_group_max_size_caps_growth_at_seventeen_paint_ops() {
    use yserver_core::backend::Backend;

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let dst = b.create_pixmap(None, 32, 16, 16).expect("dst pixmap");
    let dst_xid = dst.as_raw();

    // Close the construction frame + drain setup CBs so we start from
    // an empty, closed group — BEFORE raising the cap, so the close's
    // own append flushes out under the default cap.
    if b.frame_builder_is_open_for_tests() {
        b.engine_close_open_frame_for_timeout_for_tests()
            .expect("close construction frame");
    }
    b.engine_flush_submit_group_for_tests()
        .expect("setup drain");
    assert!(
        !b.platform_submit_group_is_open_for_tests(),
        "setup drained"
    );

    // Force the cap to 16 explicitly so the test doesn't drift if
    // someone tunes the default (production runs max_size=1 during
    // B.1–B.4; see platform_open_pins_submit_group_max_size_to_one).
    b.platform_submit_group_set_max_size_for_tests(16);

    // Since B.3, fill_rectangle records into the frame builder, so a
    // bare fill never appends to the group. Each (fill + close-frame)
    // cycle submits exactly one frame CB — and the close-time
    // FrameBuilder flush must drain it immediately even though the
    // cap (16) is never reached.
    for i in 0..17u32 {
        b.fill_rectangle(None, dst_xid, i, 0, 0, 4, 4)
            .expect("fill");
        b.engine_close_open_frame_for_timeout_for_tests()
            .expect("close frame");
        assert_eq!(
            b.platform_submit_group_size_for_tests(),
            0,
            "close-time FrameBuilder flush drains the group (cycle {i})",
        );
        assert!(
            !b.platform_submit_group_is_open_for_tests(),
            "group closed after close-time flush (cycle {i})",
        );
        assert_eq!(
            b.engine_pending_group_ops_count_for_tests(),
            0,
            "parked ops graduated at close (cycle {i})",
        );
    }

    // Explicit flush on the already-drained group is a no-op.
    b.engine_flush_submit_group_for_tests()
        .expect("final flush");
    assert_eq!(b.platform_submit_group_size_for_tests(), 0);
    assert!(!b.platform_submit_group_is_open_for_tests());
}

/// Phase A T10 regression gate: renderer-failed path full rollback.
///
/// Invariants pinned:
///
/// 1. **Pending-op drop on failure.** After a `queue_submit2` failure,
///    `pending_group_ops` is cleared and the `submitted` ring is
///    unchanged (no phantom in-flight entries).
///
/// 2. **`renderer_failed` short-circuits subsequent paint ops.** After
///    failure, `engine.fill_rect` returns `RendererFailed` immediately.
///
/// 3. **No-panic on poisoned drawable state.** `store.get_by_xid(dst)`
///    must not panic after the failure.
#[test]
#[ignore = "lavapipe vk"]
fn submit_group_failure_drops_pending_ops_and_short_circuits() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let dst = b.create_pixmap(None, 32, 4, 4).expect("dst pixmap");
    let dst_xid = dst.as_raw();

    // Close the construction frame + drain setup CBs so
    // pending_group_ops and submitted start clean.
    if b.frame_builder_is_open_for_tests() {
        b.engine_close_open_frame_for_timeout_for_tests()
            .expect("close construction frame");
    }
    b.engine_flush_submit_group_for_tests().expect("drain");

    // Buffer two paint ops. Since B.3 they coalesce as recorded ops
    // in ONE open frame-builder frame (nothing parks in
    // pending_group_ops until the close replays the frame).
    b.fill_rectangle(None, dst_xid, 0xFF_00_00_00, 0, 0, 4, 4)
        .expect("fill 1");
    b.fill_rectangle(None, dst_xid, 0xFF_00_00_01, 0, 0, 4, 4)
        .expect("fill 2");
    assert!(
        b.frame_builder_is_open_for_tests(),
        "two fills buffered in an open frame before failure"
    );

    let in_flight_before = b.engine_pending_count_for_tests();

    // Scenario 1: inject failure → close the frame (its replay CB
    // appends to the group and the close-time FrameBuilder flush hits
    // the injected queue_submit2 failure) → pending_group_ops cleared,
    // submitted count unchanged.
    b.platform_force_next_submit_failure_for_tests();
    let close_result = b.engine_close_open_frame_for_timeout_for_tests();
    assert!(
        close_result.is_err(),
        "frame close must return Err on injected submit failure"
    );

    assert!(
        b.platform_renderer_failed_for_tests(),
        "renderer_failed must be set after flush failure"
    );
    assert_eq!(
        b.engine_pending_group_ops_count_for_tests(),
        0,
        "pending_group_ops must be cleared after rollback"
    );
    assert_eq!(
        b.engine_pending_count_for_tests(),
        in_flight_before,
        "submitted ring must be unchanged (no phantom entries)"
    );

    // Scenario 2: subsequent engine.fill_rect must short-circuit with
    // RendererFailed (not attempt to allocate a CB or record work).
    assert!(
        b.engine_fill_rect_is_renderer_failed_for_tests(dst_xid),
        "fill_rect must short-circuit with RendererFailed when renderer is poisoned"
    );

    // Scenario 3: store lookup must not panic on poisoned state.
    // The drawable was created before the failure; the backing
    // VkImage is still registered even though the renderer is dead.
    let _ = b.store_drawable_exists_for_tests(dst_xid);
}

/// Phase A T12 regression gate: mixed-sequence smoke test that pins the
/// flush-trigger ordering invariant across the full Phase A paint surface.
///
/// Sequence mirrors a representative MATE drag tick:
///   1. cow_copy_area (via Backend::copy_area into COW xid) — opens cow_batch
///   2. fill_rectangle on a non-COW dst — flushes cow_batch into group, parks fill CB
///   3. render_composite (SolidFill→dst picture) — parks composite CB
///   4. image_text8 (with "fixed" font) — parks glyph upload + draw CBs
///   5. get_image — SyncBoundary flush (drains group) + readback submit flush
///
/// Expected `submit_group_flushes` delta = **2**: one SyncBoundary at the
/// top of get_image (drains the buffered cow→fill→composite→glyph chain)
/// and one SyncBoundary for the readback CB itself.
///
/// Note: maybe_composite is not driven (no public wrapper available on
/// KmsBackend that ticks the scene/compose loop); the covered surface
/// is: cow_batch path, fill_rect, render_composite, glyph upload, and
/// the two get_image flush-trigger sites.
///
/// Counter used: per-backend `telemetry.lifetime.submit_group_flushes`
/// (via `telemetry_submit_group_flushes_for_tests`) — not the global
/// `queue_submit2_count`, so the assertion is parallel-safe.
#[test]
#[ignore = "lavapipe vk"]
fn submit_group_mixed_sequence_smoke_exact_submit_count() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // ── Setup ──────────────────────────────────────────────────────
    // Register the Composite Overlay Window so that copy_area to the
    // COW xid routes through engine.cow_copy_area (opens cow_batch).
    b.get_overlay_window(None).expect("get_overlay_window");
    let cow_xid = yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0;

    // Source pixmap (small: 8×8, depth 32).
    let src = b.create_pixmap(None, 32, 8, 8).expect("src pixmap");
    let src_xid = src.as_raw();

    // Destination pixmap for non-cow ops (fill, composite, image_text).
    let dst = b.create_pixmap(None, 32, 32, 32).expect("dst pixmap");
    let dst_xid = dst.as_raw();

    // Close the construction frame (init_root_storage fill) and drain
    // all setup CBs so the baseline group is clean, then capture the
    // initial flush count.
    if b.frame_builder_is_open_for_tests() {
        b.engine_close_open_frame_for_timeout_for_tests()
            .expect("close construction frame");
    }
    b.engine_flush_submit_group_for_tests()
        .expect("setup drain");
    let initial_flushes = b.telemetry_submit_group_flushes_for_tests();

    // ── Mixed sequence ────────────────────────────────────────────
    // Since B.3 the whole paint surface below records into ONE open
    // frame-builder frame; nothing parks in the group until get_image
    // closes the frame.
    // Step 1: copy_area into COW xid → cow_copy_area records into the
    // frame (opens it).
    b.copy_area(None, src_xid, cow_xid, 0, 0, 0, 0, 8, 8)
        .expect("cow copy_area");

    // Step 2: fill_rectangle on non-cow dst → records into the same
    // open frame.
    b.fill_rectangle(None, dst_xid, 0xFF_00_00_FF, 0, 0, 8, 8)
        .expect("fill_rectangle");

    // Step 3: render_composite (SolidFill src → dst picture) →
    // records into the same open frame.
    let dst_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(dst), 0, 0, &[])
        .expect("render_create_picture dst")
        .expect("Some(dst picture)");
    let src_pic = b
        .render_create_solid_fill(
            None,
            // opaque red: premul RGBA u16LE = R=0xFFFF G=0 B=0 A=0xFFFF
            [0xFF, 0xFF, 0x00, 0x00, 0x00, 0x00, 0xFF, 0xFF],
        )
        .expect("render_create_solid_fill")
        .expect("Some(src picture)");
    b.render_composite(
        None,
        1, // Src op
        src_pic.as_raw(),
        0, // no mask
        dst_pic.as_raw(),
        0,
        0,
        0,
        0,
        0,
        0,
        8,
        8,
    )
    .expect("render_composite");

    // Step 4: image_text8 — try to open the "fixed" bitmap font;
    // skip the step gracefully if fontconfig can't find it in this
    // environment (the step is exercised opportunistically).
    let font_set = if let Ok((font_handle, _metrics)) = b.open_font(None, "fixed") {
        let ds = DrawState {
            font: Some(font_handle),
            ..DrawState::default()
        };
        b.apply_draw_state(None, &ds).expect("apply_draw_state");
        // image_text8 body: 8 bytes of header (drawable+gc, unused here)
        // + x(2,LE) + y(2,LE) + text bytes.
        let mut body = vec![0u8; 12 + 1];
        body[8..10].copy_from_slice(&1i16.to_le_bytes()); // x=1
        body[10..12].copy_from_slice(&12i16.to_le_bytes()); // y=12 (below ascent)
        body[12] = b'a';
        b.image_text8(None, dst_xid, 0xFF_FF_FF_FF, 0, 1, &body)
            .expect("image_text8");
        true
    } else {
        eprintln!("T12: 'fixed' font not found; skipping image_text8 step");
        false
    };
    let _ = font_set; // suppress unused-variable warning

    // Step 5: get_image — sync barrier.
    // Internally: flush_render_batch (no-op here), then
    // close_open_frame(SyncWait) — whose close path ends with its own
    // flush_submit_group(FrameBuilder) submitting the frame CB — then
    // two SyncBoundary flush_submit_group calls:
    //   [A] SyncBoundary — drains anything still buffered
    //   [B] SyncBoundary — submits the readback CB itself
    let _ = b
        .get_image_pixels_for_tests(dst_xid, 2, 0, 0, 8, 8, !0)
        .expect("get_image");

    // ── Assertions ────────────────────────────────────────────────
    let after_flushes = b.telemetry_submit_group_flushes_for_tests();
    let delta = after_flushes - initial_flushes;
    // Exact count: 3, all inside get_image —
    //   1. close_open_frame(SyncWait)'s internal FrameBuilder flush
    //      (submits the cow→fill→composite→glyph frame CB),
    //   2. SyncBoundary [A],
    //   3. SyncBoundary [B] (readback CB).
    // Every engine flush_submit_group call queues an outcome (even a
    // 0-entry fast-path), so the counter counts calls, not entries.
    // Pre-B.3 this was 2 — there was no deferred frame to close.
    assert_eq!(
        delta, 3,
        "expected exactly 3 submit_group flushes from get_image \
         (FrameBuilder close + SyncBoundary pair); got {delta}",
    );

    // End state: group fully drained, no parked ops, renderer healthy.
    assert!(
        !b.platform_submit_group_is_open_for_tests(),
        "submit group must be closed after get_image"
    );
    assert_eq!(
        b.platform_submit_group_size_for_tests(),
        0,
        "submit group size must be 0 after get_image"
    );
    assert_eq!(
        b.engine_pending_group_ops_count_for_tests(),
        0,
        "pending_group_ops must be empty after get_image"
    );
    assert!(
        !b.platform_renderer_failed_for_tests(),
        "renderer_failed must remain false throughout"
    );
}

/// Phase B.1 Task 10 — Invariant M1: `SubmitGroup::new()` defaults
/// to `max_size=1` for the duration of the B.1–B.4 sub-phase rollout.
/// This test pins the regression so that any future accidental revert
/// of the default is caught before it reaches a review.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn platform_open_pins_submit_group_max_size_to_one() {
    let backend = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    assert_eq!(
        backend.platform_submit_group_max_size_for_tests(),
        1,
        "Phase B Invariant M1: SubmitGroup max_size must be 1 in B.1–B.4"
    );
}
