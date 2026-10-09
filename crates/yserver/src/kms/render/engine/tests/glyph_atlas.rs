use super::*;

#[test]
#[ignore = "needs live Vulkan ICD"]
fn image_text_run_records_damage_on_target() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no VkContext available — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");

    // Window-kind + scene-participating so presentation damage
    // accumulates (per the I5 spec amendment, pixmaps no longer
    // accumulate any damage in the store — protocol DamageNotify
    // fanout lives at the request layer).
    let id = alloc_drawable_3a_with_kind(
        &platform,
        &mut store,
        0x1,
        64,
        32,
        crate::kms::render::store::DrawableKind::Window,
        true,
    );
    // Two glyphs spanning x=[10..22] × y=[5..17].
    let glyphs = vec![
        build_glyph(u32::from(b'A'), 10, 5, 6, 12),
        build_glyph(u32::from(b'B'), 16, 5, 6, 12),
    ];
    let stats = engine
        .image_text(
            &mut store,
            &mut platform,
            Dst::server_internal(id),
            7,
            [1.0, 1.0, 1.0, 1.0],
            &glyphs,
        )
        .expect("image_text");
    assert_eq!(stats.atlas_interns, 2);
    assert_eq!(stats.glyph_uploads, 2);
    assert_eq!(stats.glyphs_dropped, 0);

    // Damage union covers the two glyph quads.
    let d = store.get(id).expect("drawable");
    let rects: Vec<vk::Rect2D> = d.presentation_damage.rects().to_vec();
    assert!(!rects.is_empty(), "presentation damage should be set");
    let mut min_x = i32::MAX;
    let mut min_y = i32::MAX;
    let mut max_x = i32::MIN;
    let mut max_y = i32::MIN;
    for r in rects {
        min_x = min_x.min(r.offset.x);
        min_y = min_y.min(r.offset.y);
        max_x = max_x.max(r.offset.x + r.extent.width as i32);
        max_y = max_y.max(r.offset.y + r.extent.height as i32);
    }
    assert!(min_x <= 10);
    assert!(min_y <= 5);
    assert!(max_x >= 22);
    assert!(max_y >= 17);

    engine.drain_all(&mut platform);
}

/// **Load-bearing per codex round 1**: two back-to-back glyph
/// uploads with distinct keys must not corrupt each other's
/// atlas pixels. v1's shared persistent staging would clobber
/// A when B's memcpy lands while A's GPU read is in flight; the
/// v2 per-upload arena slice rules that out.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn atlas_back_to_back_upload_no_corruption() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no VkContext available — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");
    let target = alloc_drawable_3a(&platform, &mut store, 0x1, 32, 32);

    // Pre-clear the target to black.
    engine
        .fill_rect(
            &mut store,
            &mut platform,
            Dst::server_internal(target),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 32,
                    height: 32,
                },
            },
            [0.0, 0.0, 0.0, 1.0],
        )
        .expect("clear");

    // Two glyphs with distinguishable solid-alpha rectangles.
    // The text shader does `foreground × atlas.r`; with
    // 0xFF-filled atlas and white foreground, the dst quads
    // come out (B=0xFF, G=0xFF, R=0xFF, A=0xFF).
    let glyphs = vec![
        build_glyph(u32::from(b'A'), 1, 1, 4, 4),
        build_glyph(u32::from(b'B'), 10, 1, 4, 4),
    ];
    let stats = engine
        .image_text(
            &mut store,
            &mut platform,
            Dst::server_internal(target),
            42,
            [1.0, 1.0, 1.0, 1.0],
            &glyphs,
        )
        .expect("image_text");
    assert_eq!(stats.atlas_interns, 2);

    // Read back: both quads should be white; pixels between
    // them should be the original black.
    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(target),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 32,
                    height: 32,
                },
            },
            32,
        )
        .expect("get_image");
    let pixel_at = |x: usize, y: usize| {
        let off = (y * 32 + x) * 4;
        (out[off], out[off + 1], out[off + 2], out[off + 3])
    };
    // A's quad: (1..5, 1..5).
    for y in 1..5 {
        for x in 1..5 {
            let (b, g, r, _a) = pixel_at(x, y);
            assert_eq!(
                (b, g, r),
                (0xFF, 0xFF, 0xFF),
                "glyph A quad pixel ({x},{y}) corrupted: ({b:#x},{g:#x},{r:#x})",
            );
        }
    }
    // B's quad: (10..14, 1..5).
    for y in 1..5 {
        for x in 10..14 {
            let (b, g, r, _a) = pixel_at(x, y);
            assert_eq!(
                (b, g, r),
                (0xFF, 0xFF, 0xFF),
                "glyph B quad pixel ({x},{y}) corrupted: ({b:#x},{g:#x},{r:#x})",
            );
        }
    }
    // Between the quads (7, 2) should still be black.
    let (b, g, r, _a) = pixel_at(7, 2);
    assert_eq!(
        (b, g, r),
        (0x00, 0x00, 0x00),
        "between-quad pixel (7,2) should be background black; got ({b:#x},{g:#x},{r:#x})"
    );

    engine.drain_all(&mut platform);
}

