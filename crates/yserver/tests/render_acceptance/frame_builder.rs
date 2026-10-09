use super::*;

/// Stage 3f.15: PolySegment with N segments produces ONE paint
/// submit, not N. v2 used to call `engine.fill_rect` once per
/// Bresenham-output rect inside `fill_solid_rects`; the batch entry
/// point added in 3f.15 records every rect into a single
/// `cmd_clear_attachments` call. fvwm3 drag stutter + caja apparent
/// hangs both traced back to PolySegment fan-out → many tiny
/// per-segment Vk submits. This test drives 8 segments through the
/// Backend surface and asserts the lifetime paint_submits delta is
/// exactly 1.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn poly_segment_coalesces_to_one_submit() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let xid = b.create_pixmap(None, 32, 32, 32).unwrap().as_raw();

    // Snapshot lifetime counters before the stroke op.
    let before_paint = b.telemetry().lifetime.paint_submits;
    let before_q = b.telemetry().lifetime.queue_submit2;

    // Build 8 disjoint diagonal-ish segments. Each segment is
    // (x1, y1, x2, y2) as four i16's LE = 8 bytes. Bresenham
    // produces ~6-8 1×1 rects per segment, so the call passes
    // ~50 rects through `fill_solid_rects`. Pre-3f.15 this would
    // be ~50 paint_submits; post-3f.15 the count must be 1.
    let mut wire = Vec::with_capacity(8 * 8);
    let segs: [(i16, i16, i16, i16); 8] = [
        (0, 0, 6, 6),
        (8, 0, 14, 6),
        (16, 0, 22, 6),
        (24, 0, 30, 6),
        (0, 8, 6, 14),
        (8, 8, 14, 14),
        (16, 8, 22, 14),
        (24, 8, 30, 14),
    ];
    for (x1, y1, x2, y2) in segs {
        wire.extend_from_slice(&x1.to_le_bytes());
        wire.extend_from_slice(&y1.to_le_bytes());
        wire.extend_from_slice(&x2.to_le_bytes());
        wire.extend_from_slice(&y2.to_le_bytes());
    }
    b.poly_segment(None, xid, 0xFFFF_FFFF, &wire)
        .expect("poly_segment");

    let after_paint = b.telemetry().lifetime.paint_submits;
    let after_q = b.telemetry().lifetime.queue_submit2;
    assert_eq!(
        after_paint - before_paint,
        1,
        "PolySegment with 8 segments must coalesce to one paint submit (before={before_paint}, after={after_paint})",
    );
    assert_eq!(
        after_q - before_q,
        1,
        "queue_submit2 should also tick by exactly one for the batch",
    );
}

/// Phase B.1 Task 15 acceptance scaffold: with the `FrameBuilder` gate
/// flipped ON, a `render_composite_glyphs` call that interns N unique
/// glyphs should NOT submit per-glyph + final-draw CBs. Instead, the
/// engine should record all of them in the open frame and defer the
/// actual `vkQueueSubmit2` until a close trigger fires (M2 via the
/// next non-ported paint op, M3 via `maybe_composite`, timeout,
/// `sync_wait`, or shutdown).
///
/// The load-bearing assertion here is the DISPATCH ROUTING invariant:
/// after the `composite_glyphs` call returns,
/// `frame_builder_is_open_for_tests` must be true and
/// `frame_seq` must NOT have advanced (no close happened yet).
///
/// The full "exactly ONE `vkQueueSubmit2` for N uploads + 1 draw"
/// quantitative target is covered by Task 23's mixed-sequence smoke,
/// which drives a real M3 close via the scene-compose loop. TODO
/// (Task 23): extend this test once `tick_maybe_composite_for_tests`
/// has a scene set up to actually fire M3 (today the scene is empty
/// and `maybe_composite` early-returns without closing the frame).
#[test]
#[ignore = "needs live Vulkan ICD"]
#[allow(clippy::similar_names)]
fn frame_builder_composite_glyphs_one_submit() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // Drain any setup CBs so we start from a clean baseline.
    b.engine_flush_submit_group_for_tests()
        .expect("setup drain");

    // Build a small dst pixmap + SolidFill source + glyphset with one
    // 4×4 A8 glyph, mirroring the structure used by
    // `composite_glyphs_clip_intersects_picture`.
    let dst_pix = b.create_pixmap(None, 32, 8, 4).expect("create_pixmap");
    let dst_xid = dst_pix.as_raw();
    let src_pic = b
        .render_create_solid_fill(None, [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF])
        .expect("solid_fill")
        .expect("Some(PictureHandle)");
    let dst_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(dst_pix), 0, 0, &[])
        .expect("render_create_picture")
        .expect("Some(PictureHandle)");
    let gs = b
        .render_create_glyphset(None, yserver_protocol::x11::RENDER_FMT_A8)
        .expect("glyphset")
        .expect("Some");

    let mut add_body: Vec<u8> = Vec::new();
    add_body.extend_from_slice(&1_u32.to_le_bytes()); // n
    add_body.extend_from_slice(&1_u32.to_le_bytes()); // id = 1
    add_body.extend_from_slice(&u16::to_le_bytes(4)); // width
    add_body.extend_from_slice(&u16::to_le_bytes(4)); // height
    add_body.extend_from_slice(&i16::to_le_bytes(0)); // x bearing
    add_body.extend_from_slice(&i16::to_le_bytes(0)); // y bearing
    add_body.extend_from_slice(&i16::to_le_bytes(4)); // x_off
    add_body.extend_from_slice(&i16::to_le_bytes(0)); // y_off
    add_body.extend_from_slice(&[0xFFu8; 16]); // pixels: 4×4 all opaque
    b.render_add_glyphs(None, gs.as_raw(), &add_body)
        .expect("add_glyphs");

    // Drain again to drop the cow zero-fill / glyphset side-effects
    // that may have lingered before we flipped the gate on.
    b.engine_flush_submit_group_for_tests()
        .expect("post-setup drain");

    let frame_seq_before = b.engine_frame_seq_for_tests();

    // CompositeGlyphs8 items: one element with count=2 glyphs id=1.
    let mut items: Vec<u8> = Vec::new();
    items.extend_from_slice(&[2u8, 0, 0, 0]); // count + pad
    items.extend_from_slice(&i16::to_le_bytes(0)); // dx
    items.extend_from_slice(&i16::to_le_bytes(0)); // dy
    items.extend_from_slice(&[1u8, 1, 0, 0]); // 2 ids + pad

    b.render_composite_glyphs(
        None,
        23, // CompositeGlyphs8
        3,  // Over
        src_pic.as_raw(),
        dst_pic.as_raw(),
        0,
        gs.as_raw(),
        0,
        0,
        &items,
        0,
        0,
    )
    .expect("render_composite_glyphs");

    // Dispatch-routing invariant: with the gate flipped ON, the engine
    // routed through composite_glyphs_via_frame_builder, opened a
    // frame, and deferred its submits. No close has fired yet, so
    // frame_seq must not have advanced.
    assert!(
        b.frame_builder_is_open_for_tests(),
        "frame builder must be open after composite_glyphs with gate ON"
    );
    assert_eq!(
        b.engine_frame_seq_for_tests(),
        frame_seq_before,
        "no close should have fired yet (deferred submission)"
    );

    // Sanity: dst_xid is alive (i.e. we didn't crash mid-paint).
    assert!(
        b.store_drawable_exists_for_tests(dst_xid),
        "dst pixmap must still exist after the deferred-submit cycle"
    );

    // TODO Task 23: drive a real M3 close via `scene.tick` and then
    // assert `engine_frame_seq_for_tests() - frame_seq_before == 1`
    // plus a delta of exactly one `vkQueueSubmit2` for the frame's
    // (N uploads + 1 draw) CB. Today's `tick_maybe_composite_for_tests`
    // early-returns without firing M3 because the scene's dirty bit
    // isn't set in this minimal test fixture.
}

/// Phase B.1 Task 22: forced submit failure rolls back overlays.
///
/// SCAFFOLDED — needs full test-side glyph fabrication + helpers
/// (`composite_glyphs_for_tests`, `synth_n_unique_glyphs`, `force_next_submit_failure`,
/// `drawable_current_layout_for_tests`, `drawable_last_render_ticket_for_tests`,
/// `renderer_failed_for_tests`, `glyph_atlas_lookup_for_tests`).
///
/// Intent: trip the close-failure path inside `RenderEngine::close_open_frame`
/// (via Phase A's `force_next_submit_failure_for_integration_tests` latch),
/// then assert:
/// - `renderer_failed` is set on the platform.
/// - dst drawable's `last_render_ticket` restored to pre-frame value.
/// - dst drawable's `storage.current_layout` restored to pre-frame value.
/// - The atlas cache does NOT contain the glyph keys we would have inserted
///   (`pending_glyph_inserts` dropped on failure).
///
/// The structural correctness is verified by spec review of Task 12's
/// 4 error-path rollbacks + Task 15's first-touch overlay snapshots.
/// This integration test will exercise it end-to-end once the test
/// infrastructure catches up.
#[test]
#[ignore = "scaffold — needs test-side glyph fabrication + helpers"]
fn frame_builder_renderer_failed_on_submit_failure() {
    // TODO: implement once composite_glyphs_for_tests is fully wired.
}

