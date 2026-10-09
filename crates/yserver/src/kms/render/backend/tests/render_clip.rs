use super::*;
#[test]
fn trap_bbox_shrinks_to_the_clip_extents() {
    use crate::kms::render::backend::{Rectangle16, clip_trap_bbox_to_extents};
    let r = |x, y, width, height| Rectangle16 {
        x,
        y,
        width,
        height,
    };
    // Union bbox far larger than a 100×50 dst: only the dst part stays.
    assert_eq!(
        clip_trap_bbox_to_extents((0, 0, 2400, 1800), &[r(0, 0, 100, 50)]),
        Some((0, 0, 100, 50))
    );
    // Extents of a split clip, not just its first rect.
    assert_eq!(
        clip_trap_bbox_to_extents((10, 10, 300, 300), &[r(0, 0, 40, 20), r(200, 100, 50, 400)]),
        Some((10, 10, 240, 290))
    );
    // Disjoint or empty clip: nothing to draw.
    assert_eq!(
        clip_trap_bbox_to_extents((0, 0, 10, 10), &[r(20, 20, 5, 5)]),
        None
    );
    assert_eq!(
        clip_trap_bbox_to_extents((0, 0, 10, 10), &[r(0, 0, 0, 5)]),
        None
    );
    assert_eq!(clip_trap_bbox_to_extents((0, 0, 10, 10), &[]), None);
}

#[test]
fn clip_fill_rects_by_subwindow_mode_subtracts_mapped_child() {
    let mut b = KmsBackend::for_tests();
    let _parent = seed_window(&mut b, 0x100, None, 0, 0);
    let _child = seed_window(&mut b, 0x200, Some(0x100), 10, 20);
    let child = b.windows.get_mut(&0x200).expect("child geom");
    child.width = 15;
    child.height = 10;
    b.core.current_subwindow_mode = yserver_core::backend::SubwindowMode::ClipByChildren;

    let out = b.clip_fill_rects_by_subwindow_mode(
        0x100,
        &[Rectangle16 {
            x: 0,
            y: 0,
            width: 40,
            height: 40,
        }],
    );
    let got: std::collections::BTreeSet<(i16, i16, u16, u16)> = out
        .into_iter()
        .map(|r| (r.x, r.y, r.width, r.height))
        .collect();
    let want = std::collections::BTreeSet::from([
        (0, 0, 40, 20),
        (0, 30, 40, 10),
        (0, 20, 10, 10),
        (25, 20, 15, 10),
    ]);
    assert_eq!(got, want);
}

/// The per-request clip is cached; any change to the window tree
/// (a child moved, here) computes it again.
#[test]
fn clip_fill_rects_by_subwindow_mode_follows_a_moved_child() {
    let mut b = KmsBackend::for_tests();
    let _parent = seed_window(&mut b, 0x100, None, 0, 0);
    let _child = seed_window(&mut b, 0x200, Some(0x100), 10, 20);
    b.core.current_subwindow_mode = yserver_core::backend::SubwindowMode::ClipByChildren;
    let span = [Rectangle16 {
        x: 0,
        y: 25,
        width: 40,
        height: 1,
    }];
    let child = b.windows.get_mut(&0x200).expect("child geom");
    (child.width, child.height) = (15, 10);
    let before = b.clip_fill_rects_by_subwindow_mode(0x100, &span);
    assert_eq!(before.len(), 2, "split around the child: {before:?}");
    b.windows.get_mut(&0x200).expect("child geom").y = 100;
    assert_eq!(
        b.clip_fill_rects_by_subwindow_mode(0x100, &span),
        span.to_vec()
    );
}

#[test]
fn clip_fill_rects_by_subwindow_mode_include_inferiors_is_passthrough() {
    let mut b = KmsBackend::for_tests();
    let _parent = seed_window(&mut b, 0x100, None, 0, 0);
    let _child = seed_window(&mut b, 0x200, Some(0x100), 10, 20);
    b.core.current_subwindow_mode = yserver_core::backend::SubwindowMode::IncludeInferiors;

    let src = [Rectangle16 {
        x: 0,
        y: 0,
        width: 40,
        height: 40,
    }];
    assert_eq!(b.clip_fill_rects_by_subwindow_mode(0x100, &src), src);
}

fn as_set(v: Vec<Rectangle16>) -> std::collections::BTreeSet<(i16, i16, u16, u16)> {
    v.into_iter()
        .map(|r| (r.x, r.y, r.width, r.height))
        .collect()
}