fn atlas_resets(engine: &RenderEngine) -> u64 {
    engine
        .inner
        .as_ref()
        .and_then(|i| i.glyph_atlas.as_ref())
        .map_or(0, GlyphAtlas::resets)
}

fn atlas_has(engine: &RenderEngine, font_xid: u32, codepoint: u32) -> bool {
    engine
        .inner
        .as_ref()
        .and_then(|i| i.glyph_atlas.as_ref())
        .and_then(|a| {
            a.lookup(GlyphKey {
                font_xid,
                codepoint,
            })
        })
        .is_some()
}

/// A `w × h` glyph whose coverage is 0xFF in columns `cols` of its
/// top four rows and 0 everywhere else.
fn corner_glyph(
    codepoint: u32,
    dst_x: i32,
    dst_y: i32,
    w: usize,
    h: usize,
    cols: std::ops::Range<usize>,
) -> PreparedGlyph {
    let mut g = build_glyph(codepoint, dst_x, dst_y, w, h);
    g.pixels.fill(0);
    for y in 0..4 {
        for x in cols.clone() {
            g.pixels[y * w + x] = 0xFF;
        }
    }
    g
}

/// Two 2049² glyphs cannot share the 4096² atlas. The second draw
/// arrives while the first one's upload and draw are still only
/// recorded in the open frame: the atlas must close that frame,
/// reset, and hand the second glyph the slot the first one used —
/// and both draws must still show their own glyph.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn atlas_full_resets_behind_the_recorded_draw() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no VkContext available — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");
    let target = alloc_drawable_3a(&platform, &mut store, 0x1, 32, 8);
    let full = vk::Rect2D {
        offset: vk::Offset2D::default(),
        extent: vk::Extent2D {
            width: 32,
            height: 8,
        },
    };
    engine
        .fill_rect(
            &mut store,
            &mut platform,
            Dst::server_internal(target),
            full,
            [0.0, 0.0, 0.0, 1.0],
        )
        .expect("clear");

    // Covered: g0 columns 0..4 at x 1..5; g1 columns 4..8 at x 16..20.
    let g0 = corner_glyph(1, 1, 1, 2049, 2049, 0..4);
    let g1 = corner_glyph(2, 12, 1, 2049, 2049, 4..8);
    let stats = engine
        .image_text(
            &mut store,
            &mut platform,
            Dst::server_internal(target),
            1,
            [1.0, 1.0, 1.0, 1.0],
            &[g0],
        )
        .expect("first image_text");
    assert_eq!((stats.atlas_interns, stats.glyphs_dropped), (1, 0));
    assert!(
        engine
            .inner
            .as_ref()
            .expect("inner")
            .frame_builder
            .is_open(),
        "the first draw must still be unsubmitted for this test to mean anything",
    );

    let stats = engine
        .image_text(
            &mut store,
            &mut platform,
            Dst::server_internal(target),
            1,
            [1.0, 1.0, 1.0, 1.0],
            &[g1],
        )
        .expect("second image_text");
    assert_eq!((stats.atlas_interns, stats.glyphs_dropped), (1, 0));
    assert_eq!(atlas_resets(&engine), 1);
    assert!(
        !atlas_has(&engine, 1, 1),
        "the reset dropped the first glyph"
    );
    let pending = engine
        .inner
        .as_ref()
        .and_then(|i| i.frame_builder.open.as_ref())
        .map(|o| o.pending_glyph_inserts.entries.clone())
        .expect("the second draw opened a new frame");
    assert_eq!(pending.len(), 1);
    assert_eq!(
        (pending[0].1.atlas_x, pending[0].1.atlas_y),
        (0, 0),
        "the second glyph reuses the first one's slot",
    );

    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(target),
            full,
            32,
        )
        .expect("get_image");
    for y in 0..8 {
        for x in 0..32 {
            let want = (1..5).contains(&y) && ((1..5).contains(&x) || (16..20).contains(&x));
            let off = (y * 32 + x) * 4;
            let px = (out[off], out[off + 1], out[off + 2]);
            let expect = if want { (0xFF, 0xFF, 0xFF) } else { (0, 0, 0) };
            assert_eq!(px, expect, "pixel ({x},{y})");
        }
    }
    engine.drain_all(&mut platform);
}

