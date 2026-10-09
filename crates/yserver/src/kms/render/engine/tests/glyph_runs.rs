use super::*;

// ── #137 step 1: pin-ceiling reservation for the instance buffer ──
//
// The pre-pass / per-glyph admission rules budget prospective glyph
// *uploads* only; the instance buffer is then pinned unconditionally
// afterwards. A call whose uploads exactly fill the declared ceiling
// therefore ends the frame at `ceiling + 1` pins. These assert the
// actual pin COUNT, never "it rendered" — the off-by-one renders
// fine, so an outcome-based test would pass on the broken code.

/// `composite_glyphs_via_frame_builder`'s half of the hole
/// (`render/engine.rs`, pre-pass / single-call-overflow / per-glyph
/// admission). Ceiling of 2, two never-before-seen glyphs in ONE
/// call: pre-fix, both upload (2 pins) and the instance buffer pins
/// unconditionally after (1 more) = 3 pins against a ceiling of 2.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn composite_glyphs_pin_ceiling_reserves_instance_buffer_pin() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no Vk — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");
    let target = alloc_drawable_3a(&platform, &mut store, 0x1, 32, 32);

    {
        let inner = engine.inner.as_mut().expect("inner");
        inner.frame_builder.set_max_pinned_resources_per_frame(2);
    }

    let pixels_a = [0xFFu8; 4];
    let pixels_b = [0xFFu8; 4];
    let glyphs = [
        CompositeGlyphInput {
            gs_xid: 0x7001,
            glyph_id: 1,
            w: 2,
            h: 2,
            pixels: GlyphPixels::A8(&pixels_a),
            dst_x: 1,
            dst_y: 1,
        },
        CompositeGlyphInput {
            gs_xid: 0x7001,
            glyph_id: 2,
            w: 2,
            h: 2,
            pixels: GlyphPixels::A8(&pixels_b),
            dst_x: 10,
            dst_y: 1,
        },
    ];

    let stats = engine
        .composite_glyphs(
            &mut store,
            &mut platform,
            Dst::server_internal(target),
            3, // Over
            0, // pict_format unknown → depth heuristic
            [1.0, 1.0, 1.0, 1.0],
            &glyphs,
            None,
        )
        .expect("composite_glyphs");

    let inner = engine.inner.as_ref().expect("inner");
    let ceiling = inner.frame_builder.max_pinned_resources_per_frame();
    let open = inner
        .frame_builder
        .open
        .as_ref()
        .expect("frame stays open after composite_glyphs");
    assert!(
        open.pins.len() <= ceiling,
        "pin set must never exceed the declared ceiling: {} pins against a \
             ceiling of {} (glyphs_dropped={})",
        open.pins.len(),
        ceiling,
        stats.glyphs_dropped,
    );

    engine.drain_all(&mut platform);
}

/// `image_text`'s identical hole (`render/engine.rs:5954`'s
/// per-glyph admission rule, same unconditional instance pin
/// afterwards). Same shape as the `composite_glyphs` case above,
/// through the core-font path instead of the glyphset path.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn image_text_pin_ceiling_reserves_instance_buffer_pin() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no Vk — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");
    let target = alloc_drawable_3a_with_kind(
        &platform,
        &mut store,
        0x1,
        32,
        32,
        crate::kms::render::store::DrawableKind::Window,
        true,
    );

    {
        let inner = engine.inner.as_mut().expect("inner");
        inner.frame_builder.set_max_pinned_resources_per_frame(2);
    }

    let glyphs = vec![
        build_glyph(u32::from(b'A'), 1, 1, 2, 2),
        build_glyph(u32::from(b'B'), 10, 1, 2, 2),
    ];

    let stats = engine
        .image_text(
            &mut store,
            &mut platform,
            Dst::server_internal(target),
            7,
            [1.0, 1.0, 1.0, 1.0],
            &glyphs,
        )
        .expect("image_text");

    let inner = engine.inner.as_ref().expect("inner");
    let ceiling = inner.frame_builder.max_pinned_resources_per_frame();
    let open = inner
        .frame_builder
        .open
        .as_ref()
        .expect("frame stays open after image_text");
    assert!(
        open.pins.len() <= ceiling,
        "pin set must never exceed the declared ceiling: {} pins against a \
             ceiling of {} (glyphs_dropped={})",
        open.pins.len(),
        ceiling,
        stats.glyphs_dropped,
    );

    engine.drain_all(&mut platform);
}

// ── #137 step 4a: `first_instance`, plumbed end to end ──────
//
// A `CompositeGlyphs` request will have to be recorded as SEVERAL
// contiguous draw runs — glyphs of different `GlyphLayout`s need
// different pipelines, and pipeline state is immutable — all
// sharing ONE instance buffer, so the request still costs exactly
// one frame pin (the one the ceiling reserves). Each run therefore
// carries a `(first_instance, instance_count)` range.
//
// Production forms exactly one run today, so `record_glyph_runs`
// is the seam these two tests drive. The first drives the SAME
// function production calls, handed two ranges instead of one; the
// second pins `cmd_draw`'s `firstInstance` argument at its own
// level, without the recorder in between.
//
// The failure mode is an unplumbed `first_instance`: a run whose
// offset stays 0 draws run 0's glyphs a second time instead of its
// own. Nothing downstream of a run splitter could tell that apart
// from a splitter bug, which is why it is pinned here, before the
// splitter exists.