#[test]
fn render_dst_cliplist_subtracts_mapped_child() {
    let mut b = KmsBackend::for_tests();
    let _parent = seed_window(&mut b, 0x100, None, 0, 0);
    let _child = seed_window(&mut b, 0x200, Some(0x100), 10, 20);
    let child = b.windows.get_mut(&0x200).expect("child geom");
    child.width = 15;
    child.height = 10;

    let out =
        b.render_dst_cliplist_local(0x100, true, None, rect(0, 0, 40, 40), rect(0, 0, 40, 40));
    assert_eq!(
        as_set(out),
        std::collections::BTreeSet::from([
            (0, 0, 40, 20),
            (0, 30, 40, 10),
            (0, 20, 10, 10),
            (25, 20, 15, 10),
        ]),
    );
}

/// #135 — a zero-area Composite must not pay for a source snapshot:
/// acquisition sits in the wrapper, ahead of the inner function's
/// zero-area return, so without this gate a width==0 request did a full
/// scanout readback and a full-screen scratch upload and then returned
/// nothing. Which pictures take one is end to end only (it needs a live
/// scanout or storage): tools/vng-scenarios/draw-clip-probe.c measures
/// the root and a window with children against Xorg.
#[test]
fn a_zero_area_composite_takes_no_source_snapshot() {
    assert!(composite_needs_source_snapshot(8, 8));
    for (w, h) in [(0u16, 8u16), (8, 0), (0, 0)] {
        assert!(
            !composite_needs_source_snapshot(w, h),
            "a {w}x{h} Composite must not acquire the snapshot"
        );
    }
}

#[test]
fn render_dst_cliplist_include_inferiors_keeps_children() {
    let mut b = KmsBackend::for_tests();
    let _parent = seed_window(&mut b, 0x100, None, 0, 0);
    let _child = seed_window(&mut b, 0x200, Some(0x100), 10, 20);
    let child = b.windows.get_mut(&0x200).expect("child geom");
    child.width = 15;
    child.height = 10;

    let out =
        b.render_dst_cliplist_local(0x100, false, None, rect(0, 0, 40, 40), rect(0, 0, 40, 40));
    assert_eq!(
        as_set(out),
        std::collections::BTreeSet::from([(0, 0, 40, 40)])
    );
}

#[test]
fn render_dst_cliplist_skips_manually_redirected_child() {
    let mut b = KmsBackend::for_tests();
    let _parent = seed_window(&mut b, 0x100, None, 0, 0);
    let child_id = seed_window(&mut b, 0x200, Some(0x100), 10, 20);
    {
        let child = b.windows.get_mut(&0x200).expect("child geom");
        child.width = 15;
        child.height = 10;
    }
    b.store.set_scene_participating(child_id, false);

    let out =
        b.render_dst_cliplist_local(0x100, true, None, rect(0, 0, 40, 40), rect(0, 0, 40, 40));
    assert_eq!(
        as_set(out),
        std::collections::BTreeSet::from([(0, 0, 40, 40)])
    );
}

#[test]
fn render_dst_cliplist_intersects_picture_clip_and_op_bbox() {
    let mut b = KmsBackend::for_tests();
    let _parent = seed_window(&mut b, 0x100, None, 0, 0);

    let out = b.render_dst_cliplist_local(
        0x100,
        true,
        Some(&[rect(0, 0, 20, 40)]),
        rect(0, 0, 40, 40),
        rect(5, 5, 30, 30),
    );
    assert_eq!(
        as_set(out),
        std::collections::BTreeSet::from([(5, 5, 15, 30)])
    );
}

#[test]
fn render_dst_cliplist_clamps_op_bbox_to_extent() {
    let mut b = KmsBackend::for_tests();
    let _parent = seed_window(&mut b, 0x100, None, 0, 0);

    let out =
        b.render_dst_cliplist_local(0x100, true, None, rect(0, 0, 40, 40), rect(0, 0, 100, 100));
    assert_eq!(
        as_set(out),
        std::collections::BTreeSet::from([(0, 0, 40, 40)])
    );
}

#[test]
fn render_dst_cliplist_empty_picture_clip_paints_nothing() {
    let mut b = KmsBackend::for_tests();
    let _parent = seed_window(&mut b, 0x100, None, 0, 0);

    let out = b.render_dst_cliplist_local(
        0x100,
        true,
        Some(&[]),
        rect(0, 0, 40, 40),
        rect(0, 0, 40, 40),
    );
    assert!(out.is_empty());
}