/// A glyph wider than the whole atlas can never be placed: it
/// drops (rate-limited warning) and must NOT empty the atlas.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn glyph_larger_than_atlas_drops_without_reset() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no VkContext available — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");
    let target = alloc_drawable_3a(&platform, &mut store, 0x1, 4, 4);
    let small = build_glyph(1, 0, 0, 4, 4);
    let huge = build_glyph(2, 0, 0, 4097, 1);
    let stats = engine
        .image_text(
            &mut store,
            &mut platform,
            Dst::server_internal(target),
            1,
            [1.0, 1.0, 1.0, 1.0],
            &[small, huge],
        )
        .expect("image_text");
    assert_eq!((stats.atlas_interns, stats.glyphs_dropped), (1, 1));
    assert_eq!(atlas_resets(&engine), 0);
    engine
        .close_open_frame(
            &mut store,
            &mut platform,
            crate::kms::render::frame_builder::CloseReason::SyncWait,
        )
        .expect("close");
    assert!(atlas_has(&engine, 1, 1));
    engine.drain_all(&mut platform);
}

/// `forget_glyphs` drops committed entries AND inserts still
/// pending in the open frame, so a redefined glyph id cannot be
/// served its old image once that frame commits.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn forget_glyphs_drops_committed_and_pending_entries() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no VkContext available — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");
    let target = alloc_drawable_3a(&platform, &mut store, 0x1, 16, 4);
    let close =
        |engine: &mut RenderEngine, store: &mut DrawableStore, platform: &mut PlatformBackend| {
            engine
                .close_open_frame(
                    store,
                    platform,
                    crate::kms::render::frame_builder::CloseReason::SyncWait,
                )
                .expect("close");
        };
    let draw = |engine: &mut RenderEngine,
                store: &mut DrawableStore,
                platform: &mut PlatformBackend,
                font: u32| {
        let glyphs = [build_glyph(1, 0, 0, 2, 2), build_glyph(2, 4, 0, 2, 2)];
        engine
            .image_text(
                store,
                platform,
                Dst::server_internal(target),
                font,
                [1.0, 1.0, 1.0, 1.0],
                &glyphs,
            )
            .expect("image_text");
    };
    // Committed: font 10's glyphs land in the atlas, then go.
    draw(&mut engine, &mut store, &mut platform, 10);
    close(&mut engine, &mut store, &mut platform);
    assert!(atlas_has(&engine, 10, 1) && atlas_has(&engine, 10, 2));
    engine.forget_glyphs(10, Some(&[1]));
    assert!(!atlas_has(&engine, 10, 1));
    assert!(atlas_has(&engine, 10, 2), "only the named id goes");
    engine.forget_glyphs(10, None);
    assert!(!atlas_has(&engine, 10, 2));

    // Pending: font 11's inserts are still in the open frame.
    draw(&mut engine, &mut store, &mut platform, 11);
    engine.forget_glyphs(11, Some(&[2]));
    close(&mut engine, &mut store, &mut platform);
    assert!(atlas_has(&engine, 11, 1));
    assert!(
        !atlas_has(&engine, 11, 2),
        "a forgotten pending insert never commits"
    );
    engine.drain_all(&mut platform);
}