/// Phase B.1 Task 23: realistic ordering produces exactly the expected
/// sequence of submits.
///
/// SCAFFOLDED — needs `composite_glyphs_for_tests`, `synth_n_unique_glyphs`,
/// `fill_rect_for_tests`, `platform_queue_submit2_count_for_tests`,
/// `frame_builder_is_open_for_tests` (last one exists from Task 15).
///
/// Intent: exercise the M2 close-on-non-ported-paint path:
/// 1. `fill_rect` (non-ported) → `SubmitGroup` cap=1 → 1 submit, no frame.
/// 2. `composite_glyphs` (ported) → opens the frame, no submit yet.
/// 3. `fill_rect` again → M2 closes the frame (1 submit) + `fill_rect` submits (1 submit) = 2.
///
/// Asserts the submit count delta sequence: 0 → 1 → 1 → 3.
///
/// The structural correctness is verified by spec review of Task 14's
/// M2 wiring at 10 entry points + Task 13's M3 wiring.
#[test]
#[ignore = "scaffold — needs test-side glyph + fill_rect helpers"]
fn frame_builder_mixed_sequence_smoke() {
    // TODO: implement once composite_glyphs_for_tests is fully wired.
}

/// Phase B.2 Task 3 (Mechanism 2 watermark): every descriptor
/// acquisition during an open frame tags the active descriptor pool
/// with the frame's captured `frame_generation`; an acquisition with
/// no frame open bumps `acquire_generation` and uses the new value.
///
/// Drives the engine directly via the
/// `engine_*_for_tests` / `descriptor_pool_ring_*_for_tests` test
/// helpers added in this task — no real paint op required. The
/// scenario:
///   1. Seed `acquire_generation = 10`.
///   2. Open a frame → bumps to 11, captures `frame_generation = 11`.
///   3. Two acquires while the frame is open both tag the pool with 11.
///   4. Close the frame.
///   5. One more acquire (no frame open) bumps `acquire_generation`
///      to 12 and tags the pool with 12.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn acquire_descriptor_uses_frame_generation_when_open() {
    let mut be = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // Construction (init_root_storage's fill) leaves a frame open
    // since B.3 ported fill_rect to the frame builder; close it or
    // open_frame_for_paint_for_tests trips its "frame already open"
    // debug_assert.
    if be.frame_builder_is_open_for_tests() {
        be.engine_close_open_frame_for_timeout_for_tests()
            .expect("close construction frame");
    }

    // (1) Seed a known baseline so the assertions below are
    //     deterministic and don't depend on test ordering.
    be.engine_acquire_generation_set_for_tests(10);

    // (2) Open a frame end-to-end (acquire the platform's submit-group
    //     ticket + drive the engine's open_for_paint). The engine bumps
    //     `acquire_generation` once and stamps the value as the frame's
    //     `frame_generation`.
    be.engine_open_frame_for_paint_for_tests()
        .expect("engine_open_frame_for_paint_for_tests");
    let frame_gen = be
        .engine_open_frame_generation_for_tests()
        .expect("frame is open");
    assert_eq!(
        frame_gen, 11,
        "open_for_paint must bump acquire_generation (10 -> 11) and capture it"
    );

    // Build a transient layout for the acquires below.
    let layout = be
        .engine_create_test_descriptor_set_layout_for_tests()
        .expect("create_descriptor_set_layout");

    // (3) Two acquires while the frame is open. Both must tag the
    //     active pool with the same captured frame_generation (11).
    let _ds1 = be
        .engine_acquire_descriptor_set_for_frame_or_op_for_tests(layout)
        .expect("acquire #1");
    let _ds2 = be
        .engine_acquire_descriptor_set_for_frame_or_op_for_tests(layout)
        .expect("acquire #2");
    assert_eq!(
        be.descriptor_pool_ring_high_water_generation_for_tests(),
        frame_gen,
        "both acquires must tag the descriptor pool with the open frame's \
         frame_generation (Phase B.2 Mechanism 2 watermark invariant)",
    );

    // (4) Close the frame.
    be.engine_close_open_frame_for_timeout_for_tests()
        .expect("close_open_frame");
    assert!(
        !be.frame_builder_is_open_for_tests(),
        "frame must be closed after close_open_frame_for_timeout"
    );

    // (5) Acquire one more without an open frame. The helper falls
    //     through to the legacy per-op fallback branch — bump
    //     acquire_generation and use the new value (12).
    let _ds3 = be
        .engine_acquire_descriptor_set_for_frame_or_op_for_tests(layout)
        .expect("acquire #3 post-close");
    assert_eq!(
        be.descriptor_pool_ring_high_water_generation_for_tests(),
        12,
        "post-close acquire (no frame open) must bump acquire_generation \
         from 11 to 12 and tag the pool with the new value",
    );

    be.engine_destroy_descriptor_set_layout_for_tests(layout);
}

/// Phase B.2 Task 9: `render_composite_via_frame_builder` returns
/// early on `rects.is_empty()` BEFORE any state mutation — including
/// before opening a frame. The function's first check is
/// `if rects.is_empty() { return Ok(stats); }`; under sub-gate=ON,
/// an empty render_composite must leave the frame builder closed.
///
/// This pins the empty-rects early-return contract; if a future task
/// accidentally moves the `is_empty` check below `flush_*` / asset
/// init / open-for-paint, this test catches it.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn frame_builder_render_composite_via_fb_opens_frame() {
    let mut be = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let dst = be
        .allocate_test_pixmap_bgra(64, 64)
        .expect("allocate_test_pixmap_bgra");

    // Close the construction frame (init_root_storage fill) — the
    // assert below checks the empty composite didn't OPEN a frame,
    // which needs a closed-frame baseline.
    if be.frame_builder_is_open_for_tests() {
        be.engine_close_open_frame_for_timeout_for_tests()
            .expect("close construction frame");
    }

    // Empty rects — the via_frame_builder body returns Ok(empty stats)
    // before opening the frame. No flush, no asset init, no open.
    let result = be.render_composite_empty_for_tests(dst);

    // (still partially-stubbed for non-empty rects) frame-builder
    // composite path. Done before assertions so any later panic
    // still leaves the global in a clean state.

    result.expect("render_composite_empty_for_tests");
    assert!(
        !be.frame_builder_is_open_for_tests(),
        "empty render_composite must NOT open a frame \
         (rects.is_empty() early return)",
    );
}

/// Phase B.2 Task 11 step 5: two sequential `render_composite` calls
/// against the same dst, under the `render_composite_via_frame_builder`
/// sub-gate. Op #1 reads dst's pre-frame layout (UNDEFINED for a fresh
/// pixmap). Op #2 must read the OVERLAY's post-op layout for dst —
/// `SHADER_READ_ONLY_OPTIMAL`, which is the layout the recorded
/// composite-close transition will leave dst at — NOT the stale
/// `storage.current_layout` (still UNDEFINED during recording).
///
/// Pitfall 5+6 / codex round 4 finding 3: the overlay update at
/// op-append must be one write per op, to the POST-op layout, and
/// `push_op_and_set_layouts` is the atomicity helper that bundles the
/// ops.push + overlay write into a single critical section.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn frame_builder_render_composite_via_fb_second_op_dst_old_layout_is_shader_read_only() {
    let mut be = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let dst = be
        .allocate_test_pixmap_bgra(64, 64)
        .expect("allocate_test_pixmap_bgra");

    // Drive two solid-fill composites into the same dst under the
    // frame-builder sub-gate. Both ops append into the same open
    // frame; neither flushes mid-call.
    let r1 = be.render_composite_for_tests(dst, [1.0, 0.0, 0.0, 1.0], 64, 64);
    let r2 = be.render_composite_for_tests(dst, [0.0, 1.0, 0.0, 1.0], 64, 64);

    // Snapshot the overlay-resolved dst_old_layout for both ops
    // BEFORE flipping the sub-gate back, because the peek walks the
    // current open frame.
    let layouts = be.frame_builder_peek_render_composite_dst_old_layouts_for_tests();

    r1.expect("first render_composite_for_tests");
    r2.expect("second render_composite_for_tests");

    assert_eq!(
        layouts.len(),
        2,
        "expected two RecordedRenderComposite ops in the open frame, got {layouts:?}",
    );
    assert_eq!(
        layouts[1],
        ash::vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        "second op-in-frame must resolve dst_old_layout via the overlay — \
         it reads SHADER_READ_ONLY_OPTIMAL (the post-op layout op #1's recorded \
         close transition will leave dst at), NOT the stale storage value",
    );
    // Specifically NOT COLOR_ATTACHMENT_OPTIMAL — that's an intermediate
    // in-CB state never observable across ops at append-time.
    assert_ne!(
        layouts[1],
        ash::vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        "COLOR_ATTACHMENT_OPTIMAL is an in-CB transient — must not surface as \
         a cross-op dst_old_layout (Pitfall 6)",
    );
}