/// The glyphset the step-4a fixtures intern into: two 2×2 glyphs,
/// each of HALF coverage. Half, and composited with `Add`, so
/// drawing one of them twice is observable — see
/// `RUN_SPLIT_OP`.
const RUN_SPLIT_GS: u32 = 0x7401;
/// `Add` (wire PictOp 12), the op the run-split fixture composites
/// with. It is deliberately NOT idempotent at partial coverage:
/// under `Over` with opaque foreground, drawing range 0's glyph a
/// second time lands the same white on the same pixels, so a
/// `first_instance` error that ALSO widens the count (`count =
/// end - first_instance`, which is how the recorder computes it)
/// would paint an indistinguishable result. With `Add` at coverage
/// 0x80 the double draw saturates to 0xFF and the two
/// destinations differ.
const RUN_SPLIT_OP: u8 = 12;
/// Half coverage, so `Add`ing it twice is distinguishable from
/// `Add`ing it once.
const RUN_SPLIT_COVERAGE: u8 = 0x80;
/// Left glyph: instance 0 of the shared buffer.
const RUN_SPLIT_DST_0: (i32, i32) = (2, 2);
/// Right glyph: instance 1. Disjoint from instance 0's quad, so
/// "instance 1 was never drawn" is directly observable.
const RUN_SPLIT_DST_1: (i32, i32) = (10, 2);

fn run_split_glyph_inputs(pixels: &[u8; 4]) -> [CompositeGlyphInput<'_>; 2] {
    [
        CompositeGlyphInput {
            gs_xid: RUN_SPLIT_GS,
            glyph_id: 1,
            w: 2,
            h: 2,
            pixels: GlyphPixels::A8(pixels),
            dst_x: RUN_SPLIT_DST_0.0,
            dst_y: RUN_SPLIT_DST_0.1,
        },
        CompositeGlyphInput {
            gs_xid: RUN_SPLIT_GS,
            glyph_id: 2,
            w: 2,
            h: 2,
            pixels: GlyphPixels::A8(pixels),
            dst_x: RUN_SPLIT_DST_1.0,
            dst_y: RUN_SPLIT_DST_1.1,
        },
    ]
}

/// The two interned glyphs as `RecordedTextGlyph`s at the fixture
/// positions — the same values the per-glyph walk in
/// `composite_glyphs_via_frame_builder` builds.
fn run_split_recorded_glyphs(
    engine: &RenderEngine,
) -> Vec<crate::kms::render::frame_builder::RecordedTextGlyph> {
    let atlas = engine
        .inner
        .as_ref()
        .expect("inner")
        .glyph_atlas
        .as_ref()
        .expect("atlas built by the production call");
    [(1_u32, RUN_SPLIT_DST_0), (2_u32, RUN_SPLIT_DST_1)]
        .into_iter()
        .map(|(glyph_id, (dst_x, dst_y))| {
            let entry = atlas
                .lookup(GlyphKey {
                    font_xid: RUN_SPLIT_GS,
                    codepoint: glyph_id,
                })
                .expect("glyph committed to the atlas by the drained frame");
            crate::kms::render::frame_builder::RecordedTextGlyph {
                atlas_x: entry.atlas_x,
                atlas_y: entry.atlas_y,
                logical_w: entry.logical_w,
                h: entry.h,
                dst_x,
                dst_y,
                layout: entry.layout,
            }
        })
        .collect()
}