#[test]
fn render_dst_cliplist_fully_covered_is_empty() {
    let mut b = KmsBackend::for_tests();
    let _parent = seed_window(&mut b, 0x100, None, 0, 0);
    let _child = seed_window(&mut b, 0x200, Some(0x100), 0, 0);
    let child = b.windows.get_mut(&0x200).expect("child geom");
    child.width = 40;
    child.height = 40;

    let out =
        b.render_dst_cliplist_local(0x100, true, None, rect(0, 0, 40, 40), rect(0, 0, 40, 40));
    assert!(out.is_empty());
}

#[test]
fn dst_picture_clip_by_children_reads_picture_record() {
    let mut b = KmsBackend::for_tests();
    b.core.pictures.insert(
        0xAA01,
        crate::kms::core::PictureRecord::drawable_default(0x100, 0),
    );
    assert!(dst_picture_clip_by_children(&b.core, 0xAA01));

    if let Some(crate::kms::core::PictureRecord::Drawable { subwindow_mode, .. }) =
        b.core.pictures.get_mut(&0xAA01)
    {
        *subwindow_mode = 1;
    }
    assert!(!dst_picture_clip_by_children(&b.core, 0xAA01));
    assert!(dst_picture_clip_by_children(&b.core, 0xDEAD));
}

/// Seed a mapped 100×100 dst window (`0x100`) with a mapped
/// automatic child, plus a `Drawable` dst picture wrapping the
/// window and an opaque-white SolidFill source picture. Returns
/// `(dst_pic_xid, src_pic_xid)`. Child geometry is caller-set after.
#[cfg(test)]
fn seed_render_dst_with_child(
    b: &mut KmsBackend,
    child_x: i16,
    child_y: i16,
    child_w: u16,
    child_h: u16,
) -> (u32, u32) {
    use yserver_core::backend::Backend;
    const DST_PIC_XID: u32 = 0x0000_D001;
    let _parent = seed_window(b, 0x100, None, 0, 0);
    let _child = seed_window(b, 0x200, Some(0x100), child_x, child_y);
    {
        let child = b.windows.get_mut(&0x200).expect("child geom");
        child.width = child_w;
        child.height = child_h;
    }
    b.core.pictures.insert(
        DST_PIC_XID,
        crate::kms::core::PictureRecord::drawable_default(0x100, 0),
    );
    let src_pic = b
        .render_create_solid_fill(None, [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF])
        .expect("solid_fill")
        .expect("Some");
    (DST_PIC_XID, src_pic.as_raw())
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn render_composite_returns_child_clipped_region() {
    use yserver_core::backend::Backend;
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let (dst_pic, src_pic) = seed_render_dst_with_child(&mut b, 0, 0, 40, 40);

    let region = b
        .render_composite(
            None, 1, // Src
            src_pic, 0, dst_pic, 0, 0, 0, 0, 0, 0, 100, 100,
        )
        .expect("render_composite");

    // Window (0,0,100,100) − child (0,0,40,40): bottom strip + the
    // top band's right strip (Xorg band order).
    let got: std::collections::BTreeSet<(i16, i16, u16, u16)> = region
        .into_iter()
        .map(|r| (r.x, r.y, r.width, r.height))
        .collect();
    assert_eq!(
        got,
        std::collections::BTreeSet::from([(0, 40, 100, 60), (40, 0, 60, 40)]),
        "render_composite must return the child-clipped local region",
    );
}

/// Integration guard: real `render_trapezoids` returns the
/// primitive-bbox ∩ (window − child) clipList in local coords.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn render_trapezoids_returns_child_clipped_region() {
    use yserver_core::backend::Backend;
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let (dst_pic, src_pic) = seed_render_dst_with_child(&mut b, 0, 0, 20, 20);

    // One trapezoid = axis-aligned box (0,0)-(40,40), 16.16 fixed.
    let f = |v: i32| (v << 16).to_le_bytes();
    let mut traps = Vec::new();
    for v in [0, 40, 0, 0, 0, 40, 40, 0, 40, 40] {
        traps.extend_from_slice(&f(v));
    }

    let region = b
        .render_trapezoids(None, 3, src_pic, dst_pic, 0, 0, 0, &traps, 0, 0)
        .expect("render_trapezoids");
    let got: std::collections::BTreeSet<(i16, i16, u16, u16)> = region
        .into_iter()
        .map(|r| (r.x, r.y, r.width, r.height))
        .collect();
    // Trap bbox (0,0,40,40) − child (0,0,20,20).
    assert_eq!(
        got,
        std::collections::BTreeSet::from([(0, 20, 40, 20), (20, 0, 20, 20)]),
        "render_trapezoids must return the child-clipped local region",
    );
}

