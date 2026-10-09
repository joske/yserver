use super::*;

// ── Stage 3c.3 acceptance tests ─────────────────────────────
//
// Engine-direct RENDER paint oracles. Each test allocates one
// or two Vk-backed drawables, drives `render_composite` /
// `render_fill_rectangles` through `RenderEngine`, then
// round-trips via `get_image` and asserts pixel-level
// correctness against a CPU oracle. The seventh acceptance
// test (`render_composite_no_gc_clip_leak`) lives in
// `tests/acceptance.rs` because the "no GC clip leak"
// property is a Backend-trait invariant (engine has no GC
// clip notion).

/// Allocate a Vk-backed depth-32 pixmap and pre-fill it with
/// `color` via the engine's fill_rect path. Returns the
/// store DrawableId.
fn alloc_filled_pixmap(
    platform: &mut PlatformBackend,
    store: &mut DrawableStore,
    engine: &mut RenderEngine,
    xid: u32,
    w: u16,
    h: u16,
    color_bgra_premul: [f32; 4],
) -> DrawableId {
    let storage = platform
        .allocate_drawable_storage(w, h, 32)
        .expect("alloc storage");
    let id = store
        .allocate(
            xid,
            crate::kms::render::store::DrawableKind::Pixmap,
            32,
            false,
            storage,
        )
        .expect("store.allocate");
    engine
        .fill_rect(
            store,
            platform,
            Dst::server_internal(id),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: u32::from(w),
                    height: u32::from(h),
                },
            },
            color_bgra_premul,
        )
        .expect("pre-fill");
    id
}

fn full_rect(w: u32, h: u32) -> crate::kms::vk::ops::render::CompositeRect {
    crate::kms::vk::ops::render::CompositeRect {
        src_x: 0,
        src_y: 0,
        mask_x: 0,
        mask_y: 0,
        dst_x: 0,
        dst_y: 0,
        width: w,
        height: h,
    }
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn render_composite_over_renders_alpha_blended() {
    // 50%-alpha red (premultiplied: r=0.5, a=0.5) Over opaque
    // green. Over: out = src + dst * (1 - src.a).
    //   out.b = 0 + 0 * 0.5 = 0
    //   out.g = 0 + 1 * 0.5 = 0.5 → 0x80
    //   out.r = 0.5 + 0 * 0.5 = 0.5 → 0x80
    //   out.a = 0.5 + 1 * 0.5 = 1.0 → 0xFF
    let Some(mut platform) = live_platform() else {
        eprintln!("no Vk — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");

    let dst = alloc_filled_pixmap(
        &mut platform,
        &mut store,
        &mut engine,
        0x1,
        4,
        4,
        [0.0, 1.0, 0.0, 1.0], // opaque green
    );

    let stats = engine
        .render_composite(
            &mut store,
            &mut platform,
            3,                                           // Over
            ResolvedSource::Solid([0.5, 0.0, 0.0, 0.5]), // 50% red premul
            ResolvedSource::None,
            Dst::server_internal(dst),
            &[full_rect(4, 4)],
            None,
            Repeat::None,
            Repeat::None,
            None,
            None,
            false,
            0,
            0,
            0,
        )
        .expect("render_composite");
    assert_eq!(stats.recorded_draws, 1);
    assert!(!stats.used_dst_readback);
    assert!(!stats.used_src_alias_scratch);

    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(dst),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 4,
                    height: 4,
                },
            },
            32,
        )
        .expect("get_image");
    // Centre pixel (1, 1): BGRA = [0, 0x80, 0x80, 0xFF] (±1).
    let off = (4 + 1) * 4;
    let near = |a: u8, b: u8| a.abs_diff(b) <= 2;
    assert!(near(out[off], 0x00), "B at centre: got {:#x}", out[off]);
    assert!(
        near(out[off + 1], 0x80),
        "G at centre: got {:#x}",
        out[off + 1]
    );
    assert!(
        near(out[off + 2], 0x80),
        "R at centre: got {:#x}",
        out[off + 2]
    );
    assert!(
        near(out[off + 3], 0xFF),
        "A at centre: got {:#x}",
        out[off + 3]
    );

    engine.drain_all(&mut platform);
}

/// The cairo/Pango component-alpha text path, pass 1: glyph
/// coverage composited with `op=Add` into a depth-8 R8 a8 mask
/// pixmap (the i3-config-wizard black-dialog bug — this exact
/// shape was dropped by both the old `op != Over` gate and the
/// old BGRA8-only dst gate). Two half-coverage (0x80) Adds at
/// the same position must ACCUMULATE to full coverage —
/// distinguishing Add's `(ONE, ONE)` blend from Over, which
/// would converge on 0xC0.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn composite_glyphs_add_accumulates_into_r8_mask() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no Vk — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");

    // Depth-8 pixmap → R8_UNORM storage (format_for_depth).
    let storage = platform
        .allocate_drawable_storage(4, 4, 8)
        .expect("alloc a8 mask storage");
    let mask = store
        .allocate(
            0xA8A8,
            crate::kms::render::store::DrawableKind::Pixmap,
            8,
            false,
            storage,
        )
        .expect("store.allocate");
    assert_eq!(
        store.get(mask).unwrap().storage.format,
        vk::Format::R8_UNORM,
        "depth-8 pixmap must be R8 storage",
    );
    // Clear coverage to 0 (cairo FillRectangles op=Clear).
    engine
        .fill_rect(
            &mut store,
            &mut platform,
            Dst::server_internal(mask),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 4,
                    height: 4,
                },
            },
            [0.0, 0.0, 0.0, 0.0],
        )
        .expect("clear mask");

    // One 2×2 glyph of half coverage (0x80) at (1, 1), Added
    // twice. Opaque white premul foreground (cairo uses a
    // solid source for the mask pass): alpha = fg.a * cov.
    let pixels = [0x80u8; 4];
    let glyph = [CompositeGlyphInput {
        gs_xid: 0x6060,
        glyph_id: 7,
        w: 2,
        h: 2,
        pixels: GlyphPixels::A8(&pixels),
        dst_x: 1,
        dst_y: 1,
    }];
    for _ in 0..2 {
        engine
            .composite_glyphs(
                &mut store,
                &mut platform,
                Dst::server_internal(mask),
                12, // Add — the cairo mask-accumulation op
                0,  // pict_format unknown → depth heuristic (R8 ⇒ has-alpha)
                [1.0, 1.0, 1.0, 1.0],
                &glyph,
                None,
            )
            .expect("composite_glyphs Add");
    }

    // get_image closes the open frame and reads back. Depth-8
    // readback is 1 byte/pixel from the R channel.
    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(mask),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 4,
                    height: 4,
                },
            },
            8,
        )
        .expect("get_image");
    let near = |a: u8, b: u8| a.abs_diff(b) <= 2;
    // Glyph pixel (1,1): 0x80 + 0x80 → 0xFF (clamped). Over
    // would give 0x80 + 0x80·(1−0.5) = 0xC0 — the assert
    // fails under Over, passes under Add.
    let at = |x: usize, y: usize| out[y * 4 + x];
    assert!(
        near(at(1, 1), 0xFF),
        "Add must accumulate coverage: got {:#x}",
        at(1, 1)
    );
    // Outside the glyph: still 0.
    assert!(
        near(at(0, 0), 0x00),
        "untouched mask pixel must stay 0: got {:#x}",
        at(0, 0)
    );

    engine.drain_all(&mut platform);
}