/// Two A8 glyphs recorded as **two** `(first_instance, count)`
/// ranges through `record_glyph_runs` must render exactly what the
/// same two glyphs render as production's single range.
///
/// With `first_instance` unplumbed, the second range redraws the
/// first glyph and the right-hand quad never appears, so the two
/// destinations differ. The two "must be painted" assertions keep
/// the comparison from passing on two blank images.
/// #177: a glyph run's instance data lives in an upload arena block
/// that is allocated and freed within one telemetry period here
/// (`drain_all` empties the idle list), so the live ledger (`vram by
/// use`) never sees it. The churn counters must: `upload_arena` gains a
/// block allocation and free, the arena counts a sub-allocation sized
/// for the run, and the formatted line shows non-zero rates. Other
/// tests run in parallel against the same process-wide counters, so
/// this asserts lower bounds only.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_glyph_run_upload_shows_in_churn_rates() {
    use crate::kms::vk::mem_accounting::{ChurnClass, churn_snapshot, format_churn_line};
    let Some(mut platform) = live_platform() else {
        eprintln!("no Vk — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");
    let target = alloc_drawable_3a(&platform, &mut store, 0x1, 32, 32);
    let full = vk::Rect2D {
        offset: vk::Offset2D::default(),
        extent: vk::Extent2D {
            width: 32,
            height: 32,
        },
    };
    let pixels = [0xFFu8; 4];
    let glyphs = run_split_glyph_inputs(&pixels);
    let draw =
        |engine: &mut RenderEngine, store: &mut DrawableStore, platform: &mut PlatformBackend| {
            engine
                .composite_glyphs(
                    store,
                    platform,
                    Dst::server_internal(target),
                    3,
                    0,
                    [1.0, 1.0, 1.0, 1.0],
                    &glyphs,
                    None,
                )
                .expect("composite_glyphs");
            // Closes the frame; `drain_all` then retires it and frees its
            // upload block.
            engine
                .get_image(store, platform, Src::server_internal(target), full, 32)
                .expect("get_image");
            engine.drain_all(platform);
        };
    // Warm-up interns the glyphs so the measured run allocates only
    // what every later run of the same text allocates.
    draw(&mut engine, &mut store, &mut platform);

    let before = churn_snapshot();
    draw(&mut engine, &mut store, &mut platform);
    let after = churn_snapshot();

    let (b, a) = (
        before.class(ChurnClass::UploadArena),
        after.class(ChurnClass::UploadArena),
    );
    let instance = std::mem::size_of::<crate::kms::vk::text_pipeline::GlyphInstanceData>();
    assert!(a.allocs > b.allocs, "upload block allocation not counted");
    assert!(a.frees > b.frees, "upload block free not counted");
    let (br, ar) = (before.upload_arena, after.upload_arena);
    assert!(
        ar.suballocs > br.suballocs,
        "glyph run sub-allocation not counted"
    );
    assert!(
        ar.suballoc_bytes - br.suballoc_bytes >= 2 * instance as u64,
        "glyph run bytes: {} < 2 instances",
        ar.suballoc_bytes - br.suballoc_bytes
    );
    assert!(
        after.class(ChurnClass::Readback).frees > before.class(ChurnClass::Readback).frees,
        "get_image readback staging not counted"
    );
    let line = format_churn_line(&before, &after, 1.0, None);
    let seg = |name: &str| {
        line.split(&format!(" {name}["))
            .nth(1)
            .and_then(|s| s.split(']').next())
            .unwrap_or_else(|| panic!("no {name} segment: {line}"))
            .to_owned()
    };
    let blocks = seg("upload_arena");
    assert!(
        !blocks.starts_with("alloc=0/s") && !blocks.contains(" free=0/s"),
        "rate line misses the block churn: {line}"
    );
    assert!(
        !seg("arena").contains(" sub=0/s"),
        "rate line misses the sub-allocation: {line}"
    );
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_glyph_run_split_into_two_ranges_renders_as_one_range() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no Vk — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");
    let one_range = alloc_drawable_3a(&platform, &mut store, 0x1, 32, 32);
    let two_ranges = alloc_drawable_3a(&platform, &mut store, 0x2, 32, 32);
    let full = vk::Rect2D {
        offset: vk::Offset2D::default(),
        extent: vk::Extent2D {
            width: 32,
            height: 32,
        },
    };
    let black = [0.0, 0.0, 0.0, 1.0];
    let white = [1.0, 1.0, 1.0, 1.0];

    // Both destinations start from the same fully-defined content
    // (the storage allocation itself is not initialised — see
    // `window_storage_init_covers_the_whole_allocation`).
    for id in [one_range, two_ranges] {
        engine
            .fill_rect(
                &mut store,
                &mut platform,
                Dst::server_internal(id),
                full,
                black,
            )
            .expect("fill_rect");
    }

    // (1) The baseline, through production: ONE run over both
    //     glyphs. This also interns them and builds the
    //     (Over, BGRA8, has-alpha) text pipeline that step (3)
    //     reuses.
    let pixels = [RUN_SPLIT_COVERAGE; 4];
    let glyphs = run_split_glyph_inputs(&pixels);
    engine
        .composite_glyphs(
            &mut store,
            &mut platform,
            Dst::server_internal(one_range),
            RUN_SPLIT_OP,
            0, // pict_format unknown → depth heuristic
            white,
            &glyphs,
            None,
        )
        .expect("composite_glyphs");
    // Read the baseline back. `get_image` is what CLOSES the open
    // frame (`drain_all` holds no `store` borrow and so cannot
    // commit a close), and the atlas cache inserts are
    // transactional on close-success — so this is also what makes
    // the entries below look-up-able.
    let out_one = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(one_range),
            full,
            32,
        )
        .expect("get_image one range");

    // (2) Reopen a frame on the second destination and first-touch
    //     it, exactly as any second op in a frame would find it.
    //     Its content is already the black fill from above; this
    //     repeats it only to open a frame and touch the drawable.
    engine
        .fill_rect(
            &mut store,
            &mut platform,
            Dst::server_internal(two_ranges),
            full,
            black,
        )
        .expect("fill_rect reopen");

    // (3) The same glyphs, recorded as TWO ranges over one shared
    //     instance buffer, through the function production uses.
    let recorded = run_split_recorded_glyphs(&engine);
    assert_eq!(recorded.len(), 2, "both glyphs must have interned");
    let pins_before = engine
        .inner
        .as_ref()
        .expect("inner")
        .frame_builder
        .open
        .as_ref()
        .expect("frame open after fill_rect")
        .pins
        .len();
    let inner = engine.inner.as_mut().expect("inner");
    let dst_old_layout = inner.current_layout_for_drawable(&store, two_ranges);
    let instances = RenderEngine::record_glyph_runs(
        inner,
        &[&recorded[..1], &recorded[1..]],
        &GlyphRunCommon {
            dst_id: two_ranges,
            dst_old_layout,
            op: RUN_SPLIT_OP,
            dst_has_alpha: dst_has_alpha_for_pict_format(vk::Format::B8G8R8A8_UNORM, 32, 0),
            foreground_rgba: white,
            clip_scissors: vec![full],
        },
    )
    .expect("record_glyph_runs");
    assert_eq!(instances, 2, "both glyphs must have become instances");
    store.mark_contents_modified(two_ranges);

    // Two runs, ONE instance pin — the property the frame-pin
    // ceiling's single reserved pin rests on.
    {
        let open = engine
            .inner
            .as_ref()
            .expect("inner")
            .frame_builder
            .open
            .as_ref()
            .expect("frame still open");
        assert_eq!(
            open.pins.len(),
            pins_before + 1,
            "two runs must share ONE pinned instance buffer",
        );
        let runs = open
            .ops
            .iter()
            .filter(|op| {
                matches!(
                    op,
                    crate::kms::render::frame_builder::RecordedOp::CompositeGlyphs(_)
                )
            })
            .count();
        assert_eq!(runs, 2, "the helper must have recorded two runs");
    }

    // (4) Read the two-range destination back and compare.
    let out_two = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(two_ranges),
            full,
            32,
        )
        .expect("get_image two ranges");

    // Teeth: both quads really are painted, so the equality below
    // is not comparing two black images.
    let px = |buf: &[u8], x: usize, y: usize| {
        let o = (y * 32 + x) * 4;
        [buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]
    };
    let bg = px(&out_one, 30, 30);
    for (x, y) in [
        (RUN_SPLIT_DST_0.0 as usize, RUN_SPLIT_DST_0.1 as usize),
        (RUN_SPLIT_DST_1.0 as usize, RUN_SPLIT_DST_1.1 as usize),
    ] {
        assert_ne!(
            px(&out_one, x, y),
            bg,
            "the one-range baseline must paint the glyph at ({x}, {y})",
        );
        assert_ne!(
            px(&out_two, x, y),
            bg,
            "the two-range recording must paint the glyph at ({x}, {y}) — a \
                 `first_instance` left at zero redraws range 0's glyph instead",
        );
    }
    assert_eq!(
        out_two, out_one,
        "recording the same glyphs as two ranges over one shared instance \
             buffer must be pixel-identical to recording them as one range",
    );

    engine.drain_all(&mut platform);
}