/// Integration guard: real `render_triangles_op` (minor 11) returns
/// the triangle-bbox ∩ (window − child) clipList in local coords.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn render_triangles_returns_child_clipped_region() {
    use yserver_core::backend::Backend;
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let (dst_pic, src_pic) = seed_render_dst_with_child(&mut b, 0, 0, 20, 20);

    // One triangle: (0,0),(40,0),(0,40) — bbox (0,0,40,40). 16.16.
    let f = |v: i32| (v << 16).to_le_bytes();
    let mut prims = Vec::new();
    for (x, y) in [(0, 0), (40, 0), (0, 40)] {
        prims.extend_from_slice(&f(x));
        prims.extend_from_slice(&f(y));
    }

    let region = b
        .render_triangles_op(None, 11, 3, src_pic, dst_pic, 0, 0, 0, &prims, 0, 0)
        .expect("render_triangles_op");
    let got: std::collections::BTreeSet<(i16, i16, u16, u16)> = region
        .into_iter()
        .map(|r| (r.x, r.y, r.width, r.height))
        .collect();
    assert_eq!(
        got,
        std::collections::BTreeSet::from([(0, 20, 40, 20), (20, 0, 20, 20)]),
        "render_triangles_op must return the child-clipped local region",
    );
}

/// Integration guard: real `render_composite_glyphs` returns the
/// rendered-glyph-quad bbox ∩ (window − child) clipList in local
/// coords (a single bbox, matching X.Org `GlyphExtents`).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn render_composite_glyphs_returns_child_clipped_region() {
    use yserver_core::backend::Backend;
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let (dst_pic, src_pic) = seed_render_dst_with_child(&mut b, 0, 0, 10, 10);

    // Glyphset with one 20×20 A8 glyph id=1, zero bearing.
    let gs = b
        .render_create_glyphset(None, yserver_protocol::x11::RENDER_FMT_A8)
        .expect("glyphset")
        .expect("Some");
    let mut add_body: Vec<u8> = Vec::new();
    add_body.extend_from_slice(&1_u32.to_le_bytes()); // n
    add_body.extend_from_slice(&1_u32.to_le_bytes()); // id = 1
    add_body.extend_from_slice(&u16::to_le_bytes(20)); // width
    add_body.extend_from_slice(&u16::to_le_bytes(20)); // height
    add_body.extend_from_slice(&i16::to_le_bytes(0)); // x bearing
    add_body.extend_from_slice(&i16::to_le_bytes(0)); // y bearing
    add_body.extend_from_slice(&i16::to_le_bytes(20)); // x_off
    add_body.extend_from_slice(&i16::to_le_bytes(0)); // y_off
    // A8 stride for w=20: (20+3) & !3 = 20. Pixels = 20×20 = 400.
    add_body.extend_from_slice(&[0xFFu8; 400]);
    b.render_add_glyphs(None, gs.as_raw(), &add_body)
        .expect("add_glyphs");

    // One element: count=1, dx=0, dy=0, id=1 (CompositeGlyphs8).
    let mut items: Vec<u8> = Vec::new();
    items.extend_from_slice(&[1u8, 0, 0, 0]); // count + pad
    items.extend_from_slice(&i16::to_le_bytes(0)); // dx
    items.extend_from_slice(&i16::to_le_bytes(0)); // dy
    items.extend_from_slice(&[1u8, 0, 0, 0]); // id=1 + pad

    let region = b
        .render_composite_glyphs(
            None,
            23,
            3,
            src_pic,
            dst_pic,
            0,
            gs.as_raw(),
            0,
            0,
            &items,
            0,
            0,
        )
        .expect("render_composite_glyphs");
    let got: std::collections::BTreeSet<(i16, i16, u16, u16)> = region
        .into_iter()
        .map(|r| (r.x, r.y, r.width, r.height))
        .collect();
    // Glyph quad (0,0,20,20) − child (0,0,10,10).
    assert_eq!(
        got,
        std::collections::BTreeSet::from([(0, 10, 20, 10), (10, 0, 10, 10)]),
        "render_composite_glyphs must return the child-clipped local region",
    );
}