/// Phase B.2 Task 12: two consecutive `render_composite` calls against
/// the same dst, under the `render_composite_via_frame_builder`
/// sub-gate, collapse into ONE `flush_submit_group` (and therefore ONE
/// `vkQueueSubmit2`) on frame close.
///
/// This is the close-time replay's load-bearing invariant: the frame
/// builder defers per-op submits and emits a single CB at close time
/// (`Timeout` here, forced via the test helper). The plan calls this
/// out as the headline win of Phase B.2 — render_composite submit
/// rate halves when the workload coalesces two paints in one tick.
///
/// Counter used: per-backend `telemetry.lifetime.submit_group_flushes`
/// (via `telemetry_submit_group_flushes_for_tests`). This is the
/// parallel-safe counter — process-global `vkQueueSubmit2` count
/// includes the engine's lazy `run_one_shot_op` asset-init submits
/// AND would interleave with other tests' submits in a parallel
/// test-runner. The per-backend counter only ticks when this
/// backend's `flush_submit_group` runs (the frame-builder collapse
/// target).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn frame_builder_render_composite_collapses_two_in_one_frame() {
    let mut be = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let dst = be
        .allocate_test_pixmap_bgra(128, 128)
        .expect("allocate_test_pixmap_bgra");

    // Drain any baseline flush outcomes (e.g. setup CBs / cow zero-fill
    // from pixmap allocation) so the per-backend counter snapshot is
    // taken at a clean baseline.
    be.engine_flush_submit_group_for_tests()
        .expect("setup drain");
    let pre = be.telemetry_submit_group_flushes_for_tests();

    // Two solid-fill composites into the same dst. Both ops append into
    // the same open frame (cap=1 group + sub-gate ON); neither flushes
    // mid-call.
    let r1 = be.render_composite_for_tests(dst, [1.0, 0.0, 0.0, 1.0], 128, 128);
    let r2 = be.render_composite_for_tests(dst, [0.0, 1.0, 0.0, 1.0], 128, 128);

    // Force frame close via the Timeout helper (unconditional close).
    // This runs the close-walk: emit each RecordedOp into the frame CB,
    // end + submit the CB, drain pending_group_ops → submitted, then
    // call flush_submit_group → vkQueueSubmit2 exactly once.
    let close_result = be.engine_close_open_frame_for_timeout_for_tests();

    r1.expect("first render_composite_for_tests");
    r2.expect("second render_composite_for_tests");
    close_result.expect("engine_close_open_frame_for_timeout_for_tests");

    let post = be.telemetry_submit_group_flushes_for_tests();
    let delta = post - pre;
    assert_eq!(
        delta, 1,
        "two render_composite in one frame must collapse into ONE \
         flush_submit_group / vkQueueSubmit2 on close (got delta={delta})",
    );

    // Frame must be closed after the helper returns.
    assert!(
        !be.frame_builder_is_open_for_tests(),
        "frame must be closed after engine_close_open_frame_for_timeout_for_tests",
    );

    // And the renderer must still be healthy (no submit failure).
    assert!(
        !be.platform_renderer_failed_for_tests(),
        "renderer_failed must remain false through the close-replay path",
    );
}

/// Phase B.2 Task 16: a realistic MATE-drag-like sequence — RENDER
/// paints interleaved with text into the same dst — collapses into a
/// single `vkQueueSubmit2` per frame.
///
/// Sequence:
///   1. 3× `render_composite` (solid src into dst).
///   2. `render_composite_glyphs` (one element of 2 ids into the same
///      dst picture; the wire-level entry routes through
///      `composite_glyphs_via_frame_builder` under the sub-gate).
///   3. 2× `render_composite` (solid src into dst).
///
/// Then force-close the frame via the Timeout helper and assert the
/// per-backend `submit_group_flushes` delta is exactly one. The
/// per-backend counter (not the process-global `queue_submit2_count`)
/// keeps the assertion parallel-safe across the test binary.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn frame_builder_mixed_render_and_glyphs_one_submit() {
    let mut be = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // Build a real pixmap + dst picture so the wire-level
    // `render_composite_glyphs` resolves dst through the picture map
    // while `render_composite_for_tests` looks up the same drawable
    // by its raw xid.
    let dst_pix = be.create_pixmap(None, 32, 256, 256).expect("create_pixmap");
    let dst_xid = dst_pix.as_raw();
    let src_pic = be
        .render_create_solid_fill(None, [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF])
        .expect("solid_fill")
        .expect("Some(PictureHandle)");
    let dst_pic = be
        .render_create_picture(None, AnyHandle::Pixmap(dst_pix), 0, 0, &[])
        .expect("render_create_picture")
        .expect("Some(PictureHandle)");
    let gs = be
        .render_create_glyphset(None, yserver_protocol::x11::RENDER_FMT_A8)
        .expect("glyphset")
        .expect("Some");

    // Register one 4×4 opaque-A8 glyph (id=1) on the glyphset. Mirrors
    // the existing `frame_builder_composite_glyphs_one_submit`
    // fixture shape — smallest plausible run that exercises the
    // atlas-intern → upload → text-draw path under the frame builder.
    let mut add_body: Vec<u8> = Vec::new();
    add_body.extend_from_slice(&1_u32.to_le_bytes()); // n
    add_body.extend_from_slice(&1_u32.to_le_bytes()); // id = 1
    add_body.extend_from_slice(&u16::to_le_bytes(4)); // width
    add_body.extend_from_slice(&u16::to_le_bytes(4)); // height
    add_body.extend_from_slice(&i16::to_le_bytes(0)); // x bearing
    add_body.extend_from_slice(&i16::to_le_bytes(0)); // y bearing
    add_body.extend_from_slice(&i16::to_le_bytes(4)); // x_off
    add_body.extend_from_slice(&i16::to_le_bytes(0)); // y_off
    add_body.extend_from_slice(&[0xFFu8; 16]); // 4×4 all opaque
    be.render_add_glyphs(None, gs.as_raw(), &add_body)
        .expect("add_glyphs");

    // Drain setup CBs (cow zero-fill on pixmap, glyphset side-effects)
    // BEFORE snapping the baseline.
    be.engine_flush_submit_group_for_tests()
        .expect("setup drain");
    let pre = be.telemetry_submit_group_flushes_for_tests();

    // 3× render_composite (solid src into dst).
    let r1 = be.render_composite_for_tests(dst_xid, [1.0, 0.0, 0.0, 1.0], 256, 256);
    let r2 = be.render_composite_for_tests(dst_xid, [0.0, 1.0, 0.0, 1.0], 256, 256);
    let r3 = be.render_composite_for_tests(dst_xid, [0.0, 0.0, 1.0, 1.0], 256, 256);

    // Then a CompositeGlyphs8 paint into the SAME dst (via dst_pic →
    // resolves to dst's drawable; the frame builder collapses both
    // paint shapes into the open frame).
    //
    // 4 glyph ids in one element (count=4, all id=1) — synth_4_glyphs:
    // smallest run that exercises the multi-glyph append path.
    let mut items: Vec<u8> = Vec::new();
    items.extend_from_slice(&[4u8, 0, 0, 0]); // count + pad
    items.extend_from_slice(&i16::to_le_bytes(0)); // dx
    items.extend_from_slice(&i16::to_le_bytes(0)); // dy
    items.extend_from_slice(&[1u8, 1, 1, 1]); // 4 ids
    let g = be.render_composite_glyphs(
        None,
        23, // CompositeGlyphs8
        3,  // PictOp::Over
        src_pic.as_raw(),
        dst_pic.as_raw(),
        0,
        gs.as_raw(),
        0,
        0,
        &items,
        0,
        0,
    );

    // 2 more render_composite into the same dst.
    let r4 = be.render_composite_for_tests(dst_xid, [1.0, 1.0, 0.0, 1.0], 256, 256);
    let r5 = be.render_composite_for_tests(dst_xid, [1.0, 0.0, 1.0, 1.0], 256, 256);

    // Force frame close via the Timeout helper (unconditional close).
    let close_result = be.engine_close_open_frame_for_timeout_for_tests();

    r1.expect("first render_composite_for_tests");
    r2.expect("second render_composite_for_tests");
    r3.expect("third render_composite_for_tests");
    g.expect("render_composite_glyphs");
    r4.expect("fourth render_composite_for_tests");
    r5.expect("fifth render_composite_for_tests");
    close_result.expect("engine_close_open_frame_for_timeout_for_tests");

    let post = be.telemetry_submit_group_flushes_for_tests();
    let delta = post - pre;
    assert_eq!(
        delta, 1,
        "mixed render_composite + composite_glyphs in one frame → \
         ONE flush_submit_group / vkQueueSubmit2 on close (got delta={delta})",
    );

    // Frame must be closed after the helper returns.
    assert!(
        !be.frame_builder_is_open_for_tests(),
        "frame must be closed after engine_close_open_frame_for_timeout_for_tests",
    );

    // Renderer must still be healthy.
    assert!(
        !be.platform_renderer_failed_for_tests(),
        "renderer_failed must remain false through the close-replay path",
    );
}

/// Phase B.2 Task 17: `render_fill_rectangles` routes through the
/// frame builder by delegating to `render_composite` (with
/// `ResolvedSource::Solid`). After Task 13 dropped the wrapper-level
/// M2 close, two `render_fill_rectangles` calls into the same dst
/// share one open frame and collapse into a single `vkQueueSubmit2`.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn frame_builder_render_fill_rectangles_via_frame_builder() {
    let mut be = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let dst = be
        .allocate_test_pixmap_bgra(64, 64)
        .expect("allocate_test_pixmap_bgra");

    // Drain setup CBs so the per-backend counter baseline is clean.
    be.engine_flush_submit_group_for_tests()
        .expect("setup drain");
    let pre = be.telemetry_submit_group_flushes_for_tests();

    // PictOp::Over (3) + a 3-rect run, then a 2-rect run. Both
    // routed through render_composite → frame builder.
    let rects_a = [
        yserver::kms::vk::ops::render::CompositeRect {
            src_x: 0,
            src_y: 0,
            mask_x: 0,
            mask_y: 0,
            dst_x: 0,
            dst_y: 0,
            width: 16,
            height: 16,
        },
        yserver::kms::vk::ops::render::CompositeRect {
            src_x: 0,
            src_y: 0,
            mask_x: 0,
            mask_y: 0,
            dst_x: 16,
            dst_y: 0,
            width: 16,
            height: 16,
        },
        yserver::kms::vk::ops::render::CompositeRect {
            src_x: 0,
            src_y: 0,
            mask_x: 0,
            mask_y: 0,
            dst_x: 32,
            dst_y: 0,
            width: 16,
            height: 16,
        },
    ];
    let rects_b = [
        yserver::kms::vk::ops::render::CompositeRect {
            src_x: 0,
            src_y: 0,
            mask_x: 0,
            mask_y: 0,
            dst_x: 0,
            dst_y: 16,
            width: 32,
            height: 16,
        },
        yserver::kms::vk::ops::render::CompositeRect {
            src_x: 0,
            src_y: 0,
            mask_x: 0,
            mask_y: 0,
            dst_x: 32,
            dst_y: 16,
            width: 32,
            height: 16,
        },
    ];

    let r1 = be.render_fill_rectangles_for_tests(dst, 3, [1.0, 0.0, 0.0, 1.0], &rects_a);
    let r2 = be.render_fill_rectangles_for_tests(dst, 3, [0.0, 1.0, 0.0, 1.0], &rects_b);

    let close_result = be.engine_close_open_frame_for_timeout_for_tests();

    r1.expect("first render_fill_rectangles_for_tests");
    r2.expect("second render_fill_rectangles_for_tests");
    close_result.expect("engine_close_open_frame_for_timeout_for_tests");

    let post = be.telemetry_submit_group_flushes_for_tests();
    let delta = post - pre;
    assert_eq!(
        delta, 1,
        "two render_fill_rectangles in one frame must collapse via the \
         render_composite delegate into ONE flush_submit_group (got delta={delta})",
    );

    assert!(
        !be.frame_builder_is_open_for_tests(),
        "frame must be closed after engine_close_open_frame_for_timeout_for_tests",
    );
    assert!(
        !be.platform_renderer_failed_for_tests(),
        "renderer_failed must remain false through the close-replay path",
    );
}