/// The cairo/Pango component-alpha text path, end to end:
/// pass 1 Adds glyph coverage into the a8 mask (above), pass 2
/// paints the window through the mask with the general
/// `Composite op=Src` (solid source, mask = the a8 pixmap —
/// sampled via the AlphaOnlyR8 swizzle). Text pixels must land
/// on the BGRA dst; zero-coverage pixels get src·0.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn composite_glyphs_add_mask_then_composite_src_renders_text() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no Vk — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");

    // Pass 1: a8 mask with a full-coverage 2×2 glyph at (1,1).
    let storage = platform
        .allocate_drawable_storage(4, 4, 8)
        .expect("alloc a8 mask storage");
    let mask = store
        .allocate(
            0xA8A9,
            crate::kms::render::store::DrawableKind::Pixmap,
            8,
            false,
            storage,
        )
        .expect("store.allocate");
    engine
        .fill_rect(
            &mut store,
            &mut platform,
            Dst::server_internal(mask),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 4,
                    height: 4,
                },
            },
            [0.0, 0.0, 0.0, 0.0],
        )
        .expect("clear mask");
    let pixels = [0xFFu8; 4];
    let glyph = [CompositeGlyphInput {
        gs_xid: 0x6061,
        glyph_id: 8,
        w: 2,
        h: 2,
        pixels: GlyphPixels::A8(&pixels),
        dst_x: 1,
        dst_y: 1,
    }];
    engine
        .composite_glyphs(
            &mut store,
            &mut platform,
            Dst::server_internal(mask),
            12, // Add
            0,
            [1.0, 1.0, 1.0, 1.0],
            &glyph,
            None,
        )
        .expect("composite_glyphs Add");

    // Pass 2: opaque-blue BGRA dst; Composite Src (white solid
    // through the mask) — the wizard's mask-paint pass.
    let dst = alloc_filled_pixmap(
        &mut platform,
        &mut store,
        &mut engine,
        0x2,
        4,
        4,
        [0.0, 0.0, 1.0, 1.0], // opaque blue (premul RGBA)
    );
    engine
        .render_composite(
            &mut store,
            &mut platform,
            1,                                           // Src
            ResolvedSource::Solid([1.0, 1.0, 1.0, 1.0]), // opaque white
            ResolvedSource::Drawable(SourceDrawable::whole(mask)),
            Dst::server_internal(dst),
            &[full_rect(4, 4)],
            None,
            Repeat::None,
            Repeat::None,
            None,
            None,
            false,
            0,
            0,
            0,
        )
        .expect("render_composite Src through a8 mask");

    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(dst),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 4,
                    height: 4,
                },
            },
            32,
        )
        .expect("get_image");
    let near = |a: u8, b: u8| a.abs_diff(b) <= 2;
    // Glyph pixel (1,1): white·1 replaces blue → BGRA FF FF FF FF.
    let off = (4 + 1) * 4;
    assert!(
        near(out[off], 0xFF) && near(out[off + 1], 0xFF) && near(out[off + 2], 0xFF),
        "text pixel must be white: got BGR {:#x} {:#x} {:#x}",
        out[off],
        out[off + 1],
        out[off + 2],
    );
    // Zero-coverage pixel (3,3): Src ⇒ white·0 = transparent
    // black replaces blue.
    let off00 = (4 * 3 + 3) * 4;
    assert!(
        near(out[off00], 0x00) && near(out[off00 + 1], 0x00) && near(out[off00 + 2], 0x00),
        "zero-coverage pixel must be src·0: got BGR {:#x} {:#x} {:#x}",
        out[off00],
        out[off00 + 1],
        out[off00 + 2],
    );

    engine.drain_all(&mut platform);
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn render_composite_picture_clip_per_rect() {
    // Two disjoint clip rects with a hole between them; one
    // composite covering the union bbox must paint inside both
    // rects AND leave the hole untouched. Exercises plan §4's
    // per-rect scissoring against v1's union-bbox shortcut.
    let Some(mut platform) = live_platform() else {
        eprintln!("no Vk — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");

    let dst = alloc_filled_pixmap(
        &mut platform,
        &mut store,
        &mut engine,
        0x1,
        8,
        4,
        [0.0, 0.0, 1.0, 1.0], // RGBA: opaque blue
    );
    // Two clip rects with a 2-wide hole at x=3..=4.
    let clip = vec![
        Rectangle16 {
            x: 0,
            y: 0,
            width: 3,
            height: 4,
        },
        Rectangle16 {
            x: 5,
            y: 0,
            width: 3,
            height: 4,
        },
    ];
    engine
        .render_composite(
            &mut store,
            &mut platform,
            1,                                           // Src
            ResolvedSource::Solid([1.0, 0.0, 0.0, 1.0]), // RGBA: opaque red
            ResolvedSource::None,
            Dst::server_internal(dst),
            &[full_rect(8, 4)],
            Some(&clip),
            Repeat::None,
            Repeat::None,
            None,
            None,
            false,
            0,
            0,
            0,
        )
        .expect("render_composite");
    // Verify observable output: red inside both clip rects, original
    // blue preserved in the 2-wide hole. (The internal `recorded_draws`
    // count is an implementation detail of the pre-rework submit path
    // and is intentionally not asserted — the pixels are the contract.)
    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(dst),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 8,
                    height: 4,
                },
            },
            32,
        )
        .expect("get_image");
    // BGRA layout: B at +0, R at +2.
    for y in 0..4 {
        for x in 0..8u32 {
            let off = (y * 8 + x as usize) * 4;
            let in_clip = (0..3).contains(&x) || (5..8).contains(&x);
            if in_clip {
                assert_eq!(out[off + 2], 0xFF, "R painted at ({x},{y})");
                assert_eq!(out[off], 0x00, "B cleared at ({x},{y})");
            } else {
                // Hole (x=3..=4): original blue.
                assert_eq!(out[off], 0xFF, "B preserved at ({x},{y})");
                assert_eq!(out[off + 2], 0x00, "R untouched at ({x},{y})");
            }
        }
    }

    engine.drain_all(&mut platform);
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn render_composite_solid_fill_source_path() {
    // SolidFill source over (op=Src) an unrelated start colour —
    // every dst pixel must equal the source's premul colour.
    let Some(mut platform) = live_platform() else {
        eprintln!("no Vk — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");

    let dst = alloc_filled_pixmap(
        &mut platform,
        &mut store,
        &mut engine,
        0x1,
        4,
        4,
        [0.0, 0.0, 0.0, 1.0], // opaque black
    );
    engine
        .render_composite(
            &mut store,
            &mut platform,
            1,                                             // Src
            ResolvedSource::Solid([0.25, 0.5, 0.75, 1.0]), // RGBA premul
            ResolvedSource::None,
            Dst::server_internal(dst),
            &[full_rect(4, 4)],
            None,
            Repeat::None,
            Repeat::None,
            None,
            None,
            false,
            0,
            0,
            0,
        )
        .expect("render_composite");
    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(dst),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 4,
                    height: 4,
                },
            },
            32,
        )
        .expect("get_image");
    // Storage BGRA bytes for RGBA(0.25, 0.5, 0.75, 1.0):
    // B=0.75→0xC0, G=0.5→0x80, R=0.25→0x40, A=1→0xFF.
    let near = |a: u8, b: u8| a.abs_diff(b) <= 1;
    for px in out.chunks_exact(4) {
        assert!(near(px[0], 0xC0), "B: {:#x}", px[0]);
        assert!(near(px[1], 0x80), "G: {:#x}", px[1]);
        assert!(near(px[2], 0x40), "R: {:#x}", px[2]);
        assert!(near(px[3], 0xFF), "A: {:#x}", px[3]);
    }
    engine.drain_all(&mut platform);
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn render_composite_linear_gradient_horizontal_two_stop() {
    // 256×1 dst pre-filled black; Composite Src + LinearGradient
    // source (p1=(0,0), p2=(256,0)<<16) with two stops:
    //   pos=0   black (0,0,0,1)
    //   pos=0xFFFFFFFF white (1,1,1,1)
    // Stage 3f.13 wires the LUT path — pixel n should read
    // roughly (n, n, n, 0xFF) ± a couple of units (NEAREST
    // sampler + LUT rounding).
    use crate::kms::vk::gradient::Stop;
    let Some(mut platform) = live_platform() else {
        eprintln!("no Vk — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");

    let dst = alloc_filled_pixmap(
        &mut platform,
        &mut store,
        &mut engine,
        0x1,
        256,
        1,
        [0.0, 0.0, 0.0, 1.0],
    );

    let grad_xid = 0xABBA_FACE_u32;
    engine
        .build_and_insert_linear_gradient(
            &mut platform,
            grad_xid,
            (0, 0),
            (256_i32 << 16, 0),
            &[
                Stop {
                    pos: 0,
                    r: 0,
                    g: 0,
                    b: 0,
                    a: 0xFFFF,
                },
                // 16.16 fixed-point: 1.0 = 0x10000. Using i32::MAX
                // here would put the second stop far past t=1.0,
                // so `sample_stops` would lerp `(target - 0) /
                // i32::MAX ≈ 0` and every LUT pixel would read
                // the first stop (black).
                Stop {
                    pos: 0x10000,
                    r: 0xFFFF,
                    g: 0xFFFF,
                    b: 0xFFFF,
                    a: 0xFFFF,
                },
            ],
        )
        .expect("build gradient");

    let stats = engine
        .render_composite(
            &mut store,
            &mut platform,
            1, // Src — copy source to dst, no blend
            ResolvedSource::Gradient(grad_xid),
            ResolvedSource::None,
            Dst::server_internal(dst),
            &[full_rect(256, 1)],
            None,
            Repeat::None,
            Repeat::None,
            None,
            None,
            false,
            0,
            0,
            0,
        )
        .expect("render_composite gradient");
    assert_eq!(stats.recorded_draws, 1);

    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(dst),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 256,
                    height: 1,
                },
            },
            32,
        )
        .expect("get_image");

    // Sample several points along the ramp; tolerate ±4 due to
    // NEAREST sampler + 8-bit LUT quantisation + premultiplied
    // colour conversion. Direction-of-travel + monotonicity is
    // the strong gate (rules out the 3f.12 first-stop collapse,
    // which would read 0 at every x).
    let bgra = |x: usize| (out[x * 4], out[x * 4 + 1], out[x * 4 + 2], out[x * 4 + 3]);
    let (b0, g0, r0, _a0) = bgra(0);
    let (bm, gm, rm, _am) = bgra(128);
    let (b255, g255, r255, _a255) = bgra(255);
    // x=0 is near-black; x=255 is near-white; x=128 sits between.
    assert!(b0 <= 4 && g0 <= 4 && r0 <= 4, "x=0 BGRA={:?}", bgra(0));
    assert!(
        b255 >= 0xF0 && g255 >= 0xF0 && r255 >= 0xF0,
        "x=255 BGRA={:?}",
        bgra(255),
    );
    assert!(
        (0x40..=0xC0).contains(&bm) && (0x40..=0xC0).contains(&gm) && (0x40..=0xC0).contains(&rm),
        "x=128 BGRA={:?} (expected mid-grey)",
        bgra(128),
    );

    // Cleanup so the gradient image is freed in this drain.
    engine.picture_paint_remove(grad_xid);
    engine.drain_all(&mut platform);
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn render_composite_radial_gradient_centred() {
    // 64×64 dst, radial gradient centred at (32,32) inner_r=0
    // outer_r=32, stops black→white. Center pixel should be
    // dark (t near 0 = first stop = black); border pixel should
    // be near-white.
    use crate::kms::vk::gradient::Stop;
    let Some(mut platform) = live_platform() else {
        eprintln!("no Vk — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");

    let dst = alloc_filled_pixmap(
        &mut platform,
        &mut store,
        &mut engine,
        0x1,
        64,
        64,
        [0.5, 0.5, 0.5, 1.0],
    );

    let grad_xid = 0xDEAD_BEEF_u32;
    engine
        .build_and_insert_radial_gradient(
            &mut platform,
            grad_xid,
            (32_i32 << 16, 32_i32 << 16, 0),
            (32_i32 << 16, 32_i32 << 16, 32_i32 << 16),
            &[
                Stop {
                    pos: 0,
                    r: 0,
                    g: 0,
                    b: 0,
                    a: 0xFFFF,
                },
                // 16.16 fixed-point: 1.0 = 0x10000. See linear-
                // gradient test above for why i32::MAX is wrong.
                Stop {
                    pos: 0x10000,
                    r: 0xFFFF,
                    g: 0xFFFF,
                    b: 0xFFFF,
                    a: 0xFFFF,
                },
            ],
        )
        .expect("build radial");

    let stats = engine
        .render_composite(
            &mut store,
            &mut platform,
            1, // Src
            ResolvedSource::Gradient(grad_xid),
            ResolvedSource::None,
            Dst::server_internal(dst),
            &[full_rect(64, 64)],
            None,
            Repeat::None,
            Repeat::None,
            None,
            None,
            false,
            0,
            0,
            0,
        )
        .expect("render_composite radial");
    assert_eq!(stats.recorded_draws, 1);

    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(dst),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 64,
                    height: 64,
                },
            },
            32,
        )
        .expect("get_image");

    let bgra = |x: usize, y: usize| {
        let off = (y * 64 + x) * 4;
        (out[off], out[off + 1], out[off + 2], out[off + 3])
    };
    // Centre near-black, edge near-white.
    let (bc, gc, rc, _ac) = bgra(32, 32);
    assert!(
        bc < 0x40 && gc < 0x40 && rc < 0x40,
        "centre BGRA={:?} (expected dark)",
        bgra(32, 32),
    );
    // Corner is outside the unit circle for an inscribed
    // radial — pick a point on the rim instead (x=62, y=32 →
    // r ≈ 30/32).
    let (be, ge, re_, _ae) = bgra(62, 32);
    assert!(
        be > 0xC0 && ge > 0xC0 && re_ > 0xC0,
        "rim BGRA={:?} (expected near-white)",
        bgra(62, 32),
    );

    engine.picture_paint_remove(grad_xid);
    engine.drain_all(&mut platform);
}