/// `record_text_run_scissored` at a NONZERO `first_instance`, with
/// no recorder in between: a 2-instance buffer drawn as the range
/// `[1, 2)` must paint the second glyph's quad and leave the
/// first's untouched.
///
/// This is the argument-level companion to the seam test above —
/// it fails if `first_instance` reaches the function but not
/// `cmd_draw`'s fourth argument.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn record_text_run_scissored_draws_only_the_requested_instance_range() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no Vk — skipping");
        return;
    };
    let Some(pool) = platform.ops_command_pool_handle() else {
        eprintln!("no ops command pool — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");
    let target = alloc_drawable_3a(&platform, &mut store, 0x1, 32, 32);
    let full = vk::Rect2D {
        offset: vk::Offset2D::default(),
        extent: vk::Extent2D {
            width: 32,
            height: 32,
        },
    };
    let white = [1.0, 1.0, 1.0, 1.0];

    // Intern the glyphs + build the text pipeline through
    // production, then drain so the atlas cache inserts commit and
    // the atlas image is left in SHADER_READ_ONLY_OPTIMAL.
    let pixels = [0xFFu8; 4];
    let glyphs = run_split_glyph_inputs(&pixels);
    engine
        .fill_rect(
            &mut store,
            &mut platform,
            Dst::server_internal(target),
            full,
            [0.0, 0.0, 0.0, 1.0],
        )
        .expect("fill_rect");
    engine
        .composite_glyphs(
            &mut store,
            &mut platform,
            Dst::server_internal(target),
            3,
            0,
            white,
            &glyphs,
            None,
        )
        .expect("composite_glyphs warm-up");
    // `get_image` is what closes the frame, and the atlas cache
    // inserts commit on close-success — `drain_all` holds no
    // `store` borrow and cannot close.
    engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(target),
            full,
            32,
        )
        .expect("get_image closes the warm-up frame");

    // Repaint the destination black so the warm-up's own quads are
    // gone and only this test's draw can put pixels down — and
    // close again, so no recorded op can land AFTER the
    // out-of-band draw below and overwrite it.
    engine
        .fill_rect(
            &mut store,
            &mut platform,
            Dst::server_internal(target),
            full,
            [0.0, 0.0, 0.0, 1.0],
        )
        .expect("fill_rect clear");
    engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(target),
            full,
            32,
        )
        .expect("get_image closes the clear frame");

    // A 2-instance buffer, in glyph order.
    let recorded = run_split_recorded_glyphs(&engine);
    let mut bytes: Vec<u8> = Vec::new();
    for g in &recorded {
        let inst = crate::kms::vk::text_pipeline::GlyphInstanceData::from_glyph(
            g.dst_x,
            g.dst_y,
            g.atlas_x,
            g.atlas_y,
            g.logical_w,
            g.h,
            g.layout,
        )
        .expect("instance geometry");
        bytes.extend_from_slice(inst.as_bytes());
    }
    {
        let inner = engine.inner.as_mut().expect("inner");
        let vk_ctx = Arc::clone(&inner.vk);
        let buf = StagingBuffer::new_with_usage(
            Arc::clone(&vk_ctx),
            u64::try_from(bytes.len()).expect("len"),
            vk::BufferUsageFlags::VERTEX_BUFFER,
            crate::kms::vk::mem_accounting::ChurnClass::GlyphRun,
        )
        .expect("instance buffer");
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf.mapped.as_ptr(), bytes.len());
        }
        let atlas_extent = inner.glyph_atlas.as_ref().expect("atlas").extent();
        let pipeline = inner
            .text_pipelines
            .get(&(3, vk::Format::B8G8R8A8_UNORM, true, false))
            .expect("pipeline built by the warm-up");
        let drawable = store.get_mut(target).expect("target");
        let mut adapter = StorageTextTarget {
            extent: drawable.storage.extent,
            image: drawable.storage.image,
            image_view: drawable.storage.image_view,
            current_layout: drawable.storage.current_layout,
        };
        crate::kms::vk::ops::run_one_shot_op(&vk_ctx, pool, |vk, cb| {
            crate::kms::vk::ops::text::record_text_run_scissored(
                vk,
                cb,
                &mut adapter,
                atlas_extent,
                pipeline,
                buf.buffer,
                0,
                // Range [1, 2): the SECOND instance only.
                1,
                1,
                white,
                &[full],
            )
        })
        .expect("one-shot text run");
        drawable.storage.current_layout = adapter.current_layout;
    }

    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(target),
            full,
            32,
        )
        .expect("get_image");
    let px = |x: usize, y: usize| {
        let o = (y * 32 + x) * 4;
        [out[o], out[o + 1], out[o + 2], out[o + 3]]
    };
    let bg = px(30, 30);
    assert_ne!(
        px(RUN_SPLIT_DST_1.0 as usize, RUN_SPLIT_DST_1.1 as usize),
        bg,
        "instance 1 is the range's only member and must be drawn",
    );
    assert_eq!(
        px(RUN_SPLIT_DST_0.0 as usize, RUN_SPLIT_DST_0.1 as usize),
        bg,
        "instance 0 is BELOW first_instance and must not be drawn — a \
             hardcoded firstInstance of 0 draws it",
    );

    engine.drain_all(&mut platform);
}