/// Phase B.2 Task 18: injected submit failure during close (a)
/// trips `renderer_failed`, (b) restores the drawable's pre-frame
/// `current_layout` via the overlay's `rollback_pre_submit` path.
///
/// Snapshots the dst layout BEFORE issuing the first render_composite
/// (UNDEFINED for a fresh pixmap, since storage's layout starts at
/// UNDEFINED until a real op promotes it). After the failed close,
/// the layout must be restored to that snapshot — the overlay's
/// `first_touch_drawable` captured `UNDEFINED` as the pre-frame
/// value, and rollback writes it back to storage.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn frame_builder_render_composite_renderer_failed_on_submit_failure() {
    let mut be = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let dst = be
        .allocate_test_pixmap_bgra(64, 64)
        .expect("allocate_test_pixmap_bgra");

    // Snapshot the pre-frame layout BEFORE the render_composite — for
    // a fresh pixmap this is UNDEFINED, which the overlay captures as
    // the rollback target via `first_touch_drawable`.
    let pre_layout = be.drawable_current_layout_for_tests(dst);

    // Arm the next vkQueueSubmit2 to fail.
    be.platform_force_next_submit_failure_for_tests();

    let r = be.render_composite_for_tests(dst, [1.0, 0.0, 0.0, 1.0], 64, 64);
    let close_result = be.engine_close_open_frame_for_timeout_for_tests();

    // The render_composite itself records into the frame builder
    // without submitting; it should succeed (no error visible until
    // the close-path submit fires).
    r.expect("render_composite_for_tests (records into open frame)");
    // The close-walk must surface the submit error.
    assert!(
        close_result.is_err(),
        "engine_close_open_frame_for_timeout_for_tests must propagate the injected submit failure"
    );

    assert!(
        be.platform_renderer_failed_for_tests(),
        "injected submit failure must trip renderer_failed",
    );
    assert_eq!(
        be.drawable_current_layout_for_tests(dst),
        pre_layout,
        "rollback_pre_submit must restore the drawable's pre-frame current_layout",
    );

    // Frame must be closed (failure path still drives the close).
    assert!(
        !be.frame_builder_is_open_for_tests(),
        "frame must be closed after the close-walk fails",
    );
}

/// Phase B.3 Task 1 (N8 + Pitfall 7): a frame containing only non-CopyArea
/// ops produces a `SubmittedOp` with an empty `scratch: Vec<ScratchImage>`.
///
/// Exercises the REAL close path (`close_open_frame` -> scratch walk ->
/// `SubmittedOp` push) rather than just the filter_map mechanism in isolation:
/// open a frame via a `render_composite` call (no scratch allocation), force-
/// close via the Timeout helper, then inspect the most-recent submitted op's
/// scratch len via the new accessor.
///
/// `RecordedRenderComposite` carries no `self_overlap_scratch`, so the walk
/// should yield an empty Vec — the `SubmittedOp::scratch` field is initialized
/// from `frame_scratches`, which collects only `RecordedCopyArea` self-overlap
/// scratches. With zero CopyArea ops in the frame, the resulting Vec must be
/// empty.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn b3_close_path_scratch_walk_yields_empty_for_no_copy_area_frames() {
    let mut be = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let dst = be
        .allocate_test_pixmap_bgra(64, 64)
        .expect("allocate_test_pixmap_bgra");

    // Drain any baseline flush outcomes (setup CBs / cow zero-fill from
    // pixmap allocation) so the test starts at a clean baseline.
    be.engine_flush_submit_group_for_tests()
        .expect("setup drain");

    // One render_composite call — this opens the frame and appends a
    // RecordedRenderComposite op. RenderComposite does NOT allocate any
    // self-overlap scratch (that is specific to RecordedCopyArea).
    let r = be.render_composite_for_tests(dst, [1.0, 0.0, 0.0, 1.0], 64, 64);

    // Force frame close via the Timeout helper. This runs the close-walk:
    //   1. iter_mut over open_frame.ops, std::mem::take each CopyArea's
    //      self_overlap_scratch into a local Vec<ScratchImage> (empty here).
    //   2. push a SubmittedOp with scratch = that local vec.
    //   3. flush_submit_group -> vkQueueSubmit2 once.
    let close_result = be.engine_close_open_frame_for_timeout_for_tests();

    r.expect("render_composite_for_tests");
    close_result.expect("engine_close_open_frame_for_timeout_for_tests");

    // Frame must be closed after the helper returns.
    assert!(
        !be.frame_builder_is_open_for_tests(),
        "frame must be closed after engine_close_open_frame_for_timeout_for_tests",
    );

    // The just-submitted op must have an empty scratch Vec — no CopyArea
    // ops were appended to the frame, so the close-path walk's filter_map
    // collected zero entries. This proves close_open_frame correctly threads
    // the `frame_scratches` local into `SubmittedOp::scratch` (B.3 N8).
    let scratch_len = be.engine_most_recent_submitted_op_scratch_len_for_tests();
    assert_eq!(
        scratch_len, 0,
        "frame with no CopyArea ops must produce SubmittedOp with empty scratch \
         Vec (got len={scratch_len})",
    );
}

/// Phase B.3 Task 2 (N1, N8, N9): two consecutive `copy_area` calls in the
/// same open frame produce exactly ONE `SubmittedOp` / `vkQueueSubmit2`. Before
/// B.3, each `copy_area` closed the open frame (M2), submitted its own CB, and
/// opened a fresh one — producing N submits for N calls. After B.3 the calls
/// accumulate into the already-open frame and collapse to one submit on close.
///
/// Invariants exercised:
/// - N9: `flush_render_batch` is called at entry; the frame stays open.
/// - N1: both dst and src overlays are set to `SHADER_READ_ONLY_OPTIMAL`.
/// - N8: no self-overlap scratch allocated (disjoint src/dst).
/// - M2: `close_open_frame_for_non_ported_op` is GONE — copy_area extends the
///   frame instead of closing it.
///
/// Counter: per-backend `telemetry.lifetime.submit_group_flushes` (delta == 1
/// for the one forced-close flush — parallel-safe).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn frame_builder_copy_area_collapses_two_in_one_frame() {
    let mut be = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let src = be
        .allocate_test_pixmap_bgra(64, 64)
        .expect("allocate src pixmap");
    let dst = be
        .allocate_test_pixmap_bgra(64, 64)
        .expect("allocate dst pixmap");

    // Drain any baseline flush outcomes (setup CBs from pixmap allocation)
    // so the per-backend counter snapshot is at a clean baseline.
    be.engine_flush_submit_group_for_tests()
        .expect("setup drain");
    let pre_flushes = be.telemetry_submit_group_flushes_for_tests();
    let pre_non_ported = be.telemetry_close_reason_non_ported_for_tests();

    let src_rect = ash::vk::Rect2D {
        offset: ash::vk::Offset2D { x: 0, y: 0 },
        extent: ash::vk::Extent2D {
            width: 32,
            height: 32,
        },
    };

    // Two copy_area calls — both must append into the same open frame
    // WITHOUT closing it in between (the old M2 close is gone in B.3).
    be.engine_copy_area_for_tests(src, dst, src_rect, ash::vk::Offset2D { x: 0, y: 0 })
        .expect("first copy_area");
    be.engine_copy_area_for_tests(src, dst, src_rect, ash::vk::Offset2D { x: 32, y: 0 })
        .expect("second copy_area");

    // Frame should still be open — copy_area extends, doesn't close.
    assert!(
        be.frame_builder_is_open_for_tests(),
        "open frame should survive two copy_area calls"
    );

    // Force-close via the Timeout helper (one flush = one vkQueueSubmit2).
    let close_result = be.engine_close_open_frame_for_timeout_for_tests();
    close_result.expect("engine_close_open_frame_for_timeout_for_tests");

    let post_flushes = be.telemetry_submit_group_flushes_for_tests();
    let post_non_ported = be.telemetry_close_reason_non_ported_for_tests();

    assert_eq!(
        post_flushes.saturating_sub(pre_flushes),
        1,
        "two copy_area calls must collapse to ONE flush_submit_group call (got delta={})",
        post_flushes.saturating_sub(pre_flushes),
    );
    assert_eq!(
        post_non_ported.saturating_sub(pre_non_ported),
        0,
        "copy_area must NOT fire CloseReason::NonPortedPaintOp (got delta={})",
        post_non_ported.saturating_sub(pre_non_ported),
    );

    // Frame must be closed.
    assert!(
        !be.frame_builder_is_open_for_tests(),
        "frame must be closed after engine_close_open_frame_for_timeout_for_tests",
    );
}