/// #214: black → white two-stop ramp over x ∈ [0, 256).
fn bw_ramp_stops() -> [crate::kms::vk::gradient::Stop; 2] {
    use crate::kms::vk::gradient::Stop;
    [
        Stop {
            pos: 0,
            r: 0,
            g: 0,
            b: 0,
            a: 0xFFFF,
        },
        Stop {
            pos: 0x10000,
            r: 0xFFFF,
            g: 0xFFFF,
            b: 0xFFFF,
            a: 0xFFFF,
        },
    ]
}

/// #214: Src-composite gradient `xid` over the 256×1 `dst`, read it
/// back and check the ramp (black at 0, mid-grey at 128, white at
/// 255) — an uninitialized or not-yet-uploaded LUT fails this.
fn composite_and_check_bw_ramp(
    engine: &mut RenderEngine,
    store: &mut DrawableStore,
    platform: &mut PlatformBackend,
    dst: DrawableId,
    xid: u32,
) {
    let stats = engine
        .render_composite(
            store,
            platform,
            1, // Src
            ResolvedSource::Gradient(xid),
            ResolvedSource::None,
            Dst::server_internal(dst),
            &[full_rect(256, 1)],
            None,
            Repeat::None,
            Repeat::None,
            None,
            None,
            false,
            0,
            0,
            0,
        )
        .expect("render_composite gradient");
    assert_eq!(stats.recorded_draws, 1);
    let out = engine
        .get_image(
            store,
            platform,
            Src::server_internal(dst),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 256,
                    height: 1,
                },
            },
            32,
        )
        .expect("get_image");
    let px = |x: usize| [out[x * 4], out[x * 4 + 1], out[x * 4 + 2], out[x * 4 + 3]];
    assert!(px(0)[..3].iter().all(|&c| c <= 4), "x=0 BGRA={:?}", px(0));
    assert!(
        px(255)[..3].iter().all(|&c| c >= 0xF0),
        "x=255 BGRA={:?}",
        px(255)
    );
    assert!(
        px(128)[..3].iter().all(|&c| (0x40..=0xC0).contains(&c)),
        "x=128 BGRA={:?}",
        px(128)
    );
    assert!((0..256).all(|x| px(x)[3] == 0xFF), "alpha must be opaque");
}