// ── #137 step 4b: the run splitter ────────────────────────────
//
// A single `CompositeGlyphs` request can switch glyphset
// mid-stream (the inline `count == 255` items element) and
// glyphsets can differ in picture format, so one request can
// interleave A8 and ARGB32 glyphs — which need different
// pipelines, and pipeline state is immutable. The request is
// therefore recorded as several contiguous runs.
//
// Both tests below share one fixture: a real items stream over
// two glyphsets of different formats, parsed by the production
// parse (`parse_composite_glyph_items`). The first asserts the
// splitter's ORDER; the second asserts that today it produces
// exactly one run.

/// Two glyphsets — A8 and ARGB32 — and an items stream that
/// alternates between them via the inline `count == 255`
/// element. Returns `(glyphsets, initial_gs_xid, items)`.
///
/// The stream is deliberately not one-glyph-per-element:
/// elements of 2, 1, 1 and 2 glyphs, so a run has to span an
/// element boundary (glyphs 0-1) and two same-format glyphs
/// inside one element must stay in one run (glyphs 4-5). Source
/// formats come out as A8, A8, ARGB32, A8, ARGB32, ARGB32 and
/// the pen advances one pixel per glyph, so `dst_x` doubles as
/// each glyph's index in request order.
fn mixed_format_items_fixture() -> (HashMap<u32, crate::kms::core::GlyphSetState>, u32, Vec<u8>) {
    use crate::kms::core::{GlyphSetFormat, GlyphSetState, StoredGlyph};

    const GS_A8: u32 = 0x4B01;
    const GS_ARGB32: u32 = 0x4B02;

    let mut glyphsets: HashMap<u32, GlyphSetState> = HashMap::new();
    for (xid, format, bytes_per_pixel) in [
        (GS_A8, GlyphSetFormat::A8, 1),
        (GS_ARGB32, GlyphSetFormat::Argb32, 4),
    ] {
        let mut glyphs = HashMap::new();
        // Ids 1..=6, so every glyph the stream names resolves in
        // whichever glyphset is active at the time.
        for glyph_id in 1..=6u32 {
            glyphs.insert(
                glyph_id,
                StoredGlyph {
                    width: 1,
                    height: 1,
                    x: 0,
                    y: 0,
                    x_off: 1,
                    y_off: 0,
                    pixels: vec![0x80; bytes_per_pixel],
                    format,
                },
            );
        }
        glyphsets.insert(xid, GlyphSetState { format, glyphs });
    }

    // Element layout: count(u8) pad pad pad dx(i16) dy(i16), then
    // `count` 1-byte ids padded to a 4-byte boundary (minor 23).
    let mut items: Vec<u8> = Vec::new();
    let element = |items: &mut Vec<u8>, ids: &[u8]| {
        items.extend_from_slice(&[u8::try_from(ids.len()).expect("count"), 0, 0, 0, 0, 0, 0, 0]);
        items.extend_from_slice(ids);
        while !items.len().is_multiple_of(4) {
            items.push(0);
        }
    };
    let switch_to = |items: &mut Vec<u8>, xid: u32| {
        items.extend_from_slice(&[255u8, 0, 0, 0]);
        items.extend_from_slice(&xid.to_le_bytes());
    };
    element(&mut items, &[1, 2]); // glyphs 0,1 — A8 (initial gs)
    switch_to(&mut items, GS_ARGB32);
    element(&mut items, &[3]); // glyph 2 — ARGB32
    switch_to(&mut items, GS_A8);
    element(&mut items, &[4]); // glyph 3 — A8
    switch_to(&mut items, GS_ARGB32);
    element(&mut items, &[5, 6]); // glyphs 4,5 — ARGB32

    (glyphsets, GS_A8, items)
}

/// The parsed glyphs as the per-glyph walk would record them —
/// one `RecordedTextGlyph` per parsed glyph, same order, each
/// tagged with the effective layout a device with (or without)
/// `dualSrcBlend` gives it. The atlas coordinates are irrelevant
/// to the splitter; `dst_x` carries the glyph's request-order
/// index.
fn recorded_from_parsed(
    parsed: &[crate::kms::render::backend::ParsedGlyph],
    component_alpha_supported: bool,
) -> Vec<crate::kms::render::frame_builder::RecordedTextGlyph> {
    parsed
        .iter()
        .map(|p| crate::kms::render::frame_builder::RecordedTextGlyph {
            atlas_x: 0,
            atlas_y: 0,
            logical_w: p.w,
            h: p.h,
            dst_x: p.dst_x,
            dst_y: p.dst_y,
            layout: RenderEngine::effective_glyph_layout(
                p.source_format,
                component_alpha_supported,
            ),
        })
        .collect()
}