/// #177: every glyph run and every trapezoid/triangle request used to
/// `vkAllocateMemory` + `vkFreeMemory` its own 1–10 KiB instance/vertex
/// buffer — 85–96% of all alloc/free calls, and on RADV each allocation
/// and free walks libdrm's VA hole list. A text-heavy client issues many
/// such requests per frame (the reporter: glyph_run at 317/s average,
/// peaks of 3654/s, the size-classed pool pinned at its 64-entry cap).
/// With per-frame upload blocks, N requests per frame across many
/// frames must cost O(blocks) allocations — none in the steady state —
/// not O(requests).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn glyph_and_trap_requests_share_upload_blocks_across_frames() {
    use crate::kms::vk::mem_accounting;
    use yserver_core::backend::{AnyHandle, Backend};
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    // A real pixmap destination: the frames below are submitted, so the
    // dst must have storage.
    let pix = b.create_pixmap(None, 32, 100, 100).expect("pixmap");
    let dst_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(pix), 0, 0, &[])
        .expect("create_picture")
        .expect("Some")
        .as_raw();
    let src_pic = b
        .render_create_solid_fill(None, [0xFF; 8])
        .expect("solid_fill")
        .expect("Some")
        .as_raw();

    let gs = b
        .render_create_glyphset(None, yserver_protocol::x11::RENDER_FMT_A8)
        .expect("glyphset")
        .expect("Some");
    let mut add_body: Vec<u8> = Vec::new();
    add_body.extend_from_slice(&1_u32.to_le_bytes()); // n
    add_body.extend_from_slice(&1_u32.to_le_bytes()); // id = 1
    add_body.extend_from_slice(&u16::to_le_bytes(8)); // width
    add_body.extend_from_slice(&u16::to_le_bytes(8)); // height
    add_body.extend_from_slice(&i16::to_le_bytes(0)); // x bearing
    add_body.extend_from_slice(&i16::to_le_bytes(0)); // y bearing
    add_body.extend_from_slice(&i16::to_le_bytes(8)); // x_off
    add_body.extend_from_slice(&i16::to_le_bytes(0)); // y_off
    add_body.extend_from_slice(&[0xFFu8; 64]); // A8, stride 8
    b.render_add_glyphs(None, gs.as_raw(), &add_body)
        .expect("add_glyphs");
    // One element of 8 glyphs (CompositeGlyphs8): a text-run-sized
    // instance buffer.
    let mut items: Vec<u8> = vec![8u8, 0, 0, 0];
    items.extend_from_slice(&i16::to_le_bytes(0)); // dx
    items.extend_from_slice(&i16::to_le_bytes(0)); // dy
    items.extend_from_slice(&[1u8; 8]); // 8 × id=1

    // One trapezoid: axis-aligned box (0,0)-(40,40), 16.16 fixed.
    let f = |v: i32| (v << 16).to_le_bytes();
    let mut traps = Vec::new();
    for v in [0, 40, 0, 0, 0, 40, 40, 0, 40, 40] {
        traps.extend_from_slice(&f(v));
    }

    // Submit the frame, wait for the GPU, retire (as before_block does).
    fn settle(b: &mut KmsBackend) {
        b.engine
            .close_open_frame(
                &mut b.store,
                &mut b.platform,
                crate::kms::render::frame_builder::CloseReason::SyncWait,
            )
            .expect("close frame");
        b.engine
            .flush_submit_group(
                &mut b.store,
                &mut b.platform,
                crate::kms::render::submit_group::FlushReason::SyncBoundary,
            )
            .expect("flush");
        b.platform.wait_idle_bounded();
        b.for_tests_poll_retired();
    }
    let draw_one = |b: &mut KmsBackend| {
        b.render_composite_glyphs(
            None,
            23,
            3,
            src_pic,
            dst_pic,
            0,
            gs.as_raw(),
            0,
            0,
            &items,
            0,
            0,
        )
        .expect("render_composite_glyphs");
        b.render_trapezoids(None, 3, src_pic, dst_pic, 0, 0, 0, &traps, 0, 0)
            .expect("render_trapezoids");
    };

    // A text-heavy frame: more glyph runs and trapezoid requests than
    // the old pool kept idle per size class (64).
    const REQUESTS_PER_FRAME: usize = 100;
    let draw = |b: &mut KmsBackend| {
        for _ in 0..REQUESTS_PER_FRAME {
            draw_one(b);
        }
    };

    // Warm-up: atlas, glyph upload, mask scratch, pipelines, and the
    // first upload block.
    for _ in 0..2 {
        draw(&mut b);
        settle(&mut b);
    }

    const FRAMES: u64 = 16;
    let before = mem_accounting::thread_alloc_calls();
    for _ in 0..FRAMES {
        draw(&mut b);
        settle(&mut b);
    }
    let allocs = mem_accounting::thread_alloc_calls() - before;
    eprintln!(
        "{FRAMES} frames × {REQUESTS_PER_FRAME} × (glyph run + trapezoid request): \
             {allocs} allocations"
    );
    // 200 small requests per frame fit one upload block, and each frame
    // retires before the next opens, so the steady state reuses the
    // warm-up's block. Per-request buffers make this O(requests): the
    // size-classed pool, capped at 64 idle entries, reallocates the
    // other 136 of every frame's 200.
    assert!(
        allocs <= 2,
        "{allocs} vkAllocateMemory calls for {FRAMES} frames of {REQUESTS_PER_FRAME} glyph \
             runs + {REQUESTS_PER_FRAME} trapezoid requests: upload data is not sharing blocks",
    );
}