// ── Task 4: cow_copy_area frame-builder integration tests ─────────────

/// B.3 Task 4 acceptance gate (collapse): two consecutive `cow_copy_area`
/// calls in the same open frame produce exactly ONE `vkQueueSubmit2`
/// (one `flush_submit_group` call). Pre-B.3 each call submitted its own
/// `PendingCowBatch` CB independently.
///
/// The test creates a COW drawable via `get_overlay_window`, issues two
/// `cow_copy_area` calls, confirms the frame is still open, force-closes
/// via the timeout helper, and asserts submit_group_flushes delta == 1.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn frame_builder_cow_copy_area_collapses_two_in_one_frame() {
    let mut be = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // Allocate the COW drawable (get_overlay_window registers it at the
    // well-known COMPOSITE_OVERLAY_WINDOW xid; backend wires cow_id to it).
    be.get_overlay_window(None).expect("get_overlay_window");

    let src = be
        .allocate_test_pixmap_bgra(256, 256)
        .expect("allocate src pixmap");

    // Drain any setup CBs (zero-fills from pixmap allocation).
    be.engine_flush_submit_group_for_tests()
        .expect("setup drain");
    let pre_flushes = be.telemetry_submit_group_flushes_for_tests();

    let src_rect = ash::vk::Rect2D {
        offset: ash::vk::Offset2D { x: 0, y: 0 },
        extent: ash::vk::Extent2D {
            width: 64,
            height: 64,
        },
    };

    // Two cow_copy_area calls — both must append into the same open frame.
    be.engine_cow_copy_area_for_tests(src, src_rect, ash::vk::Offset2D { x: 0, y: 0 })
        .expect("first cow_copy_area");
    be.engine_cow_copy_area_for_tests(src, src_rect, ash::vk::Offset2D { x: 64, y: 0 })
        .expect("second cow_copy_area");

    // Frame should still be open — cow_copy_area extends, doesn't close.
    assert!(
        be.frame_builder_is_open_for_tests(),
        "open frame should survive two cow_copy_area calls"
    );

    // Force-close via the Timeout helper (one flush = one vkQueueSubmit2).
    be.engine_close_open_frame_for_timeout_for_tests()
        .expect("engine_close_open_frame_for_timeout_for_tests");

    let post_flushes = be.telemetry_submit_group_flushes_for_tests();
    assert_eq!(
        post_flushes.saturating_sub(pre_flushes),
        1,
        "two cow_copy_area calls must collapse to ONE flush_submit_group call (got delta={})",
        post_flushes.saturating_sub(pre_flushes),
    );

    // Frame must be closed.
    assert!(
        !be.frame_builder_is_open_for_tests(),
        "frame must be closed after force-close",
    );
}

/// B.3 Task 4 acceptance gate (PRESENT-completion N10): a
/// `cow_copy_area` followed by `attach_present_completion` inside
/// an open frame correctly delivers a `CompletedPresentEvent` when
/// the frame retires (the event is NOT dropped on flush-success).
///
/// Uses `attach_synthetic_present_completion_to_cow_for_tests` to
/// inject a fake completion entry without a real X PRESENT client.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn frame_builder_cow_copy_area_delivers_present_completion() {
    let mut be = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // Allocate the COW drawable.
    be.get_overlay_window(None).expect("get_overlay_window");

    let src = be
        .allocate_test_pixmap_bgra(64, 64)
        .expect("allocate src pixmap");

    // Drain setup CBs.
    be.engine_flush_submit_group_for_tests()
        .expect("setup drain");

    let src_rect = ash::vk::Rect2D {
        offset: ash::vk::Offset2D { x: 0, y: 0 },
        extent: ash::vk::Extent2D {
            width: 32,
            height: 32,
        },
    };

    // cow_copy_area opens the frame and writes to cow_id.
    be.engine_cow_copy_area_for_tests(src, src_rect, ash::vk::Offset2D::default())
        .expect("cow_copy_area");

    // Frame must be open (cow_copy_area extends it, doesn't close).
    assert!(
        be.frame_builder_is_open_for_tests(),
        "frame must be open after cow_copy_area"
    );

    // Attach a synthetic PRESENT completion (N10 predicate fires because
    // cow_id is written in the open frame).
    let synthetic_serial = 0xB3_CAFE_u32;
    let attached = be.attach_synthetic_present_completion_to_cow_for_tests(synthetic_serial);
    assert!(
        attached,
        "attach_synthetic_present_completion_to_cow_for_tests must succeed \
         (cow_id is written in the open frame)"
    );

    // Force-close the frame. The close-path drains pending_present_completions
    // into a PendingPresentBatch with the exported sync_file fd.
    be.engine_close_open_frame_for_timeout_for_tests()
        .expect("force-close");

    // The synthetic completion event must appear in the drained set.
    let events =
        drain_present_events_until(&mut be, |e| e.iter().any(|e| e.serial == synthetic_serial));
    assert!(
        events.iter().any(|e| e.serial == synthetic_serial),
        "synthetic PRESENT completion (serial=0x{synthetic_serial:x}) must be delivered \
         after frame retires; got {} events: {events:?}",
        events.len(),
    );
}

// ── Task 6: put_image frame-builder integration test ─────────────────────

/// B.3 Task 6 acceptance gate (collapse): two consecutive `put_image` calls
/// in the same open frame produce exactly ONE `flush_submit_group` call
/// (one `vkQueueSubmit2`). Pre-B.3 each call submitted its own CB
/// independently via `end_and_submit_op`.
///
/// The test uploads two non-overlapping 32×32 tiles into a 64×64 pixmap.
/// Both calls must stay in the open frame — no `close_open_frame_for_non_ported_op`
/// firing between them (that call was deleted from the B.3 body per N9).
///
/// Asserts:
/// - Frame is still open after both `put_image` calls.
/// - After force-close, the `submit_group_flushes` delta is exactly 1.
/// - `close_reason_non_ported` counter is unchanged (put_image no longer
///   fires CloseReason::NonPortedPaintOp).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn frame_builder_put_image_collapses_two_in_one_frame() {
    let mut be = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let dst = be
        .allocate_test_pixmap_bgra(64, 64)
        .expect("allocate dst pixmap");

    // Drain any setup CBs so the counter snapshot is at a clean baseline.
    be.engine_flush_submit_group_for_tests()
        .expect("setup drain");
    let pre_flushes = be.telemetry_submit_group_flushes_for_tests();
    let pre_non_ported = be.telemetry_close_reason_non_ported_for_tests();

    // 32×32 pixels of solid BGRA (B=0xff, G=0x00, R=0x00, A=0xff).
    let bytes: Vec<u8> = vec![0xffu8; 32 * 32 * 4];

    // First tile: top-left 32×32.
    be.engine_put_image_for_tests(
        dst,
        ash::vk::Offset2D { x: 0, y: 0 },
        ash::vk::Extent2D {
            width: 32,
            height: 32,
        },
        &bytes,
        32,
    )
    .expect("first put_image");

    // Second tile: top-right 32×32.
    be.engine_put_image_for_tests(
        dst,
        ash::vk::Offset2D { x: 32, y: 0 },
        ash::vk::Extent2D {
            width: 32,
            height: 32,
        },
        &bytes,
        32,
    )
    .expect("second put_image");

    // Both calls must have stayed in the open frame.
    assert!(
        be.frame_builder_is_open_for_tests(),
        "open frame must survive two put_image calls (not closed by non-ported M2 path)"
    );

    // Force-close via the Timeout helper (one flush = one vkQueueSubmit2).
    be.engine_close_open_frame_for_timeout_for_tests()
        .expect("force-close");

    let post_flushes = be.telemetry_submit_group_flushes_for_tests();
    let post_non_ported = be.telemetry_close_reason_non_ported_for_tests();

    assert_eq!(
        post_flushes.saturating_sub(pre_flushes),
        1,
        "two put_image calls must collapse to ONE flush_submit_group call (got delta={})",
        post_flushes.saturating_sub(pre_flushes),
    );
    assert_eq!(
        post_non_ported.saturating_sub(pre_non_ported),
        0,
        "put_image must NOT fire CloseReason::NonPortedPaintOp (got delta={})",
        post_non_ported.saturating_sub(pre_non_ported),
    );

    // Frame must be closed after the force-close.
    assert!(
        !be.frame_builder_is_open_for_tests(),
        "frame must be closed after engine_close_open_frame_for_timeout_for_tests",
    );
}