/// Step 2 proof (component-alpha glyphs plan,
/// `docs/superpowers/plans/2026-09-10-component-alpha-glyphs-plan.md`):
/// `AtlasEntry.w` split into `packed_w` (atlas footprint) and
/// `logical_w` (the glyph's own size). While the two agree, either
/// field paints identical pixels, so a green suite says nothing
/// about which one a given consumer actually reads — this test
/// manufactures a cache entry where they DISAGREE and checks each
/// consumer against the correct one, through the real
/// `image_text` code path (not a reimplementation of it).
///
/// A first glyph is uploaded for real at packed_w == logical_w ==
/// 40, with its 40 texels spatially varying — left half (cols
/// 0..20) opaque, right half (20..40) transparent — so the atlas
/// holds content a wrong-width sample would visibly disagree with.
/// Its cache entry is then overwritten in place (same atlas slot,
/// same uploaded pixels) with `logical_w` shrunk to 10 while
/// `packed_w` stays 40. A second `image_text` call at the SAME
/// glyph key is then a committed cache hit: it never re-uploads,
/// so every downstream value comes from the (now-asymmetric)
/// `AtlasEntry`, not from anything this test computes itself.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn atlas_entry_packed_vs_logical_width_feed_the_right_consumers() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no VkContext available — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");

    let real_target = alloc_drawable_3a(&platform, &mut store, 0x1, 64, 32);
    // Window + scene-participating so presentation damage
    // accumulates (mirrors `image_text_run_records_damage_on_target`).
    let probe_target = alloc_drawable_3a_with_kind(
        &platform,
        &mut store,
        0x2,
        64,
        32,
        crate::kms::render::store::DrawableKind::Window,
        true,
    );

    // Pre-clear the probe target to black so a painted quad — or
    // the absence of one — is unambiguous on readback.
    engine
        .fill_rect(
            &mut store,
            &mut platform,
            Dst::server_internal(probe_target),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 64,
                    height: 32,
                },
            },
            [0.0, 0.0, 0.0, 1.0],
        )
        .expect("clear");

    // Real 40×20 upload: left half (texels 0..20) opaque, right
    // half (20..40) transparent.
    let font_xid = 4242;
    let codepoint = u32::from(b'Z');
    let (w, h) = (40usize, 20usize);
    let mut pixels = vec![0u8; w * h];
    for row in 0..h {
        for col in 0..20 {
            pixels[row * w + col] = 0xFF;
        }
    }
    let real_glyph = PreparedGlyph {
        dst_x: 0,
        dst_y: 0,
        w,
        h,
        pixels,
        codepoint,
    };
    let stats = engine
        .image_text(
            &mut store,
            &mut platform,
            Dst::server_internal(real_target),
            font_xid,
            [1.0, 1.0, 1.0, 1.0],
            &[real_glyph],
        )
        .expect("image_text (real upload)");
    assert_eq!(stats.atlas_interns, 1);
    assert_eq!(stats.glyph_uploads, 1);

    // The glyph insert is transactional — pending until the frame
    // closes (`commit_close_success`). Close it now so the entry
    // is actually in `glyph_atlas`'s cache before we read it back.
    engine
        .close_open_frame(
            &mut store,
            &mut platform,
            crate::kms::render::frame_builder::CloseReason::SyncWait,
        )
        .expect("close frame after real upload");

    // Overwrite the cached entry: same atlas slot (same uploaded
    // pixels), but logical_w now disagrees with packed_w.
    let key = GlyphKey {
        font_xid,
        codepoint,
    };
    let inner = engine.inner.as_mut().expect("inner");
    let atlas = inner
        .glyph_atlas
        .as_mut()
        .expect("atlas init by first call");
    let real_entry = atlas.lookup(key).expect("entry cached by real upload");
    assert_eq!(real_entry.packed_w, 40);
    assert_eq!(real_entry.logical_w, 40);
    let asymmetric_entry = AtlasEntry {
        packed_w: 40,
        logical_w: 10,
        ..real_entry
    };
    atlas.insert_entry(key, asymmetric_entry);

    // Second call, same key: a committed hit. dst_x/dst_y/w/h/pixels
    // on this input glyph are irrelevant on the hit path — only the
    // cached entry's fields drive geometry — so they're placeholders.
    let probe_glyph = PreparedGlyph {
        dst_x: 5,
        dst_y: 5,
        w: 1,
        h: 1,
        pixels: vec![0u8; 1],
        codepoint,
    };
    let stats2 = engine
        .image_text(
            &mut store,
            &mut platform,
            Dst::server_internal(probe_target),
            font_xid,
            [1.0, 1.0, 1.0, 1.0],
            &[probe_glyph],
        )
        .expect("image_text (cache hit)");
    assert_eq!(
        stats2.atlas_interns, 0,
        "must be a cache hit, not a re-upload, or this proves nothing"
    );
    assert_eq!(stats2.glyph_uploads, 0);

    // (1) Damage extent: the append-time damage union must use
    // logical_w (10), not packed_w (40).
    let d = store.get(probe_target).expect("drawable");
    let rects: Vec<vk::Rect2D> = d.presentation_damage.rects().to_vec();
    let probe_rect = rects
        .iter()
        .find(|r| r.offset.x == 5 && r.offset.y == 5)
        .unwrap_or_else(|| panic!("no damage rect at (5,5): {rects:?}"));
    assert_eq!(
        probe_rect.extent.width, 10,
        "damage extent used packed_w (40) instead of logical_w (10)"
    );
    assert_eq!(probe_rect.extent.height, 20);

    // (2) Instance geometry: the dst quad is logical_w (10) wide,
    // and its atlas UV span must ALSO be logical_w wide (never the
    // packed footprint) — so it samples only texels 0..10, a
    // subset of the real upload's opaque 0..20, and paints fully
    // opaque white. Had the instance geometry used packed_w (40)
    // for the atlas span instead, the 10-pixel-wide quad would
    // stretch across all 40 texels and its right half would land
    // on the transparent texels 20..40, producing a visibly mixed
    // opaque/transparent pattern instead of a solid one.
    engine.drain_all(&mut platform);
    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(probe_target),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 64,
                    height: 32,
                },
            },
            32,
        )
        .expect("get_image");
    let pixel_at = |x: usize, y: usize| {
        let off = (y * 64 + x) * 4;
        (out[off], out[off + 1], out[off + 2], out[off + 3])
    };
    for y in 5..25 {
        for x in 5..15 {
            let (b, g, r, _a) = pixel_at(x, y);
            assert_eq!(
                (b, g, r),
                (0xFF, 0xFF, 0xFF),
                "probe quad pixel ({x},{y}) not fully opaque — instance geometry likely \
                     sampled packed_w's atlas span instead of logical_w's: \
                     ({b:#x},{g:#x},{r:#x})",
            );
        }
    }
}