/// The splitter must cut CONTIGUOUS runs in REQUEST ORDER.
///
/// Homogeneity alone is not the property worth testing: a
/// splitter that gathers all the A8 glyphs into one run and all
/// the component-alpha glyphs into another is perfectly
/// homogeneous and wrong — PictOps are not commutative, so
/// reordering changes pixels wherever two glyph quads overlap,
/// and overlap is ordinary (kerning, italics, combining marks).
/// So the load-bearing assertion is that concatenating the runs
/// reproduces the input glyph sequence exactly: same glyphs, same
/// order, none lost or duplicated at a boundary.
///
/// The layout sequence here is heterogeneous, which is what
/// `effective_glyph_layout` answers for this stream on a device
/// WITH `dualSrcBlend` — i.e. what production produces on
/// lavapipe, RADV and every desktop GPU. The companion test
/// below covers the device that has none, where the same stream
/// collapses to one run.
#[test]
fn a_mixed_format_items_stream_splits_into_ordered_homogeneous_runs() {
    let (glyphsets, initial_gs, items) = mixed_format_items_fixture();
    let parsed = crate::kms::render::backend::parse_composite_glyph_items(
        &glyphsets, 23, initial_gs, 0, 0, &items,
    );

    // The tag follows the inline glyphset change, in request
    // order. If the parse ignored the `count == 255` element this
    // would be all-A8, and no splitter could recover.
    assert_eq!(
        parsed
            .glyphs
            .iter()
            .map(|p| p.source_format)
            .collect::<Vec<_>>(),
        vec![
            GlyphSourceFormat::A8,
            GlyphSourceFormat::A8,
            GlyphSourceFormat::Argb32,
            GlyphSourceFormat::A8,
            GlyphSourceFormat::Argb32,
            GlyphSourceFormat::Argb32,
        ],
        "source-format tags must follow the inline glyphset change, in request order",
    );

    // Layouts as a `dualSrcBlend` device gives them: ARGB32
    // interns as four packed planes, A8 as one.
    let recorded = recorded_from_parsed(&parsed.glyphs, true);
    // Request-order index, carried in dst_x by the fixture's
    // one-pixel pen advance.
    assert_eq!(
        recorded.iter().map(|g| g.dst_x).collect::<Vec<_>>(),
        vec![0, 1, 2, 3, 4, 5],
        "the fixture's pen must advance one pixel per glyph",
    );

    let layouts: Vec<GlyphLayout> = recorded.iter().map(|g| g.layout).collect();
    assert_eq!(
        layouts,
        vec![
            GlyphLayout::A8,
            GlyphLayout::A8,
            GlyphLayout::ComponentAlpha,
            GlyphLayout::A8,
            GlyphLayout::ComponentAlpha,
            GlyphLayout::ComponentAlpha,
        ],
        "on a dualSrcBlend device ARGB32 interns as four packed planes",
    );
    let runs = RenderEngine::split_glyph_runs(&recorded);

    // (a) Contiguous and maximal: 2 + 1 + 1 + 2. One run per
    //     glyph would also be homogeneous and ordered; these
    //     lengths reject it, and they prove a run spans an
    //     element boundary (glyphs 0-1) while two same-format
    //     glyphs in one element stay together (glyphs 4-5).
    assert_eq!(
        runs.iter().map(|r| r.len()).collect::<Vec<_>>(),
        vec![2, 1, 1, 2],
        "runs must be maximal contiguous stretches of one layout",
    );

    // (b) ORDER, and nothing lost or duplicated: the runs
    //     concatenated ARE the input sequence.
    let flattened: Vec<crate::kms::render::frame_builder::RecordedTextGlyph> =
        runs.iter().flat_map(|r| r.iter().copied()).collect();
    assert_eq!(
        flattened, recorded,
        "concatenating the runs must reproduce the glyphs in request order",
    );

    // (c) Homogeneous, and adjacent runs differ — so the cut is
    //     exactly where the layout changes.
    let mut at = 0usize;
    let mut run_layouts = Vec::new();
    for run in &runs {
        let slice = &layouts[at..at + run.len()];
        assert!(
            slice.iter().all(|l| *l == slice[0]),
            "run at {at} mixes layouts: {slice:?}",
        );
        run_layouts.push(slice[0]);
        at += run.len();
    }
    assert_eq!(at, layouts.len(), "the runs must cover every glyph");
    assert_eq!(
        run_layouts,
        vec![
            GlyphLayout::A8,
            GlyphLayout::ComponentAlpha,
            GlyphLayout::A8,
            GlyphLayout::ComponentAlpha,
        ],
        "adjacent runs must differ in layout, in request order",
    );
}

/// The splitter is inert **exactly where `dualSrcBlend` is
/// absent**, and only there.
///
/// This test used to assert inertness unconditionally, because
/// step 3's upload reduction applied on every device: an ARGB32
/// glyph WAS an A8 glyph in the atlas, so a mixed request could
/// only ever form one run. Step 5 narrowed that reduction to
/// `!component_alpha_supported`, and lavapipe and RADV both
/// report `dualSrcBlend`, so the unconditional claim is now
/// false on every device CI and the desktop actually run on.
///
/// What survives is the conditional half, which is worth more:
/// with the reduction in force every glyph of a mixed stream has
/// the SAME effective layout, so the split really is inert there,
/// and a device that cannot blend four planes never records an op
/// asking it to. The `true` case — several runs from the same
/// stream — is the sibling test above.
#[test]
fn the_run_split_is_inert_only_where_component_alpha_is_unsupported() {
    let (glyphsets, initial_gs, items) = mixed_format_items_fixture();
    let parsed = crate::kms::render::backend::parse_composite_glyph_items(
        &glyphsets, 23, initial_gs, 0, 0, &items,
    );
    assert_eq!(parsed.glyphs.len(), 6, "fixture must parse six glyphs");
    assert!(
        parsed
            .glyphs
            .iter()
            .any(|p| p.source_format == GlyphSourceFormat::Argb32),
        "fixture must actually mix formats, or inertness is vacuous",
    );

    // No `dualSrcBlend`: the upload reduces ARGB32 to one
    // grayscale coverage plane, so every glyph is an A8 entry.
    let recorded = recorded_from_parsed(&parsed.glyphs, false);
    let layouts: Vec<GlyphLayout> = recorded.iter().map(|g| g.layout).collect();
    assert!(
        layouts.iter().all(|l| *l == GlyphLayout::A8),
        "without dualSrcBlend the upload reduces every source format to one \
             A8 plane: {layouts:?}",
    );

    let runs = RenderEngine::split_glyph_runs(&recorded);
    assert_eq!(runs.len(), 1, "a reduced request must record as ONE run");
    assert_eq!(
        runs[0],
        &recorded[..],
        "the single run must carry every glyph, in request order",
    );

    // And the same stream on a device that CAN blend four planes
    // does not collapse — so the assertion above is a statement
    // about the device, not a tautology about the fixture.
    let with_ca = recorded_from_parsed(&parsed.glyphs, true);
    assert_eq!(
        RenderEngine::split_glyph_runs(&with_ca).len(),
        4,
        "with dualSrcBlend the same mixed stream must record as four runs",
    );
}