/// Phase B.3 Task 8 acceptance gate: two consecutive `fill_rect_batch`
/// calls in the same open frame produce exactly ONE `SubmittedOp` +
/// ONE `vkQueueSubmit2`. Pre-B.3 each call submitted independently.
///
/// The test also verifies `CloseReason::NonPortedPaintOp` is NOT
/// fired — fill_rect_batch now extends the open frame rather than
/// closing it via `close_open_frame_for_non_ported_op`.
#[test]
#[ignore = "requires Vk fixture — gated to acceptance harness"]
fn frame_builder_fill_rect_batch_collapses_two_in_one_frame() {
    let mut be = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let dst = be
        .allocate_test_pixmap_bgra(128, 128)
        .expect("allocate dst pixmap");

    // Drain any setup CBs so the counter snapshot is at a clean baseline.
    be.engine_flush_submit_group_for_tests()
        .expect("setup drain");
    let pre_flushes = be.telemetry_submit_group_flushes_for_tests();
    let pre_non_ported = be.telemetry_close_reason_non_ported_for_tests();

    let rects1 = [ash::vk::Rect2D {
        offset: ash::vk::Offset2D { x: 0, y: 0 },
        extent: ash::vk::Extent2D {
            width: 16,
            height: 16,
        },
    }];
    let rects2 = [ash::vk::Rect2D {
        offset: ash::vk::Offset2D { x: 32, y: 0 },
        extent: ash::vk::Extent2D {
            width: 16,
            height: 16,
        },
    }];

    // Both calls must accumulate into the same open frame.
    be.engine_fill_rect_batch_for_tests(dst, [1.0, 0.0, 0.0, 1.0], &rects1)
        .expect("first fill_rect_batch");
    be.engine_fill_rect_batch_for_tests(dst, [0.0, 1.0, 0.0, 1.0], &rects2)
        .expect("second fill_rect_batch");

    // The frame must still be open — fill_rect_batch extends, doesn't close.
    assert!(
        be.frame_builder_is_open_for_tests(),
        "open frame must survive two fill_rect_batch calls (not closed by M2 path)"
    );

    // Force-close via the Timeout helper (one flush = one vkQueueSubmit2).
    be.engine_close_open_frame_for_timeout_for_tests()
        .expect("force-close");

    let post_flushes = be.telemetry_submit_group_flushes_for_tests();
    let post_non_ported = be.telemetry_close_reason_non_ported_for_tests();

    assert_eq!(
        post_flushes.saturating_sub(pre_flushes),
        1,
        "two fill_rect_batch calls must collapse to ONE flush_submit_group call (got delta={})",
        post_flushes.saturating_sub(pre_flushes),
    );
    assert_eq!(
        post_non_ported.saturating_sub(pre_non_ported),
        0,
        "fill_rect_batch must NOT fire CloseReason::NonPortedPaintOp (got delta={})",
        post_non_ported.saturating_sub(pre_non_ported),
    );

    // Frame must be closed after the force-close.
    assert!(
        !be.frame_builder_is_open_for_tests(),
        "frame must be closed after engine_close_open_frame_for_timeout_for_tests",
    );
}

/// Phase B.3 Task 10 acceptance gate: two consecutive `logic_fill`
/// calls in the same open frame produce exactly ONE `SubmittedOp` +
/// ONE `vkQueueSubmit2`. Pre-B.3 each call submitted independently.
///
/// Also verifies `CloseReason::NonPortedPaintOp` is NOT fired —
/// `logic_fill` now extends the open frame rather than closing it via
/// `close_open_frame_for_non_ported_op`.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn frame_builder_logic_fill_collapses_two_in_one_frame() {
    let mut be = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let dst = be
        .allocate_test_pixmap_bgra(64, 64)
        .expect("allocate dst pixmap");

    // Drain any setup CBs so the counter snapshot is at a clean baseline.
    be.engine_flush_submit_group_for_tests()
        .expect("setup drain");
    let pre_flushes = be.telemetry_submit_group_flushes_for_tests();
    let pre_non_ported = be.telemetry_close_reason_non_ported_for_tests();

    let rects = [yserver::kms::cpu_types::Rectangle16 {
        x: 0,
        y: 0,
        width: 16,
        height: 16,
    }];

    // Both calls must accumulate into the same open frame.
    be.engine_logic_fill_for_tests(
        dst,
        yserver_core::backend::GcFunction::Xor,
        /* opaque_alpha */ true,
        0xFF00FF,
        &rects,
    )
    .expect("first logic_fill");
    be.engine_logic_fill_for_tests(
        dst,
        yserver_core::backend::GcFunction::And,
        true,
        0x00FF00,
        &rects,
    )
    .expect("second logic_fill");

    // The frame must still be open — logic_fill extends, doesn't close.
    assert!(
        be.frame_builder_is_open_for_tests(),
        "open frame must survive two logic_fill calls (not closed by M2 path)"
    );

    // Force-close via the Timeout helper (one flush = one vkQueueSubmit2).
    be.engine_close_open_frame_for_timeout_for_tests()
        .expect("force-close");

    let post_flushes = be.telemetry_submit_group_flushes_for_tests();
    let post_non_ported = be.telemetry_close_reason_non_ported_for_tests();

    assert_eq!(
        post_flushes.saturating_sub(pre_flushes),
        1,
        "two logic_fill calls must collapse to ONE flush_submit_group call (got delta={})",
        post_flushes.saturating_sub(pre_flushes),
    );
    assert_eq!(
        post_non_ported.saturating_sub(pre_non_ported),
        0,
        "logic_fill must NOT fire CloseReason::NonPortedPaintOp (got delta={})",
        post_non_ported.saturating_sub(pre_non_ported),
    );

    // Frame must be closed after the force-close.
    assert!(
        !be.frame_builder_is_open_for_tests(),
        "frame must be closed after engine_close_open_frame_for_timeout_for_tests",
    );
}

// ── Phase B.3 Task 14: image_text frame-builder tests ────────────────────

/// Phase B.3 Task 14 (N7) acceptance gate (collapse): two consecutive
/// `image_text` calls in the same open frame produce exactly ONE
/// `vkQueueSubmit2` (one `flush_submit_group` call).
///
/// Pre-B.3 each `image_text` call submitted its own CB batch independently.
/// After B.3 the calls accumulate into the open frame and collapse to one
/// submit on close.
///
/// Asserts:
/// - Frame is still open after both `image_text` calls.
/// - After force-close, `submit_group_flushes` delta is exactly 1.
/// - `close_reason_non_ported` counter is unchanged (image_text no longer
///   fires `CloseReason::NonPortedPaintOp`).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn frame_builder_image_text_collapses_two_in_one_frame() {
    let mut be = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let dst = be
        .allocate_test_pixmap_bgra(64, 64)
        .expect("allocate dst pixmap");

    // Drain setup CBs so the counter snapshot starts clean.
    be.engine_flush_submit_group_for_tests()
        .expect("setup drain");
    let pre_flushes = be.telemetry_submit_group_flushes_for_tests();
    let pre_non_ported = be.telemetry_close_reason_non_ported_for_tests();

    // Two calls with distinct glyphs (font 42, codepoints 0x41 + 0x42).
    // Each glyph is 4×4 pixels of solid alpha.
    be.engine_image_text_for_tests(dst, 42, [1.0, 1.0, 1.0, 1.0], &[(0x41, 0, 0, 4, 4)])
        .expect("first image_text");

    be.engine_image_text_for_tests(dst, 42, [1.0, 1.0, 1.0, 1.0], &[(0x42, 8, 0, 4, 4)])
        .expect("second image_text");

    // Both calls must have stayed in the open frame.
    assert!(
        be.frame_builder_is_open_for_tests(),
        "open frame must survive two image_text calls (not closed by M2 path)"
    );

    // Force-close (one flush = one vkQueueSubmit2).
    be.engine_close_open_frame_for_timeout_for_tests()
        .expect("force-close");

    let post_flushes = be.telemetry_submit_group_flushes_for_tests();
    let post_non_ported = be.telemetry_close_reason_non_ported_for_tests();

    assert_eq!(
        post_flushes.saturating_sub(pre_flushes),
        1,
        "two image_text calls must collapse to ONE flush_submit_group call (got delta={})",
        post_flushes.saturating_sub(pre_flushes),
    );
    assert_eq!(
        post_non_ported.saturating_sub(pre_non_ported),
        0,
        "image_text must NOT fire CloseReason::NonPortedPaintOp (got delta={})",
        post_non_ported.saturating_sub(pre_non_ported),
    );

    assert!(
        !be.frame_builder_is_open_for_tests(),
        "frame must be closed after force-close",
    );
}

/// Phase B.3 Task 14 (N7 atlas transactional discipline): force a close
/// failure AFTER an `image_text` frame and assert that:
/// - `renderer_failed` is set.
/// - The drawable's `current_layout` is restored to its pre-frame value.
/// - The frame is closed after the failure path runs.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn frame_builder_image_text_close_failure_rolls_back_atlas() {
    let mut be = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let dst = be
        .allocate_test_pixmap_bgra(64, 64)
        .expect("allocate dst pixmap");

    // Snapshot the pre-frame layout (fresh pixmap → UNDEFINED).
    let pre_layout = be.drawable_current_layout_for_tests(dst);

    // Arm the next vkQueueSubmit2 to fail.
    be.platform_force_next_submit_failure_for_tests();

    // image_text records into the open frame without submitting yet.
    let r = be.engine_image_text_for_tests(dst, 99, [1.0, 0.0, 0.0, 1.0], &[(0xAB, 4, 4, 4, 4)]);
    let close_result = be.engine_close_open_frame_for_timeout_for_tests();

    // The image_text call itself should succeed (records into the frame).
    r.expect("engine_image_text_for_tests (records into open frame)");

    // The close-walk must surface the submit error.
    assert!(
        close_result.is_err(),
        "force-close must propagate the injected submit failure"
    );
    assert!(
        be.platform_renderer_failed_for_tests(),
        "injected submit failure must trip renderer_failed",
    );
    assert_eq!(
        be.drawable_current_layout_for_tests(dst),
        pre_layout,
        "rollback_pre_submit must restore the drawable's pre-frame current_layout",
    );
    assert!(
        !be.frame_builder_is_open_for_tests(),
        "frame must be closed after the close-walk fails",
    );
}