fn close_for_tests(
    engine: &mut RenderEngine,
    store: &mut DrawableStore,
    platform: &mut PlatformBackend,
) {
    engine
        .close_open_frame(
            store,
            platform,
            crate::kms::render::frame_builder::CloseReason::Timeout,
        )
        .expect("close frame");
}

/// #214: CreateLinearGradient must not submit + wait; the upload is
/// queued on the open frame. A picture freed before any use stays
/// alive until the frame carrying its upload retires, then releases.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn gradient_create_then_free_before_use_defers_release_to_frame_retire() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no Vk — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");
    let xid = 0x0214_0001_u32;
    engine
        .build_and_insert_linear_gradient(
            &mut platform,
            xid,
            (0, 0),
            (256_i32 << 16, 0),
            &bw_ramp_stops(),
        )
        .expect("build gradient");
    let weak = {
        let inner = engine.inner.as_ref().expect("inner");
        let open = inner
            .frame_builder
            .open
            .as_ref()
            .expect("upload opens a frame");
        assert_eq!(open.gradient_inits.len(), 1, "upload queued, not run");
        assert!(open.ops.is_empty());
        match inner.picture_paint.get(&xid) {
            Some(PicturePaintState::Gradient(g)) => g.downgrade(),
            None => panic!("gradient not registered"),
        }
    };
    engine.picture_paint_remove(xid);
    assert!(
        weak.upgrade().is_some(),
        "image freed while its upload is still unsubmitted"
    );
    close_for_tests(&mut engine, &mut store, &mut platform);
    assert!(
        weak.upgrade().is_some(),
        "image freed while its upload may be in flight"
    );
    engine.drain_all(&mut platform);
    assert!(
        weak.upgrade().is_none(),
        "gradient resources leaked past frame retirement"
    );
}

/// #214: create, composite and free in ONE frame — the upload is
/// emitted at the frame head, ahead of the sampling op.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn gradient_create_composite_free_in_one_frame_renders_and_releases() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no Vk — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");
    let dst = alloc_filled_pixmap(
        &mut platform,
        &mut store,
        &mut engine,
        0x1,
        256,
        1,
        [1.0, 0.0, 0.0, 1.0],
    );
    // The pixmap fill must not share the frame: prove the gradient
    // itself opens (or joins) a frame and is ordered inside it.
    close_for_tests(&mut engine, &mut store, &mut platform);
    let xid = 0x0214_0002_u32;
    engine
        .build_and_insert_linear_gradient(
            &mut platform,
            xid,
            (0, 0),
            (256_i32 << 16, 0),
            &bw_ramp_stops(),
        )
        .expect("build gradient");
    let weak = match engine
        .inner
        .as_ref()
        .expect("inner")
        .picture_paint
        .get(&xid)
    {
        Some(PicturePaintState::Gradient(g)) => g.downgrade(),
        None => panic!("gradient not registered"),
    };
    // Record the composite, then free the picture while the frame is
    // still open; get_image closes and submits that frame.
    let stats = engine
        .render_composite(
            &mut store,
            &mut platform,
            1,
            ResolvedSource::Gradient(xid),
            ResolvedSource::None,
            Dst::server_internal(dst),
            &[full_rect(256, 1)],
            None,
            Repeat::None,
            Repeat::None,
            None,
            None,
            false,
            0,
            0,
            0,
        )
        .expect("render_composite gradient");
    assert_eq!(stats.recorded_draws, 1);
    {
        let open = engine
            .inner
            .as_ref()
            .expect("inner")
            .frame_builder
            .open
            .as_ref()
            .expect("open");
        assert_eq!(open.gradient_inits.len(), 1);
        assert_eq!(open.ops.len(), 1, "composite shares the upload's frame");
    }
    engine.picture_paint_remove(xid);
    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(dst),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 256,
                    height: 1,
                },
            },
            32,
        )
        .expect("get_image");
    let px = |x: usize| [out[x * 4], out[x * 4 + 1], out[x * 4 + 2]];
    assert!(px(0).iter().all(|&c| c <= 4), "x=0 BGR={:?}", px(0));
    assert!(
        px(255).iter().all(|&c| c >= 0xF0),
        "x=255 BGR={:?}",
        px(255)
    );
    assert!(
        px(128).iter().all(|&c| (0x40..=0xC0).contains(&c)),
        "x=128 BGR={:?}",
        px(128)
    );
    engine.drain_all(&mut platform);
    assert!(
        weak.upgrade().is_none(),
        "gradient resources leaked past frame retirement"
    );
}