/// Stage 4d Manual-redirect CopyArea clip-by-children fix.
///
/// Scenario from `yserver-hw-mate.log` (CC frame + reparented
/// CC client window in one redirected backing):
///   - Frame W=997, H=652 at parent-local (0, 0).
///   - Reparented CC client at (11, 41) inside the frame,
///     size 975×600 (mapped).
///   - Marco copies its decoration pixmap into the frame with
///     a `ClipByChildren` GC, full 997×652.
///
/// Pre-fix: v2's `copy_area` blits the full 997×652 into the
/// redirected backing, clobbering CC's content. Visible symptom:
/// only the small region CC repaints next survives — the famous
/// "top-left square only" artefact.
///
/// Spec-correct behaviour (Xorg `mi/midispcur.c` + the
/// `ClipByChildren` rule): subtract every mapped child window's
/// rect from the destination rect before issuing the copy. For
/// this one-child case the result is exactly four strips: top,
/// bottom, left-of-child, right-of-child.
#[test]
fn copy_area_clip_by_children_excludes_mapped_child_rect() {
    use ash::vk;
    let dst = vk::Rect2D {
        offset: vk::Offset2D { x: 0, y: 0 },
        extent: vk::Extent2D {
            width: 997,
            height: 652,
        },
    };
    let child = vk::Rect2D {
        offset: vk::Offset2D { x: 11, y: 41 },
        extent: vk::Extent2D {
            width: 975,
            height: 600,
        },
    };

    let got = compute_copy_area_dst_rects(dst, &[child]);

    // Expected order: top strip, bottom strip, left middle,
    // right middle (Xorg/pixman band order).
    let want = [
        vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: vk::Extent2D {
                width: 997,
                height: 41,
            },
        },
        vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 641 },
            extent: vk::Extent2D {
                width: 997,
                height: 11,
            },
        },
        vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 41 },
            extent: vk::Extent2D {
                width: 11,
                height: 600,
            },
        },
        vk::Rect2D {
            offset: vk::Offset2D { x: 986, y: 41 },
            extent: vk::Extent2D {
                width: 11,
                height: 600,
            },
        },
    ];

    assert_eq!(
        got.len(),
        want.len(),
        "expected 4 surviving strips (top, bottom, left-middle, right-middle); \
             pre-fix returns the unclipped 1-rect input, which is the bug",
    );
    for (i, (g, w)) in got.iter().zip(want.iter()).enumerate() {
        assert_eq!(
            (g.offset.x, g.offset.y, g.extent.width, g.extent.height),
            (w.offset.x, w.offset.y, w.extent.width, w.extent.height),
            "strip {i} mismatch",
        );
    }
}

#[test]
fn copy_area_clip_by_children_no_children_returns_input() {
    use ash::vk;
    let dst = vk::Rect2D {
        offset: vk::Offset2D { x: 5, y: 7 },
        extent: vk::Extent2D {
            width: 100,
            height: 80,
        },
    };
    let got = compute_copy_area_dst_rects(dst, &[]);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].offset.x, 5);
    assert_eq!(got[0].offset.y, 7);
    assert_eq!(got[0].extent.width, 100);
    assert_eq!(got[0].extent.height, 80);
}

// ── compute_render_composite_clip — audit #2 (2026-05-19) ────────
//
// Mirrors Xorg's `miComputeCompositeRegion`
// (`render/mipict.c:316-389`). Per-test vectors hand-traced from
// the Xorg algorithm so the expected output is grounded in the
// reference, not in my own arithmetic (per
// `feedback_test_vectors_must_be_external`).

/// All three clips `None` → result `None` (engine paints
/// everywhere, matching Xorg's "no clientClip" path which
/// leaves pRegion unconstrained beyond dst extent).
#[test]
fn compute_render_composite_clip_all_none_returns_none() {
    let got = compute_render_composite_clip(None, None, (0, 0), None, (0, 0));
    assert!(got.is_none());
}