/// The layout derivation itself, as a table — the one place
/// `component_alpha_supported` is consulted for glyphs, and the
/// pure test of the `dualSrcBlend`-less fallback's SELECTION
/// (`glyph_pixels` tests pin the reduction's arithmetic).
///
/// No runtime switch is needed to reach either column: the
/// function takes the capability as an argument
/// (`feedback_no_feature_kill_switches`).
#[test]
fn the_effective_layout_answers_component_alpha_only_for_argb32_on_a_capable_device() {
    for supported in [false, true] {
        for source in [GlyphSourceFormat::A8, GlyphSourceFormat::A1] {
            assert_eq!(
                RenderEngine::effective_glyph_layout(source, supported),
                GlyphLayout::A8,
                "{source:?} is a single coverage plane whatever the device does",
            );
        }
    }
    assert_eq!(
        RenderEngine::effective_glyph_layout(GlyphSourceFormat::Argb32, true),
        GlyphLayout::ComponentAlpha,
        "ARGB32 on a dualSrcBlend device packs four planes",
    );
    assert_eq!(
        RenderEngine::effective_glyph_layout(GlyphSourceFormat::Argb32, false),
        GlyphLayout::A8,
        "ARGB32 without dualSrcBlend reduces to one grayscale plane — the \
             fallback vk/device.rs already promises",
    );
}

// ── #137 step 5: the packed footprint reaches the UPLOAD ─────
//
// `AtlasEntry` carries `packed_w` (what the packer reserved and
// what the copy region covers) and `logical_w` (the glyph's own
// size). Step 2 split them but could not assert which one the
// recorded upload gets, because production built both from a
// single variable and no call path produced an asymmetric entry.
// Component alpha is the first and only path where they differ,
// so this is the first step where the assertion is reachable —
// and the failure mode is an upload copying a QUARTER of the
// glyph, three planes left as whatever the atlas held.

/// The recorded `GlyphUpload` for a component-alpha glyph carries
/// the PACKED width, and its committed entry carries both widths.
///
/// The op is inspected on the still-open frame, before the close
/// that replays it — `packed_w` is exactly what
/// `GlyphAtlas::record_upload` passes as the copy region's
/// `image_extent.width`.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_component_alpha_glyph_upload_records_the_packed_width() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no Vk — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");
    let target = alloc_drawable_3a(&platform, &mut store, 0x1, 32, 32);

    // 2x2 ARGB32 wire, dense CARD32 rows: [B, G, R, A] per pixel.
    let wire: [u8; 16] = [
        0x10, 0x20, 0x30, 0x40, 0x11, 0x21, 0x31, 0x41, 0x12, 0x22, 0x32, 0x42, 0x13, 0x23, 0x33,
        0x43,
    ];
    let glyphs = [CompositeGlyphInput {
        gs_xid: 0x7501,
        glyph_id: 1,
        w: 2,
        h: 2,
        pixels: GlyphPixels::Argb32Wire(&wire),
        dst_x: 1,
        dst_y: 1,
    }];
    engine
        .composite_glyphs(
            &mut store,
            &mut platform,
            Dst::server_internal(target),
            3, // Over
            0,
            [1.0, 1.0, 1.0, 1.0],
            &glyphs,
            None,
        )
        .expect("composite_glyphs");

    let supported = engine
        .inner
        .as_ref()
        .expect("inner")
        .vk
        .component_alpha_supported;
    let uploads: Vec<(u32, u32, GlyphLayout, u32)> = engine
        .inner
        .as_ref()
        .expect("inner")
        .frame_builder
        .open
        .as_ref()
        .expect("the call opened a frame")
        .ops
        .iter()
        .filter_map(|op| match op {
            crate::kms::render::frame_builder::RecordedOp::GlyphUpload(up) => Some((
                up.packed_w,
                up.h,
                up.insert_entry.layout,
                up.insert_entry.logical_w,
            )),
            _ => None,
        })
        .collect();
    assert_eq!(uploads.len(), 1, "one glyph, one recorded upload");
    let (packed_w, h, layout, logical_w) = uploads[0];
    assert_eq!(h, 2);
    assert_eq!(logical_w, 2, "the entry's logical width is the glyph's own");

    if supported {
        assert_eq!(
            layout,
            GlyphLayout::ComponentAlpha,
            "a dualSrcBlend device must intern ARGB32 as four planes",
        );
        assert_eq!(
            packed_w, 8,
            "the recorded upload must copy the PACKED footprint 4 * 2 = 8; \
                 receiving the logical width instead copies a quarter of the \
                 glyph and leaves three planes as whatever the atlas held",
        );
        assert_ne!(
            packed_w, logical_w,
            "this is the one path where the two widths differ — if they are \
                 equal here the assertion above is vacuous",
        );
    } else {
        // No dualSrcBlend: the upload reduced to one grayscale
        // plane, so the two widths coincide and there is nothing
        // asymmetric to catch here.
        assert_eq!(layout, GlyphLayout::A8);
        assert_eq!(packed_w, logical_w);
    }

    engine.drain_all(&mut platform);
}