/// #214: a gradient whose upload frame was already submitted is
/// sampled correctly by a LATER frame (same-queue order + the
/// upload's closing barrier), and many creates stay correct.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn gradient_sampled_in_a_later_frame_sees_the_upload() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no Vk — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");
    let dst = alloc_filled_pixmap(
        &mut platform,
        &mut store,
        &mut engine,
        0x1,
        256,
        1,
        [1.0, 0.0, 0.0, 1.0],
    );
    // A burst of creates in one frame (a GTK repaint), then use one.
    for i in 0..32_u32 {
        engine
            .build_and_insert_linear_gradient(
                &mut platform,
                0x0214_0100 + i,
                (0, 0),
                (256_i32 << 16, 0),
                &bw_ramp_stops(),
            )
            .expect("build gradient");
    }
    close_for_tests(&mut engine, &mut store, &mut platform);
    composite_and_check_bw_ramp(
        &mut engine,
        &mut store,
        &mut platform,
        dst,
        0x0214_0100 + 17,
    );
    for i in 0..32_u32 {
        engine.picture_paint_remove(0x0214_0100 + i);
    }
    assert_eq!(engine.picture_paint_len(), 0);
    engine.drain_all(&mut platform);
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn render_composite_missing_gradient_picture_is_gap() {
    // Engine receives a ResolvedSource::Gradient(xid) for an
    // xid that has no picture_paint entry (LUT build failed or
    // dropped early). Must return stats with recorded_draws=0,
    // log a debug gap, and NOT panic.
    let Some(mut platform) = live_platform() else {
        eprintln!("no Vk — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");

    let dst = alloc_filled_pixmap(
        &mut platform,
        &mut store,
        &mut engine,
        0x1,
        4,
        4,
        [0.0, 0.0, 0.0, 1.0],
    );

    let stats = engine
        .render_composite(
            &mut store,
            &mut platform,
            1, // Src
            ResolvedSource::Gradient(0xC0FF_EE00),
            ResolvedSource::None,
            Dst::server_internal(dst),
            &[full_rect(4, 4)],
            None,
            Repeat::None,
            Repeat::None,
            None,
            None,
            false,
            0,
            0,
            0,
        )
        .expect("render_composite Ok even on missing gradient");
    assert_eq!(stats.recorded_draws, 0);
    engine.drain_all(&mut platform);
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn render_composite_self_alias() {
    // src == dst: pre-fill with a vertical gradient, then
    // Composite(Over, dst, NoMask, dst). Over with itself on
    // opaque alpha yields self exactly (out = src + dst*(1-1) =
    // src). Without the scratch path the GPU samples a region
    // as it writes it — undefined behaviour; with it, the
    // result must be bit-identical to the pre-fill.
    let Some(mut platform) = live_platform() else {
        eprintln!("no Vk — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");

    // Allocate + PutImage a distinct pattern (per-pixel unique).
    let storage = platform.allocate_drawable_storage(8, 4, 32).expect("alloc");
    let dst = store
        .allocate(
            0x1,
            crate::kms::render::store::DrawableKind::Pixmap,
            32,
            false,
            storage,
        )
        .expect("alloc");
    let mut src_bytes = vec![0u8; 8 * 4 * 4];
    for y in 0u8..4 {
        for x in 0u8..8 {
            let off = (usize::from(y) * 8 + usize::from(x)) * 4;
            src_bytes[off] = x * 0x20; // B
            src_bytes[off + 1] = y * 0x40; // G
            src_bytes[off + 2] = (x + y) * 0x10; // R
            src_bytes[off + 3] = 0xFF; // A (opaque)
        }
    }
    engine
        .put_image(
            &mut store,
            &mut platform,
            Dst::server_internal(dst),
            vk::Offset2D::default(),
            vk::Extent2D {
                width: 8,
                height: 4,
            },
            &src_bytes,
            32,
        )
        .expect("put_image");

    engine
        .render_composite(
            &mut store,
            &mut platform,
            3, // Over
            ResolvedSource::Drawable(SourceDrawable::whole(dst)),
            ResolvedSource::None,
            Dst::server_internal(dst),
            &[full_rect(8, 4)],
            None,
            Repeat::None,
            Repeat::None,
            None,
            None,
            false,
            0,
            0,
            0,
        )
        .expect("render_composite");
    // The real contract: Over(self, NoMask, self) on opaque alpha must
    // be bit-identical to self — i.e. the engine must NOT let the GPU
    // sample dst while writing it (read-write hazard → corruption). We
    // assert that on the observable output below rather than on the
    // internal `used_src_alias_scratch` routing flag (an implementation
    // detail of how the hazard is avoided).
    let after = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(dst),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 8,
                    height: 4,
                },
            },
            32,
        )
        .expect("get_image");
    assert_eq!(
        after, src_bytes,
        "Over(self, NoMask, self) must equal self bit-identical",
    );

    engine.drain_all(&mut platform);
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn render_fill_rectangles_src_clears_to_color() {
    // render_fill_rectangles(op=Src, premul colour) — every
    // pixel in the rect must equal the premul colour.
    let Some(mut platform) = live_platform() else {
        eprintln!("no Vk — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");

    let dst = alloc_filled_pixmap(
        &mut platform,
        &mut store,
        &mut engine,
        0x1,
        4,
        4,
        [0.0, 0.0, 0.0, 1.0],
    );
    let stats = engine
        .render_fill_rectangles(
            &mut store,
            &mut platform,
            1,                    // Src
            [1.0, 0.0, 0.0, 1.0], // RGBA: opaque red premul
            Dst::server_internal(dst),
            &[full_rect(4, 4)],
            None,
        )
        .expect("render_fill_rectangles");
    assert_eq!(stats.recorded_draws, 1);
    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(dst),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 4,
                    height: 4,
                },
            },
            32,
        )
        .expect("get_image");
    // BGRA: B=0, G=0, R=0xFF, A=0xFF.
    for px in out.chunks_exact(4) {
        assert_eq!(&px[..4], &[0x00, 0x00, 0xFF, 0xFF]);
    }
    engine.drain_all(&mut platform);
}

// ── Stage 3e.2 decoder + degenerate-trap unit tests ─────────