/// Phase B.3 Task 14 (N7 LOAD-BEARING format gate): a non-BGRA8 target
/// (R8_UNORM) drops the entire run WITHOUT touching the atlas.
///
/// The gate fires BEFORE any atlas first-touch / glyph upload / op append,
/// so:
/// - `stats.glyphs_dropped == 0` (run is dropped wholesale — no glyphs
///   were individually processed).
/// - The frame must still be open after the call (no frame was opened).
/// - `submit_group_flushes` delta must be 0 (no submit happened).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn frame_builder_image_text_non_bgra8_target_drops_run() {
    use yserver::kms::render::KmsBackend;
    let mut be = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // Allocate an R8_UNORM (depth-8) pixmap — text pipeline requires
    // B8G8R8A8_UNORM; this triggers the N7 format gate.
    // NB create_pixmap is (origin, DEPTH, w, h) — this test shipped
    // with (32, 32, 8) = a depth-32 32×8 pixmap, so the gate never
    // fired and the glyph was interned (born-failing test).
    let dst_r8 = be
        .create_pixmap(None, 8, 32, 32)
        .expect("create_pixmap depth-8");
    let dst_xid = dst_r8.as_raw();

    // Close the construction frame (init_root_storage fill) so the
    // "no frame open after a format-gated drop" assert below sees
    // only this test's effect.
    if be.frame_builder_is_open_for_tests() {
        be.engine_close_open_frame_for_timeout_for_tests()
            .expect("close construction frame");
    }
    be.engine_flush_submit_group_for_tests()
        .expect("setup drain");
    let pre_flushes = be.telemetry_submit_group_flushes_for_tests();

    let (atlas_interns, _glyph_uploads, glyphs_dropped) = be
        .engine_image_text_for_tests(dst_xid, 7, [1.0, 1.0, 1.0, 1.0], &[(0x41, 0, 0, 4, 4)])
        .expect("image_text on R8 dst must return Ok (format gate drops silently)");

    // The format gate fires BEFORE any glyph processing, so glyphs_dropped
    // must be 0 (the run is dropped wholesale, not per-glyph).
    assert_eq!(
        glyphs_dropped, 0,
        "format gate drops the run before per-glyph processing; glyphs_dropped must be 0"
    );
    assert_eq!(
        atlas_interns, 0,
        "no atlas interning should occur for a non-BGRA8 target",
    );

    // No frame should have been opened (gate fires before frame open).
    assert!(
        !be.frame_builder_is_open_for_tests(),
        "no frame should be open after a format-gated drop",
    );

    let post_flushes = be.telemetry_submit_group_flushes_for_tests();
    assert_eq!(
        post_flushes.saturating_sub(pre_flushes),
        0,
        "format-gated drop must produce 0 flush calls (no submit)",
    );
}

/// Phase B.3 Task 14 (N7 + N10): open a frame with an `image_text` op,
/// attach a synthetic PRESENT completion, force-close, drain, and assert
/// that the `CompletedPresentEvent` is delivered.
///
/// Mirrors `frame_builder_cow_copy_area_delivers_present_completion`
/// but uses `image_text` as the op and `attach_synthetic_present_completion_for_tests`
/// (the generic variant that works on any drawable, not just the COW).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn frame_builder_image_text_delivers_present_completion() {
    let mut be = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let dst = be
        .allocate_test_pixmap_bgra(64, 64)
        .expect("allocate dst pixmap");

    // Drain setup CBs.
    be.engine_flush_submit_group_for_tests()
        .expect("setup drain");

    // image_text opens the frame and writes to dst.
    be.engine_image_text_for_tests(dst, 11, [0.0, 1.0, 0.0, 1.0], &[(0x41, 0, 0, 4, 4)])
        .expect("image_text");

    assert!(
        be.frame_builder_is_open_for_tests(),
        "frame must be open after image_text call"
    );

    // Attach a synthetic PRESENT completion (N10 predicate fires because
    // dst is written in the open frame).
    let synthetic_serial = 0xB3_1E77_u32;
    let attached = be.attach_synthetic_present_completion_for_tests(dst, synthetic_serial);
    assert!(
        attached,
        "attach_synthetic_present_completion_for_tests must succeed \
         (dst is written in the open frame)"
    );

    // Force-close the frame.
    be.engine_close_open_frame_for_timeout_for_tests()
        .expect("force-close");

    let events =
        drain_present_events_until(&mut be, |e| e.iter().any(|e| e.serial == synthetic_serial));
    assert!(
        events.iter().any(|e| e.serial == synthetic_serial),
        "synthetic PRESENT completion (serial=0x{synthetic_serial:x}) must be delivered \
         after frame retires; got {} events: {events:?}",
        events.len(),
    );
}

// ── Phase B.3 Task 12: render_traps_or_tris frame-builder tests ──────────

/// Phase B.3 Task 12 (N5): two \ calls with the same
/// dst collapse into ONE \ / \ call.
///
/// Both ops use a Solid source (PictOp_Src, small bbox) so neither
/// triggers a mask-scratch grow.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn frame_builder_render_traps_or_tris_collapses_two_in_one_frame() {
    let mut be = match yserver::kms::render::KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let dst = be
        .allocate_test_pixmap_bgra(128, 128)
        .expect("allocate_test_pixmap_bgra");

    be.engine_flush_submit_group_for_tests()
        .expect("setup drain");
    let pre = be.telemetry_submit_group_flushes_for_tests();
    let pre_non_ported = be.telemetry_close_reason_non_ported_for_tests();

    be.engine_render_traps_or_tris_for_tests(dst, [1.0, 0.0, 0.0, 1.0], 32, 32)
        .expect("first render_traps_or_tris");
    be.engine_render_traps_or_tris_for_tests(dst, [0.0, 1.0, 0.0, 1.0], 32, 32)
        .expect("second render_traps_or_tris");

    assert!(
        be.frame_builder_is_open_for_tests(),
        "open frame must survive two render_traps_or_tris calls",
    );

    be.engine_close_open_frame_for_timeout_for_tests()
        .expect("force-close");

    let post = be.telemetry_submit_group_flushes_for_tests();
    let post_non_ported = be.telemetry_close_reason_non_ported_for_tests();

    assert_eq!(
        post.saturating_sub(pre),
        1,
        "two render_traps_or_tris calls must collapse to ONE flush_submit_group (got delta={})",
        post.saturating_sub(pre),
    );
    assert_eq!(
        post_non_ported.saturating_sub(pre_non_ported),
        0,
        "render_traps_or_tris must NOT fire CloseReason::NonPortedPaintOp (got delta={})",
        post_non_ported.saturating_sub(pre_non_ported),
    );
    assert!(
        !be.frame_builder_is_open_for_tests(),
        "frame must be closed after force-close",
    );
    assert!(
        !be.platform_renderer_failed_for_tests(),
        "renderer_failed must remain false through the close-replay path",
    );
}

/// Phase B.3 Task 12 (N5): cross-frame mask-scratch grow test.
///
/// 3-op sequence: (small-bbox, large-bbox, large-bbox). Op 2 triggers
/// Phase 9A close-before-grow: F1 closes, grows, F2 opens. Op 3
/// appends to F2 without a further grow.
///
/// Asserts flushes delta = 2 and scratch_grow lifetime counter delta = 1.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn frame_builder_render_traps_or_tris_cross_frame_mask_grow() {
    let mut be = match yserver::kms::render::KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let dst = be
        .allocate_test_pixmap_bgra(512, 512)
        .expect("allocate_test_pixmap_bgra 512x512");

    // Construction (init_root_storage's fill) leaves a frame open
    // since B.3 ported fill_rect to the frame builder; close it so
    // F1 below contains exactly op 1.
    if be.frame_builder_is_open_for_tests() {
        be.engine_close_open_frame_for_timeout_for_tests()
            .expect("close construction frame");
    }
    be.engine_flush_submit_group_for_tests()
        .expect("setup drain");
    let pre_flushes = be.telemetry_submit_group_flushes_for_tests();
    let pre_scratch_grow = be.telemetry_close_reason_scratch_grow_for_tests();

    // Op 1: small bbox (16x16) - appends to F1, no grow.
    be.engine_render_traps_or_tris_for_tests(dst, [1.0, 0.0, 0.0, 1.0], 16, 16)
        .expect("op 1 (small)");
    assert!(
        be.frame_builder_is_open_for_tests(),
        "F1 must be open after op 1",
    );

    // Op 2: large bbox (512x512) - mask_scratch starts at 256x256, too small.
    // Phase 9A fires close-before-grow: F1 closes, mask grows, F2 opens.
    be.engine_render_traps_or_tris_for_tests(dst, [0.0, 1.0, 0.0, 1.0], 512, 512)
        .expect("op 2 (large -- triggers mask grow)");

    // Op 3: same large bbox - F2 open, no grow needed.
    be.engine_render_traps_or_tris_for_tests(dst, [0.0, 0.0, 1.0, 1.0], 512, 512)
        .expect("op 3 (large -- no grow)");

    be.engine_close_open_frame_for_timeout_for_tests()
        .expect("force-close F2");

    let post_flushes = be.telemetry_submit_group_flushes_for_tests();
    let post_scratch_grow = be.telemetry_close_reason_scratch_grow_for_tests();

    assert_eq!(
        post_flushes.saturating_sub(pre_flushes),
        2,
        "3-op sequence must produce 2 flushes (F1 + F2); got delta={}",
        post_flushes.saturating_sub(pre_flushes),
    );
    assert_eq!(
        post_scratch_grow.saturating_sub(pre_scratch_grow),
        1,
        "exactly one CloseReason::ScratchGrow must fire (got delta={})",
        post_scratch_grow.saturating_sub(pre_scratch_grow),
    );
    assert!(
        !be.frame_builder_is_open_for_tests(),
        "frame must be closed after force-close",
    );
    assert!(
        !be.platform_renderer_failed_for_tests(),
        "renderer_failed must remain false",
    );
}