/// #137 step 4b, carry-forward from 4a: the destination layout on
/// runs 2+.
///
/// Run 0 finds the destination in the frame's pre-op layout;
/// every later run finds it as the previous run left it, and
/// `record_text_run_scissored` ends in `SHADER_READ_ONLY_OPTIMAL`.
/// Carrying the pre-op layout on run 2+ declares a wrong
/// `oldLayout` in its barrier — and with a pre-op `UNDEFINED`,
/// which is exactly what a first-touched destination has, the
/// driver is then licensed to DISCARD the earlier runs' pixels.
///
/// The oracle is the RECORDED value, not pixels: an `UNDEFINED`
/// `oldLayout` is *permitted* to preserve contents, so a pixel
/// assertion on lavapipe passes whether the barrier is right or
/// wrong. Asserting `dst_old_layout` per run is exact and
/// driver-independent. The closing `get_image` then confirms the
/// recorded barriers actually emit and submit.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_second_glyph_run_records_the_layout_the_first_run_left() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no Vk — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");
    let warm = alloc_drawable_3a(&platform, &mut store, 0x1, 32, 32);
    // The split destination is never painted before the runs are
    // recorded, so its pre-op layout is the dangerous one.
    let split_dst = alloc_drawable_3a(&platform, &mut store, 0x2, 32, 32);
    let full = vk::Rect2D {
        offset: vk::Offset2D::default(),
        extent: vk::Extent2D {
            width: 32,
            height: 32,
        },
    };
    let white = [1.0, 1.0, 1.0, 1.0];

    // Warm-up through production: interns the two fixture glyphs
    // and builds the text pipeline emit will look up.
    engine
        .fill_rect(
            &mut store,
            &mut platform,
            Dst::server_internal(warm),
            full,
            [0.0, 0.0, 0.0, 1.0],
        )
        .expect("fill_rect warm");
    let pixels = [RUN_SPLIT_COVERAGE; 4];
    let glyphs = run_split_glyph_inputs(&pixels);
    engine
        .composite_glyphs(
            &mut store,
            &mut platform,
            Dst::server_internal(warm),
            RUN_SPLIT_OP,
            0,
            white,
            &glyphs,
            None,
        )
        .expect("composite_glyphs warm");
    // Closes the frame, which is what commits the atlas inserts.
    engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(warm),
            full,
            32,
        )
        .expect("get_image warm");

    // Open a frame on the OTHER drawable, so `split_dst` is
    // untouched when the runs are recorded.
    engine
        .fill_rect(
            &mut store,
            &mut platform,
            Dst::server_internal(warm),
            full,
            [0.0, 0.0, 0.0, 1.0],
        )
        .expect("fill_rect reopen");

    let recorded = run_split_recorded_glyphs(&engine);
    assert_eq!(recorded.len(), 2, "both glyphs must have interned");
    let prior_ticket = store
        .get(split_dst)
        .and_then(|d| d.last_render_ticket.clone());
    let inner = engine.inner.as_mut().expect("inner");
    let pre_op_layout = inner.current_layout_for_drawable(&store, split_dst);
    // Teeth: if the pre-op layout already WERE
    // SHADER_READ_ONLY_OPTIMAL, run 0 and run 1 would carry the
    // same value and the assertions below could not tell a
    // carried pre-op layout from the correct one. Measured
    // `UNDEFINED` here — the case where carrying it onto run 1
    // would license the driver to discard run 0's pixels.
    assert_ne!(
        pre_op_layout,
        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        "the split destination's pre-op layout must differ from where a run ends",
    );
    // First-touch it exactly as the production path does before
    // recording against it.
    {
        let open = inner.frame_builder.open.as_mut().expect("frame open");
        open.touched.first_touch(split_dst, prior_ticket);
        open.layouts.first_touch_drawable(split_dst, pre_op_layout);
    }
    let instances = RenderEngine::record_glyph_runs(
        inner,
        &[&recorded[..1], &recorded[1..]],
        &GlyphRunCommon {
            dst_id: split_dst,
            dst_old_layout: pre_op_layout,
            op: RUN_SPLIT_OP,
            dst_has_alpha: dst_has_alpha_for_pict_format(vk::Format::B8G8R8A8_UNORM, 32, 0),
            foreground_rgba: white,
            clip_scissors: vec![full],
        },
    )
    .expect("record_glyph_runs");
    assert_eq!(instances, 2, "both glyphs must have become instances");
    store.mark_contents_modified(split_dst);

    let layouts: Vec<vk::ImageLayout> = engine
        .inner
        .as_ref()
        .expect("inner")
        .frame_builder
        .open
        .as_ref()
        .expect("frame still open")
        .ops
        .iter()
        .filter_map(|op| match op {
            crate::kms::render::frame_builder::RecordedOp::CompositeGlyphs(cg)
                if cg.dst_id == split_dst =>
            {
                Some(cg.dst_old_layout)
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        layouts,
        vec![pre_op_layout, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL],
        "run 0 carries the request's pre-op layout; run 1 carries where run 0 left the image",
    );

    // The recorded barriers must also be emittable: close the
    // frame and submit them.
    engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(split_dst),
            full,
            32,
        )
        .expect("get_image closes the two-run frame");
    engine.drain_all(&mut platform);
}