/// Per plan §3e: round-trip a known wire bytestream through
/// the trapezoid decoder. Verifies field offsets + 16.16
/// fixed-point interpretation. Uses the same shape as v1's
/// `try_vk_render_trapezoids_path` (kms/backend.rs:4286)
/// since v2's `render_trapezoids` mirrors that decoder.
#[test]
fn trapezoid_decoder_x11_wire_layout() {
    // Build a single trapezoid wire record: 10 i32 fields, 40
    // bytes. Field order: top, bottom, left_p1.x, left_p1.y,
    // left_p2.x, left_p2.y, right_p1.x, right_p1.y,
    // right_p2.x, right_p2.y. All values are 16.16 fixed-point.
    let mut wire: Vec<u8> = Vec::with_capacity(40);
    let fields: [i32; 10] = [
        0,        // top = 0.0
        10 << 16, // bottom = 10.0
        2 << 16,  // left_p1.x = 2.0
        0,        // left_p1.y = 0.0
        2 << 16,  // left_p2.x = 2.0
        10 << 16, // left_p2.y = 10.0
        8 << 16,  // right_p1.x = 8.0
        0,        // right_p1.y = 0.0
        8 << 16,  // right_p2.x = 8.0
        10 << 16, // right_p2.y = 10.0
    ];
    for v in fields {
        wire.extend_from_slice(&v.to_le_bytes());
    }

    // Decode mirroring the backend's `render_trapezoids` body.
    let chunk: &[u8] = &wire;
    let read_i32 = |o: usize| -> i32 {
        i32::from_le_bytes([chunk[o], chunk[o + 1], chunk[o + 2], chunk[o + 3]])
    };
    let trap = crate::kms::vk::ops::traps::Trapezoid {
        top: read_i32(0),
        bottom: read_i32(4),
        left_p1: (read_i32(8), read_i32(12)),
        left_p2: (read_i32(16), read_i32(20)),
        right_p1: (read_i32(24), read_i32(28)),
        right_p2: (read_i32(32), read_i32(36)),
    };
    assert_eq!(trap.top, 0);
    assert_eq!(trap.bottom, 10 << 16);
    assert_eq!(trap.left_p1, (2 << 16, 0));
    assert_eq!(trap.left_p2, (2 << 16, 10 << 16));
    assert_eq!(trap.right_p1, (8 << 16, 0));
    assert_eq!(trap.right_p2, (8 << 16, 10 << 16));

    // bbox: x ∈ [2, 8], y ∈ [0, 10]; integer = (2, 0, 8, 10).
    let bbox =
        crate::kms::vk::ops::traps::trapezoid_bbox(&[trap]).expect("bbox for non-degenerate trap");
    assert_eq!(bbox, (2, 0, 8, 10));
}

/// Per plan §3e: each Triangle's three vertices round-trip
/// through the wire decoder, and the bbox helper hits each
/// vertex (so a degenerate triangle — three colinear points —
/// still produces a finite bbox if the points span pixels).
/// Mirrors v1's `try_vk_render_triangles_path` decoder shape.
#[test]
fn triangle_to_trap_degenerate() {
    let tri = crate::kms::vk::ops::traps::Triangle {
        p1: (0, 0),
        p2: (4 << 16, 0),
        p3: (2 << 16, 8 << 16),
    };
    let inst = tri.to_instance_data();
    assert!((inst.p1[0] - 0.0).abs() < 1e-6);
    assert!((inst.p2[0] - 4.0).abs() < 1e-6);
    assert!((inst.p3[1] - 8.0).abs() < 1e-6);
    let bbox = crate::kms::vk::ops::traps::triangle_bbox(&[tri])
        .expect("bbox for non-degenerate triangle");
    assert_eq!(bbox, (0, 0, 4, 8));

    // Degenerate (three colinear points) — bbox helper still
    // returns Some(extents) because the points span the axes.
    // What v1 + v2 do with such an input is: GPU pipeline draws
    // a zero-area triangle (no pixels covered), CB safely
    // completes. The plan's "degenerate trap" phrasing refers
    // to the encoding (trap with one zero-length edge), not a
    // helper output — the test confirms the trivial bbox path
    // doesn't choke on it.
    let colinear = crate::kms::vk::ops::traps::Triangle {
        p1: (0, 0),
        p2: (4 << 16, 0),
        p3: (8 << 16, 0),
    };
    assert!(crate::kms::vk::ops::traps::triangle_bbox(&[colinear]).is_none());
}

/// Stage 3f.15: `fill_rect_batch` records N rects into ONE CB +
/// ONE submit + ONE `SubmittedOp`. Drives 3 disjoint rects on a
/// 16×4 BGRA8 dst pre-cleared to blue, fills them red, and
/// asserts (a) the dst observes red inside each rect and blue
/// outside, and (b) `inner.submitted` grew by exactly 1 across the
/// two fill calls (blue-prefill + red-batch) after the frame closes.
///
/// Phase B.3 update: fill_rect / fill_rect_batch now append to the
/// open frame instead of submitting immediately. The count assertion
/// is now gated on closing the frame first (via
/// `close_open_frame_for_timeout_for_tests`), then asserting submitted
/// grew by the expected count. The pixel-correctness assertions are
/// unchanged — `get_image` closes any open frame internally (via
/// `close_open_frame(SyncWait)`) so they still observe all fills.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn fill_rect_batch_one_submit_for_n_rects() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no VkContext available — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");

    let storage = platform
        .allocate_drawable_storage(16, 4, 32)
        .expect("alloc");
    let id = store
        .allocate(
            0x1,
            crate::kms::render::store::DrawableKind::Pixmap,
            32,
            false,
            storage,
        )
        .unwrap();

    // Pre-fill the dst with blue so we can see the batch-painted
    // rects against a known background. Phase B.3: this now appends
    // to the open frame instead of submitting a per-op CB.
    let blue = decode_x11_pixel_bgra(0xFF_00_00_FF);
    engine
        .fill_rect(
            &mut store,
            &mut platform,
            Dst::server_internal(id),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 16,
                    height: 4,
                },
            },
            blue,
        )
        .expect("blue prefill");

    // Close the blue-prefill frame so the red-batch starts in a
    // fresh frame. This mirrors the production sequence where
    // fill_rect is followed by a different op that closes the frame.
    // After close + flush, the blue-prefill SubmittedOp is in submitted.
    engine
        .close_open_frame_for_timeout_for_tests(&mut store, &mut platform)
        .expect("close blue-prefill frame");
    engine
        .flush_submit_group(
            &mut store,
            &mut platform,
            crate::kms::render::submit_group::FlushReason::SyncBoundary,
        )
        .expect("setup flush");

    // Snapshot the SubmittedOp count BEFORE the red batch so we
    // can assert exactly +1 (the red frame) across the call.
    let before = engine
        .inner
        .as_ref()
        .map(|i| i.submitted.len())
        .unwrap_or(0);

    let red = decode_x11_pixel_bgra(0xFF_FF_00_00);
    let rects = [
        vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: vk::Extent2D {
                width: 2,
                height: 2,
            },
        },
        vk::Rect2D {
            offset: vk::Offset2D { x: 6, y: 1 },
            extent: vk::Extent2D {
                width: 3,
                height: 2,
            },
        },
        vk::Rect2D {
            offset: vk::Offset2D { x: 13, y: 2 },
            extent: vk::Extent2D {
                width: 3,
                height: 2,
            },
        },
    ];
    engine
        .fill_rect_batch(
            &mut store,
            &mut platform,
            Dst::server_internal(id),
            red,
            &rects,
        )
        .expect("fill_rect_batch");

    // Phase B.3: close the open frame (red batch) before asserting
    // the SubmittedOp count — the op is now frame-resident until close.
    engine
        .close_open_frame_for_timeout_for_tests(&mut store, &mut platform)
        .expect("close red-batch frame");
    engine
        .flush_submit_group(
            &mut store,
            &mut platform,
            crate::kms::render::submit_group::FlushReason::SyncBoundary,
        )
        .expect("flush before count assertion");

    let after = engine
        .inner
        .as_ref()
        .map(|i| i.submitted.len())
        .unwrap_or(0);
    assert_eq!(
        after,
        before + 1,
        "fill_rect_batch (red rects) must produce exactly ONE SubmittedOp \
             regardless of rect count — N4 invariant (before={before}, after={after})"
    );

    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(id),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 16,
                    height: 4,
                },
            },
            32,
        )
        .expect("get_image");

    // Helper: does (x, y) fall inside any of the painted rects?
    let in_rect = |x: i32, y: i32| -> bool {
        rects.iter().any(|r| {
            x >= r.offset.x
                && y >= r.offset.y
                && x < r.offset.x + r.extent.width as i32
                && y < r.offset.y + r.extent.height as i32
        })
    };
    for y in 0..4 {
        for x in 0..16 {
            let off = (y * 16 + x) as usize * 4;
            let px = &out[off..off + 4];
            if in_rect(x, y) {
                assert_eq!(px[2], 0xFF, "rect pixel ({x},{y}) R should be 0xFF (red)");
                assert_eq!(px[0], 0x00, "rect pixel ({x},{y}) B should be 0x00");
            } else {
                assert_eq!(
                    px[0], 0xFF,
                    "background pixel ({x},{y}) B should be 0xFF (blue)"
                );
                assert_eq!(px[2], 0x00, "background pixel ({x},{y}) R should be 0x00");
            }
        }
    }

    engine.drain_all(&mut platform);
}