/// Only dst clip set → result is dst clip (no translation).
#[test]
fn compute_render_composite_clip_only_dst() {
    let dst = vec![Rectangle16 {
        x: 10,
        y: 20,
        width: 30,
        height: 40,
    }];
    let got = compute_render_composite_clip(Some(&dst), None, (0, 0), None, (0, 0));
    assert_eq!(got.as_deref(), Some(dst.as_slice()));
}

/// Only src clip set, src and dst coincide (xDst==xSrc, yDst==
/// ySrc → translation (0,0)) → result is src clip as-is. This
/// is the load-bearing case for the audit: xfwm4/muffin set a
/// clip on a source picture and pre-fix yserver ignored it.
#[test]
fn compute_render_composite_clip_src_only_zero_translation() {
    let src = vec![Rectangle16 {
        x: 5,
        y: 5,
        width: 10,
        height: 10,
    }];
    let got = compute_render_composite_clip(None, Some(&src), (0, 0), None, (0, 0));
    assert_eq!(got.as_deref(), Some(src.as_slice()));
}

/// Src clip translates to dst space by (xDst - xSrc, yDst -
/// ySrc). Per Xorg `mipict.c:356`:
/// `miClipPictureSrc(pRegion, pSrc, xDst - xSrc, yDst - ySrc)`.
/// Set src clip {0,0 4×4}, composite from src(2,2) to
/// dst(10,20), 4×4 — translation is (10-2, 20-2) = (8, 18).
/// Expected: src clip translated to {8,18 4×4}.
#[test]
fn compute_render_composite_clip_translates_src_clip_to_dst_space() {
    let src = vec![Rectangle16 {
        x: 0,
        y: 0,
        width: 4,
        height: 4,
    }];
    let got = compute_render_composite_clip(None, Some(&src), (8, 18), None, (0, 0));
    assert_eq!(
        got.as_deref(),
        Some(
            &[Rectangle16 {
                x: 8,
                y: 18,
                width: 4,
                height: 4,
            }][..]
        )
    );
}

/// Dst clip ∩ src-translated clip when the two overlap on a
/// strict sub-rect. dst clip {0,0 100×100}; src clip {0,0 50×50}
/// translated by (20, 30) → {20,30 50×50}. Intersection:
/// {20,30 50×50} (src translates fully inside dst).
#[test]
fn compute_render_composite_clip_dst_and_src_intersection() {
    let dst = vec![Rectangle16 {
        x: 0,
        y: 0,
        width: 100,
        height: 100,
    }];
    let src = vec![Rectangle16 {
        x: 0,
        y: 0,
        width: 50,
        height: 50,
    }];
    let got = compute_render_composite_clip(Some(&dst), Some(&src), (20, 30), None, (0, 0));
    assert_eq!(
        got.as_deref(),
        Some(
            &[Rectangle16 {
                x: 20,
                y: 30,
                width: 50,
                height: 50,
            }][..]
        )
    );
}

/// Disjoint dst and src-translated clips → empty result (which
/// Xorg treats as "paint nothing" — `miComputeCompositeRegion`
/// returns FALSE there).
#[test]
fn compute_render_composite_clip_disjoint_yields_empty() {
    let dst = vec![Rectangle16 {
        x: 0,
        y: 0,
        width: 10,
        height: 10,
    }];
    let src = vec![Rectangle16 {
        x: 0,
        y: 0,
        width: 10,
        height: 10,
    }];
    // Translate src by (100, 0) → {100,0 10×10}; disjoint from
    // dst {0,0 10×10}.
    let got = compute_render_composite_clip(Some(&dst), Some(&src), (100, 0), None, (0, 0));
    assert_eq!(got.as_deref(), Some(&[][..]));
}

/// Three-way intersection: dst ∩ src ∩ mask. Use disjoint
/// translations that all overlap at one corner. dst {0,0 50×50},
/// src {0,0 50×50} translated by (10, 10) → {10,10 50×50},
/// mask {0,0 50×50} translated by (20, 20) → {20,20 50×50}.
/// Three-way intersection: {20,20 30×30}.
#[test]
fn compute_render_composite_clip_three_way_intersection() {
    let dst = vec![Rectangle16 {
        x: 0,
        y: 0,
        width: 50,
        height: 50,
    }];
    let src = vec![Rectangle16 {
        x: 0,
        y: 0,
        width: 50,
        height: 50,
    }];
    let mask = vec![Rectangle16 {
        x: 0,
        y: 0,
        width: 50,
        height: 50,
    }];
    let got =
        compute_render_composite_clip(Some(&dst), Some(&src), (10, 10), Some(&mask), (20, 20));
    assert_eq!(
        got.as_deref(),
        Some(
            &[Rectangle16 {
                x: 20,
                y: 20,
                width: 30,
                height: 30,
            }][..]
        )
    );
}