/// Phase B.3 Task 12 (N5): Solid-source trap op emit completes without
/// panicking. Verifies \ fires at emit time
/// (catches the stale-solid-src replay bug from codex round-7).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn frame_builder_render_traps_or_tris_solid_source_replays_color() {
    let mut be = match yserver::kms::render::KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let dst = be
        .allocate_test_pixmap_bgra(64, 64)
        .expect("allocate_test_pixmap_bgra");

    be.engine_flush_submit_group_for_tests()
        .expect("setup drain");

    be.engine_render_traps_or_tris_for_tests(dst, [0.0, 1.0, 0.0, 1.0], 32, 32)
        .expect("solid green trap op");

    assert!(
        be.frame_builder_is_open_for_tests(),
        "frame must be open after solid trap op",
    );

    be.engine_close_open_frame_for_timeout_for_tests()
        .expect("force-close: emit must not panic on Solid-src trap op");

    assert!(
        !be.frame_builder_is_open_for_tests(),
        "frame must be closed",
    );
    assert!(
        !be.platform_renderer_failed_for_tests(),
        "renderer_failed must remain false after Solid-src trap emit",
    );
}

/// Phase B.3 Task 12 hotfix: `emit_recorded_render_traps_or_tris_into_cb`
/// previously read `inner.frame_builder.open.as_ref().expect(...)` to
/// obtain `frame_generation`, but `take_open_for_close` clears that Option
/// BEFORE the emit dispatch loop runs.  Force-closing a frame that contains
/// a `RecordedOp::RenderTrapsOrTris` panicked on yoga MATE startup as soon
/// as GTK XRender Trapezoids fired.
///
/// The fix threads `frame_generation: u64` through
/// `emit_recorded_op_into_cb` from the close path's local variable (which
/// holds the value after `take_open_for_close`).  This test opens a frame
/// via `engine_render_traps_or_tris_for_tests`, force-closes it, and asserts
/// no panic and no renderer failure.  "Didn't panic" IS the assertion.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn frame_builder_render_traps_or_tris_close_frame_does_not_panic_on_frame_generation_lookup() {
    let mut be = match yserver::kms::render::KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let dst = be
        .allocate_test_pixmap_bgra(64, 64)
        .expect("allocate_test_pixmap_bgra");

    be.engine_flush_submit_group_for_tests()
        .expect("setup drain");

    be.engine_render_traps_or_tris_for_tests(dst, [1.0, 0.0, 0.0, 1.0], 32, 32)
        .expect("render_traps_or_tris");

    assert!(
        be.frame_builder_is_open_for_tests(),
        "frame must be open after render_traps_or_tris",
    );

    // Prior to the hotfix this panicked at
    // `.expect("open frame present during emit")` inside
    // `emit_recorded_render_traps_or_tris_into_cb` because
    // `inner.frame_builder.open` is None by the time emit runs.
    be.engine_close_open_frame_for_timeout_for_tests()
        .expect("force-close must not panic: frame_generation threaded through emit dispatch");

    assert!(
        !be.frame_builder_is_open_for_tests(),
        "frame must be closed after force-close",
    );
    assert!(
        !be.platform_renderer_failed_for_tests(),
        "renderer_failed must remain false after hotfix-safe trap emit",
    );
}

/// Phase B.3 Task 12 hotfix 2: a client `FreePicture` between
/// `render_traps_or_tris` append and frame close must NOT silently
/// skip the gradient op ("missing at emit — was present at append").
///
/// Before the fix, `picture_paint_remove` destroyed the engine's
/// `GradientPicture`; emit's `inner.picture_paint.get(xid)` returned
/// `None` and the trap op was skipped — theme-gradient widgets on the
/// MATE desktop went unrendered.
///
/// After the fix, the recorded op holds a strong `Arc` clone of the
/// `GradientPicture`; `picture_paint_remove` only drops the engine's
/// copy. The emit path reads directly from the Arc, which remains
/// live until `FrameSubmittedRecord` retires after the GPU fence.
///
/// The assert is "no renderer_failed" — the frame must close cleanly
/// without the abort-on-None defensive path firing.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn frame_builder_render_traps_or_tris_gradient_picture_freed_mid_frame_still_emits() {
    let mut be = match yserver::kms::render::KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let dst = be
        .allocate_test_pixmap_bgra(64, 64)
        .expect("allocate_test_pixmap_bgra");

    be.engine_flush_submit_group_for_tests()
        .expect("setup drain");

    // Build a linear gradient LUT and record a trap op referencing it.
    let grad_xid: u32 = 0xC0_FFEE;
    be.engine_build_linear_gradient_for_tests(grad_xid)
        .expect("build_linear_gradient");

    be.engine_render_traps_or_tris_gradient_for_tests(dst, grad_xid, 32, 32)
        .expect("render_traps_or_tris with gradient src");

    assert!(
        be.frame_builder_is_open_for_tests(),
        "frame must be open after gradient render_traps_or_tris",
    );

    // Simulate the client sending FreePicture BEFORE the frame closes.
    // This is the hotfix 2 scenario: picture_paint_remove was the bug site.
    be.engine_picture_paint_remove_for_tests(grad_xid);

    // Force-close. Before the fix this would log the "missing at emit"
    // warn and silently skip the trap op. After the fix, the Arc clone
    // keeps the GradientPicture alive through emit; no skip, no panic.
    be.engine_close_open_frame_for_timeout_for_tests()
        .expect("force-close after FreePicture must not abort");

    assert!(
        !be.frame_builder_is_open_for_tests(),
        "frame must be closed after force-close",
    );
    assert!(
        !be.platform_renderer_failed_for_tests(),
        "renderer_failed must remain false — gradient emit must not abort on freed picture",
    );
}

/// Phase B.3 (post-Task 12) regression: trap-emit must drive its
/// `to_color` barrier from the recorded `dst_old_layout` (the frame
/// overlay's in-frame layout at append time) — NOT from the dst's
/// `storage.current_layout`. Under deferred recording, prior ops in
/// the SAME frame transition the dst on the GPU but storage is not
/// committed until `commit_close_success` writes the overlay back on
/// submit success. Reading storage in trap-emit declares a stale
/// `old_layout` to the implementation; the spec resolves this as
/// driver-undefined dst contents.
///
/// Symptom observed on hardware (RDNA2/RADV bee, RX580 silence): the
/// α channel of depth-32 redirected backings was zeroed in regions
/// touched by marco's SSD frame trapezoids that followed an inner-window
/// `render_composite` in the same frame — visible as "partially
/// transparent" CSD chrome on the appearance dialog. RGB survived
/// (LOAD_OP=LOAD preserves most paths), α did not.
///
/// Scenario: `fill_rect_batch` into dst (Op A — B.3 Task 8 port, follows
/// the deferred-recording contract; updates the frame overlay's in-frame
/// layout to `SHADER_READ_ONLY_OPTIMAL`, leaves storage at the pre-frame
/// value), then `render_traps_or_tris` into the same dst (Op B —
/// append-time records `dst_old_layout = SHADER_READ_ONLY_OPTIMAL` from
/// the overlay). Emit-time MUST use the recorded value, not storage.
///
/// Under validation layers, the buggy version emits a barrier from a
/// layout the GPU is no longer in and trips a VUID that routes to
/// `platform.renderer_failed`. Without validation, the assert is
/// behavioural-light (no panic, frame closes cleanly), but the test
/// still documents the regression scenario and exercises the corrected
/// `RecordedCompositeTarget` + `record_render_composite_open_with_old_layout`
/// emit path so any future revert of the fix is structurally caught.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn frame_builder_render_traps_or_tris_after_prior_dst_paint_uses_recorded_old_layout() {
    let mut be = match yserver::kms::render::KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // `init_root_storage` (called by `for_tests_with_vk`) issues
    // `fill_rect` against the root drawable, which is a B.3 Task 8
    // ported op — so it leaves an open frame on construction. Force
    // it closed + drain before flipping the frame-builder gate, which
    // debug-asserts no frame is open at toggle time.
    if be.frame_builder_is_open_for_tests() {
        be.engine_close_open_frame_for_timeout_for_tests()
            .expect("force-close init_root_storage frame");
    }

    let dst = be
        .allocate_test_pixmap_bgra(64, 64)
        .expect("allocate_test_pixmap_bgra");

    be.engine_flush_submit_group_for_tests()
        .expect("setup drain");

    // Op A: fill_rect_batch into dst. B.3 Task 8 ported op — goes
    // through frame_builder, follows the deferred-recording contract
    // (storage layout NOT mutated; commit_close_success writes the
    // overlay's post-op layout back on submit success). After this
    // call the frame overlay records dst's in-frame layout as
    // SHADER_READ_ONLY_OPTIMAL; storage.current_layout still shows
    // the pre-frame value.
    let rects = [ash::vk::Rect2D {
        offset: ash::vk::Offset2D { x: 0, y: 0 },
        extent: ash::vk::Extent2D {
            width: 32,
            height: 32,
        },
    }];
    be.engine_fill_rect_batch_for_tests(dst, [0.5, 0.5, 0.5, 1.0], &rects)
        .expect("fill_rect_batch into dst");

    // Op B: trap into the same dst. Append-time reads dst_old_layout
    // from the overlay (SHADER_READ_ONLY, set by Op A above). Emit-time
    // must emit the to_color barrier from that recorded value.
    be.engine_render_traps_or_tris_for_tests(dst, [0.0, 0.0, 1.0, 1.0], 32, 32)
        .expect("render_traps_or_tris into same dst as fill_rect_batch");

    assert!(
        be.frame_builder_is_open_for_tests(),
        "frame must remain open across fill_rect_batch + render_traps_or_tris",
    );

    be.engine_close_open_frame_for_timeout_for_tests()
        .expect("force-close must not trip validation on a stale-layout barrier");

    assert!(
        !be.frame_builder_is_open_for_tests(),
        "frame must be closed after force-close",
    );
    assert!(
        !be.platform_renderer_failed_for_tests(),
        "renderer_failed must remain false — trap-emit's to_color barrier must \
         come from the recorded dst_old_layout (frame overlay snapshot at append), \
         not from storage.current_layout (stale during deferred recording)",
    );
}