/// X11 Render PictFormat fix — resolver-level oracle.
///
/// Per the X11 Render spec, a Picture wrapping a depth-24
/// drawable has `PictFormat.alpha_mask = 0`; samples must
/// return α = 1.0 regardless of the storage's padding byte.
/// `resolve_force_opaque` is the single point where v2's
/// `render_composite` and `render_traps_or_tris` decide
/// whether to set the shader-side force-opaque bit on the
/// src/mask picture.
///
/// This test is the logic-only gate: a depth-24 Drawable
/// must resolve to `true`; depth-32 to `false`. Solid and
/// Gradient sources carry α intrinsically (LUT-baked or
/// caller-supplied), so they're always `false`. `None` is
/// the synthetic white-mask path — `α = 1.0` already by
/// construction, so no override needed.
#[test]
fn render_composite_resolve_force_opaque_oracle() {
    let mut store = DrawableStore::new();
    let storage32 = crate::kms::render::store::Storage::for_tests_null(
        vk::Extent2D {
            width: 4,
            height: 4,
        },
        vk::Format::B8G8R8A8_UNORM,
    );
    let id32 = store
        .allocate(
            0xA001,
            crate::kms::render::store::DrawableKind::Pixmap,
            32,
            false,
            storage32,
        )
        .unwrap();
    let storage24 = crate::kms::render::store::Storage::for_tests_null(
        vk::Extent2D {
            width: 4,
            height: 4,
        },
        vk::Format::B8G8R8A8_UNORM,
    );
    let id24 = store
        .allocate(
            0xA002,
            crate::kms::render::store::DrawableKind::Pixmap,
            24,
            false,
            storage24,
        )
        .unwrap();

    // depth-32 Drawable: storage's α byte is client-meaningful,
    // do not force.
    assert!(!resolve_force_opaque(
        &store,
        &ResolvedSource::Drawable(SourceDrawable::whole(id32))
    ));
    // depth-24 Drawable: storage's α byte is server-owned
    // padding, force α = 1.0.
    assert!(resolve_force_opaque(
        &store,
        &ResolvedSource::Drawable(SourceDrawable::whole(id24))
    ));

    // Solid: α is caller-supplied premul. Gradient: α is
    // LUT-baked. None: white-mask scratch is initialised to
    // α = 1.0 at engine init. All three pass through.
    assert!(!resolve_force_opaque(
        &store,
        &ResolvedSource::Solid([1.0, 0.0, 0.0, 1.0]),
    ));
    assert!(!resolve_force_opaque(
        &store,
        &ResolvedSource::Gradient(0x1234)
    ));
    assert!(!resolve_force_opaque(&store, &ResolvedSource::None));

    // depth-1 (bitmap mask) and depth-8 (a8 alpha picture)
    // both have meaningful α in their PictFormat — α carries
    // the bitmap value / coverage. Forcing α = 1.0 on those
    // would turn coverage masks into solid blocks, so the
    // resolver explicitly excludes them. Only depth-24 (the
    // x8r8g8b8 / r8g8b8 case where storage's α byte is
    // server-owned padding) gets the override.
    let storage1 = crate::kms::render::store::Storage::for_tests_null(
        vk::Extent2D {
            width: 4,
            height: 4,
        },
        vk::Format::B8G8R8A8_UNORM,
    );
    let id1 = store
        .allocate(
            0xA003,
            crate::kms::render::store::DrawableKind::Pixmap,
            1,
            false,
            storage1,
        )
        .unwrap();
    assert!(!resolve_force_opaque(
        &store,
        &ResolvedSource::Drawable(SourceDrawable::whole(id1))
    ));
    let storage8 = crate::kms::render::store::Storage::for_tests_null(
        vk::Extent2D {
            width: 4,
            height: 4,
        },
        vk::Format::R8_UNORM,
    );
    let id8 = store
        .allocate(
            0xA004,
            crate::kms::render::store::DrawableKind::Pixmap,
            8,
            false,
            storage8,
        )
        .unwrap();
    assert!(!resolve_force_opaque(
        &store,
        &ResolvedSource::Drawable(SourceDrawable::whole(id8))
    ));
}

/// Audit #4 (2026-05-19) — `pict_format` overrides the depth
/// heuristic for `Drawable` sources. A picture wrapping a
/// depth-32 storage with `RENDER_FMT_XRGB32` declares
/// `alpha_mask=0` — the storage's α byte is padding, not
/// client-meaningful. Engine must force α=1 even though
/// `d.depth == 32`. Pre-fix `resolve_force_opaque` ignored
/// pict_format → depth-32 storages with xRGB32 sampled as
/// transparent black against the wallpaper.
#[test]
fn render_composite_resolve_force_opaque_honors_xrgb32_pict_format() {
    use yserver_protocol::x11::{RENDER_FMT_ARGB32, RENDER_FMT_RGB24, RENDER_FMT_XRGB32};

    let mut store = DrawableStore::new();
    // Depth-32 storage (would normally sample with real α).
    let storage32 = crate::kms::render::store::Storage::for_tests_null(
        vk::Extent2D {
            width: 4,
            height: 4,
        },
        vk::Format::B8G8R8A8_UNORM,
    );
    let id32 = store
        .allocate(
            0xA101,
            crate::kms::render::store::DrawableKind::Pixmap,
            32,
            false,
            storage32,
        )
        .unwrap();
    // Depth-24 storage (α is padding regardless of pict_format).
    let storage24 = crate::kms::render::store::Storage::for_tests_null(
        vk::Extent2D {
            width: 4,
            height: 4,
        },
        vk::Format::B8G8R8A8_UNORM,
    );
    let id24 = store
        .allocate(
            0xA102,
            crate::kms::render::store::DrawableKind::Pixmap,
            24,
            false,
            storage24,
        )
        .unwrap();
    let src32 = ResolvedSource::Drawable(SourceDrawable::whole(id32));
    let src24 = ResolvedSource::Drawable(SourceDrawable::whole(id24));

    // pict_format=0 (no picture context) → fall back to depth
    // heuristic (the engine-internal callers that synthesize
    // sources pass 0 here).
    assert!(!resolve_force_opaque_pict_format(&store, &src32, 0));
    assert!(resolve_force_opaque_pict_format(&store, &src24, 0));

    // pict_format=RENDER_FMT_XRGB32 on depth-32 storage → force
    // opaque (the audit-#4 case). Pre-fix would have returned
    // false because depth==32.
    assert!(resolve_force_opaque_pict_format(
        &store,
        &src32,
        RENDER_FMT_XRGB32,
    ));
    // pict_format=RENDER_FMT_ARGB32 on depth-32 storage → use
    // storage α (current behavior preserved).
    assert!(!resolve_force_opaque_pict_format(
        &store,
        &src32,
        RENDER_FMT_ARGB32,
    ));
    // pict_format=RENDER_FMT_RGB24 on depth-24 storage → force
    // opaque (consistent with the legacy depth-24 path).
    assert!(resolve_force_opaque_pict_format(
        &store,
        &src24,
        RENDER_FMT_RGB24,
    ));
}