/// Multi-rect dst clip ∩ single src clip translated: every
/// dst rect intersects with the translated src rect; union of
/// intersections is what the engine should emit per-scissor.
#[test]
fn compute_render_composite_clip_multi_rect_dst_with_single_src() {
    let dst = vec![
        Rectangle16 {
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        },
        Rectangle16 {
            x: 20,
            y: 0,
            width: 10,
            height: 10,
        },
    ];
    // Src clip {0,0 100×100} translated by (0, 0) → covers
    // both dst rects. Result: both dst rects survive.
    let src = vec![Rectangle16 {
        x: 0,
        y: 0,
        width: 100,
        height: 100,
    }];
    let got = compute_render_composite_clip(Some(&dst), Some(&src), (0, 0), None, (0, 0));
    assert_eq!(got.as_deref(), Some(dst.as_slice()));
}

/// GC clip intersection: rect partially inside a single clip rect
/// produces the intersection alone. Pre-fix the stub returns the
/// whole rect — losing the GC clip semantics.
#[test]
fn intersect_rect_with_clip_single_overlapping_clip_returns_intersection() {
    use ash::vk;
    let rect = vk::Rect2D {
        offset: vk::Offset2D { x: 0, y: 0 },
        extent: vk::Extent2D {
            width: 200,
            height: 200,
        },
    };
    let clip = vec![vk::Rect2D {
        offset: vk::Offset2D { x: 50, y: 50 },
        extent: vk::Extent2D {
            width: 100,
            height: 100,
        },
    }];
    let got = intersect_rect_with_clip(rect, &clip);
    assert_eq!(got.len(), 1, "single clip ∩ rect = one intersection");
    assert_eq!(
        (
            got[0].offset.x,
            got[0].offset.y,
            got[0].extent.width,
            got[0].extent.height,
        ),
        (50, 50, 100, 100),
    );
}

#[test]
fn intersect_rect_with_clip_empty_clip_returns_empty() {
    use ash::vk;
    // Empty clip-rect list represents an empty XFixes region — paint nothing.
    let rect = vk::Rect2D {
        offset: vk::Offset2D { x: 0, y: 0 },
        extent: vk::Extent2D {
            width: 10,
            height: 10,
        },
    };
    let got = intersect_rect_with_clip(rect, &[]);
    assert!(got.is_empty());
}

/// Multi-rect clip: dst that straddles two non-contiguous clip rects
/// produces two intersections.
#[test]
fn intersect_rect_with_clip_multi_rect_clip_produces_per_rect_intersections() {
    use ash::vk;
    let rect = vk::Rect2D {
        offset: vk::Offset2D { x: 0, y: 0 },
        extent: vk::Extent2D {
            width: 200,
            height: 100,
        },
    };
    let clip = vec![
        vk::Rect2D {
            offset: vk::Offset2D { x: 10, y: 10 },
            extent: vk::Extent2D {
                width: 40,
                height: 40,
            },
        },
        vk::Rect2D {
            offset: vk::Offset2D { x: 150, y: 10 },
            extent: vk::Extent2D {
                width: 40,
                height: 40,
            },
        },
    ];
    let got = intersect_rect_with_clip(rect, &clip);
    assert_eq!(got.len(), 2);
    assert_eq!(
        (
            got[0].offset.x,
            got[0].offset.y,
            got[0].extent.width,
            got[0].extent.height,
        ),
        (10, 10, 40, 40),
    );
    assert_eq!(
        (
            got[1].offset.x,
            got[1].offset.y,
            got[1].extent.width,
            got[1].extent.height,
        ),
        (150, 10, 40, 40),
    );
}

#[test]
fn copy_area_clip_by_children_disjoint_child_returns_input() {
    // Child fully outside dst → no clipping.
    use ash::vk;
    let dst = vk::Rect2D {
        offset: vk::Offset2D { x: 0, y: 0 },
        extent: vk::Extent2D {
            width: 50,
            height: 50,
        },
    };
    let child = vk::Rect2D {
        offset: vk::Offset2D { x: 200, y: 200 },
        extent: vk::Extent2D {
            width: 10,
            height: 10,
        },
    };
    let got = compute_copy_area_dst_rects(dst, &[child]);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].offset.x, 0);
    assert_eq!(got[0].offset.y, 0);
}