/// Audit #4 (2026-05-19) — destination `pict_format` overrides
/// the depth-32 storage heuristic. A Picture wrapping a
/// depth-32 storage with `RENDER_FMT_XRGB32` declares
/// `alpha_mask = 0` — the dst storage has no client-meaningful
/// alpha channel, padding bytes only. The engine must drive
/// the pipeline + readback selection as "no alpha target,"
/// matching the depth-24 case, otherwise post-composite reads
/// of those padding bytes leak through to subsequent samples
/// as partial transparency. Pre-fix `dst_has_alpha = depth == 32`
/// unconditionally → xRGB32 destination treated as ARGB.
#[test]
fn render_composite_dst_has_alpha_honors_xrgb32_pict_format() {
    use yserver_protocol::x11::{RENDER_FMT_ARGB32, RENDER_FMT_RGB24, RENDER_FMT_XRGB32};

    // pict_format=0 (no picture context — engine-internal callers
    // synthesizing draws) → depth heuristic.
    assert!(!dst_has_alpha_for_pict_format(
        vk::Format::B8G8R8A8_UNORM,
        24,
        0,
    ));
    assert!(dst_has_alpha_for_pict_format(
        vk::Format::B8G8R8A8_UNORM,
        32,
        0,
    ));

    // XRGB32 on depth-32 storage → no alpha (audit #4 case).
    assert!(!dst_has_alpha_for_pict_format(
        vk::Format::B8G8R8A8_UNORM,
        32,
        RENDER_FMT_XRGB32,
    ));
    // ARGB32 on depth-32 storage → use storage alpha
    // (current behavior preserved).
    assert!(dst_has_alpha_for_pict_format(
        vk::Format::B8G8R8A8_UNORM,
        32,
        RENDER_FMT_ARGB32,
    ));
    // RGB24 on depth-24 storage → no alpha (consistent with
    // legacy depth-24 path).
    assert!(!dst_has_alpha_for_pict_format(
        vk::Format::B8G8R8A8_UNORM,
        24,
        RENDER_FMT_RGB24,
    ));
    // R8 storage (A8 mask destination) is alpha-only regardless
    // of pict_format — A8 destinations DO have alpha bytes.
    assert!(dst_has_alpha_for_pict_format(vk::Format::R8_UNORM, 8, 0));
}

/// Audit #4 — `swizzle_class_for` must pick `BgraNoAlpha`
/// (force α=ONE swizzle on the sample view) whenever the
/// picture's PictFormat declares `alpha_mask=0`, not just
/// when `depth == 24`. Pre-fix, depth-32 storages always got
/// `RgbaIdent` (pass-through), so an xRGB32 picture wrapping
/// a depth-32 storage with α=0 padding bytes sampled as
/// transparent.
#[test]
fn render_composite_swizzle_class_for_pict_format_xrgb32_is_no_alpha() {
    use yserver_protocol::x11::{RENDER_FMT_ARGB32, RENDER_FMT_RGB24, RENDER_FMT_XRGB32};

    // pict_format=0 falls back to depth heuristic.
    assert_eq!(
        swizzle_class_for_pict_format(vk::Format::B8G8R8A8_UNORM, 24, 0),
        SwizzleClass::BgraNoAlpha,
    );
    assert_eq!(
        swizzle_class_for_pict_format(vk::Format::B8G8R8A8_UNORM, 32, 0),
        SwizzleClass::RgbaIdent,
    );

    // xRGB32 on depth-32 storage → BgraNoAlpha (force α=ONE).
    assert_eq!(
        swizzle_class_for_pict_format(vk::Format::B8G8R8A8_UNORM, 32, RENDER_FMT_XRGB32,),
        SwizzleClass::BgraNoAlpha,
    );
    // ARGB32 on depth-32 storage → RgbaIdent (use storage α).
    assert_eq!(
        swizzle_class_for_pict_format(vk::Format::B8G8R8A8_UNORM, 32, RENDER_FMT_ARGB32,),
        SwizzleClass::RgbaIdent,
    );
    // RGB24 on depth-24 storage → BgraNoAlpha (already true via
    // depth, preserved when pict_format aligns).
    assert_eq!(
        swizzle_class_for_pict_format(vk::Format::B8G8R8A8_UNORM, 24, RENDER_FMT_RGB24,),
        SwizzleClass::BgraNoAlpha,
    );
    // R8 storage (A8 mask) is alpha-only regardless of pict_format.
    assert_eq!(
        swizzle_class_for_pict_format(vk::Format::R8_UNORM, 8, 0),
        SwizzleClass::AlphaOnlyR8,
    );
}

/// RENDER `Trapezoids`/`Triangles` must honour the client's
/// `xSrc`/`ySrc` source origin when the source is a picture (e.g.
/// GTK CSD shadow blur-ramp masks sampled at `ySrc != 0`). The
/// trap composite hardcoded `src_x/src_y = 0`, collapsing the ramp
/// to a solid slab → opaque black bar below tooltips. This locks
/// the origin convention the emit now applies.
#[test]
fn trap_composite_src_origin_honours_xsrc_ysrc() {
    // full-dst branch (op=Src builds the A8 mask): the coverage
    // mask carries the bbox offset, so the source aligns directly
    // at the shifted client origin (ySrc=25 in the tooltip trace).
    assert_eq!(trap_composite_src_origin_axis(25, 18, true), 25);
    assert_eq!(trap_composite_src_origin_axis(0, 18, true), 0);
    // non-full-dst branch: composite renders at the bbox origin, so
    // the source adds it back (Xorg miTrapezoids: src at xSrc+dst).
    assert_eq!(trap_composite_src_origin_axis(25, 18, false), 43);
    // shifted-negative base (redirect/x_off pushed the origin left)
    // composes linearly with the bbox add.
    assert_eq!(trap_composite_src_origin_axis(-4, 10, false), 6);
    // The pre-fix behaviour (origin always 0) is now only correct
    // for a zero client origin on the full-dst path — proving the
    // hardcoded 0 was wrong for every nonzero xSrc/ySrc.
    assert_ne!(trap_composite_src_origin_axis(25, 18, true), 0);
}
