use super::*;

/// Regression for the runtime `notify_drawable_retired` wiring
/// (2026-05-31). Pre-fix the engine's `drawable_view_cache`
/// accumulated entries forever — every `DestroyPixmap` /
/// `DestroyWindow` orphaned the per-drawable cached `VkImageView`s
/// because `notify_drawable_retired` was defined but had zero
/// callers, and only `RenderEngine::drop` ever swept the cache (at
/// process exit). Long-running sessions grew unboundedly.
///
/// Post-fix `store_decref_with_invalidate` /
/// `poll_pending_retire_with_invalidate` bridge `DrawableStore`
/// destruction to `RenderEngine::notify_drawable_retired` via an
/// `on_destroyed` closure that fires BEFORE `Storage::destroy`,
/// so views are cleaned synchronously when the drawable retires.
///
/// The cache is populated by `ensure_drawable_view` from RENDER
/// composite paths (NOT plain `copy_area`), so this test drives
/// `render_composite(src_pic, dst_pic)` to seed the cache, then
/// releases the picture + pixmap to exercise the runtime destroy
/// chain and asserts the cache drops accordingly.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn free_pixmap_invalidates_engine_view_cache() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let src_pix = b.create_pixmap(None, 32, 4, 4).expect("src pixmap");
    let dst_pix = b.create_pixmap(None, 32, 4, 4).expect("dst pixmap");
    let src_xid = src_pix.as_raw();

    b.fill_rectangle(None, src_xid, 0xFFFF0000, 0, 0, 4, 4)
        .expect("fill src");
    b.fill_rectangle(None, dst_pix.as_raw(), 0xFF0000FF, 0, 0, 4, 4)
        .expect("fill dst");

    let src_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(src_pix), 0, 0, &[])
        .expect("render_create_picture src")
        .expect("Some(src PictureHandle)");
    let dst_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(dst_pix), 0, 0, &[])
        .expect("render_create_picture dst")
        .expect("Some(dst PictureHandle)");

    let baseline_cache_len = b.drawable_view_cache_len();

    // OP_SRC composite from src pixmap to dst pixmap — invokes
    // `ensure_drawable_view` for src (and possibly mask=white
    // alias), populating drawable_view_cache.
    b.render_composite(
        None,
        1, // OP_SRC
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
    .expect("render_composite");

    let after_composite_cache_len = b.drawable_view_cache_len();
    assert!(
        after_composite_cache_len > baseline_cache_len,
        "render_composite should populate the engine view cache \
         (baseline={baseline_cache_len}, after_composite={after_composite_cache_len})",
    );

    // render_composite records into the deferred frame builder;
    // close + flush so the GPU actually submits before we test
    // retirement. (Otherwise the FenceTicket is unsignaled →
    // decref parks in pending_retire and never destroys.)
    if b.frame_builder_is_open_for_tests() {
        b.engine_close_open_frame_for_timeout_for_tests()
            .expect("close open frame");
    }
    b.engine_flush_submit_group_for_tests()
        .expect("flush submit group");

    // Free the picture FIRST so it drops its refcount on src;
    // otherwise free_pixmap's decref returns StillReferenced.
    b.render_free_picture(None, src_pic.as_raw())
        .expect("free src picture");
    // free_pixmap routes through `backend.rs::free_pixmap` →
    // `store_decref_with_invalidate` → engine.notify_drawable_retired
    // → cache entry destroyed (VkImageView destroyed before
    // Storage::destroy releases the underlying VkImage).
    b.free_pixmap(None, src_xid).expect("free src pixmap");
    // free_pixmap likely parks in pending_retire (in-flight composite
    // fence not yet signaled). Wait on all submitted work, then drive
    // the retirement loop — poll_pending_retire_with_invalidate
    // sweeps and fires the invalidate closure for each retired id.
    b.engine_drain_all_for_tests();
    b.for_tests_poll_retired();

    let after_free_cache_len = b.drawable_view_cache_len();
    assert!(
        after_free_cache_len < after_composite_cache_len,
        "free_pixmap + for_tests_poll_retired must drop cached views \
         for the freed drawable (after_composite={after_composite_cache_len}, \
         after_free={after_free_cache_len}). Pre-fix this was equal — \
         cached views accumulated until engine Drop (process exit), so \
         long sessions grew unboundedly.",
    );
}

/// `read_depth1_pixmap` — the SHAPE::Mask introspection hook.
/// PutImage a staircase bitmap into a depth-1 pixmap (width 10,
/// deliberately not byte-aligned; wire rows are LSBFirst bits
/// with 32-bit scanline pad), then read it back as the
/// byte-per-pixel `(w, h, bytes)` triple the YX-bander consumes.
/// The trait default returns `Ok(None)` — v2 must override it or
/// every ShapeMask degrades to a bounding-box rect.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn read_depth1_pixmap_returns_mask_bytes() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    const W: u16 = 10;
    const H: u16 = 6;
    let xid = b
        .create_pixmap(None, 1, W, H)
        .expect("create_pixmap")
        .as_raw();

    // Staircase: row y sets pixels x <= y. Wire format: LSBFirst
    // bit order, ceil(10/32)*4 = 4 bytes per row.
    let mut bits = vec![0u8; 4 * H as usize];
    for y in 0..H as usize {
        bits[y * 4] = (1u16 << (y + 1)).wrapping_sub(1) as u8;
    }
    b.put_image(None, xid, 1, W, H, 0, 0, &bits)
        .expect("put_image depth-1");

    let (w, h, bytes) = b
        .read_depth1_pixmap(None, xid)
        .expect("read_depth1_pixmap")
        .expect(
            "render must introspect depth-1 pixmaps (trait default None = ShapeMask degrades to bbox)",
        );
    assert_eq!(
        (w, h),
        (u32::from(W), u32::from(H)),
        "dims match the pixmap"
    );
    assert_eq!(
        bytes.len(),
        (w * h) as usize,
        "byte per pixel, tightly packed"
    );
    for y in 0..H as usize {
        for x in 0..W as usize {
            let set = bytes[y * W as usize + x] != 0;
            assert_eq!(set, x <= y, "pixel ({x},{y}) staircase membership");
        }
    }
}

/// `read_depth1_pixmap` on a non-depth-1 drawable must return
/// `None` (best-effort decline), not misread BGRA bytes as mask
/// coverage.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn read_depth1_pixmap_declines_depth32() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let xid = b
        .create_pixmap(None, 32, 4, 4)
        .expect("create_pixmap")
        .as_raw();
    let got = b.read_depth1_pixmap(None, xid).expect("read_depth1_pixmap");
    assert!(got.is_none(), "depth-32 drawable must decline, got {got:?}");
}

/// xts5 Xlib9/XFillRectangle TP1 minimal repro: two consecutive
/// `PolyFillRectangle` calls — first a full-drawable background clear
/// (mimicking the Map-time fill that the X server applies to a freshly
/// mapped window), then a small foreground rectangle at (20, 30, 70x30)
/// — followed by `GetImage` over the whole drawable. The second fill's
/// pixels MUST be visible in the readback; outside the rect, the
/// background must show through.
///
/// In a vng XTS run on 2026-06-04 every TP1 fail showed an all-zero
/// `bad` image — the first fill landed, the second fill vanished. This
/// test captures that exact sequence so the bug can be bisected
/// against `cargo test` instead of a 20s vng cycle.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn two_fills_then_get_image_returns_second_fill() {
    use yserver_core::{backend::WindowHandle, host_x11::HostSubwindowVisual};
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // Create a depth-24 child window at 0,0 sized 100×90, with
    // background_pixel=W_BG=0 — matches makewin's setup.
    // allocate_window_storage's init fill is the equivalent of the
    // first fill we see in the XTS trace.
    let parent = WindowHandle::from_raw(1).expect("root WindowHandle");
    let win = b
        .create_subwindow(
            None,
            parent,
            0,
            0,
            100,
            90,
            1,
            HostSubwindowVisual::Explicit {
                depth: 24,
                visual_xid: 0,
                colormap_xid: 0,
            },
            Some(0x0000_0000), // W_BG = 0
            None,
        )
        .expect("create_subwindow");
    let xid = win.as_raw();
    b.map_window_for_tests(xid).expect("map_subwindow");

    // XCALL fill at (20, 30, 70, 30) with fg=W_FG=1 (pixel value 1).
    let small_rect = {
        let mut buf = Vec::new();
        buf.extend_from_slice(&i16::to_le_bytes(20));
        buf.extend_from_slice(&i16::to_le_bytes(30));
        buf.extend_from_slice(&u16::to_le_bytes(70));
        buf.extend_from_slice(&u16::to_le_bytes(30));
        buf
    };
    b.poly_fill_rectangle(None, xid, 0x0000_0001, &small_rect)
        .expect("XCALL fill");

    // GetImage the whole drawable as ZPixmap, AllPlanes.
    let bytes = b
        .get_image_pixels_for_tests(xid, 2, 0, 0, 100, 90, !0)
        .expect("get_image")
        .expect("Some(bytes)");
    assert_eq!(
        bytes.len(),
        100 * 90 * 4,
        "depth-24 ZPixmap reply is 4 bytes/pixel (BGRA wire)",
    );

    // Pixel inside the rect — (50, 45). Expect BGRA [0x01, 0, 0, *].
    let inside = (45 * 100 + 50) * 4;
    assert_eq!(
        bytes[inside],
        0x01,
        "inside-rect B byte must be 1 (second fill landed); full pixel = {:02x?}",
        &bytes[inside..inside + 4],
    );

    // Pixel outside the rect — (5, 5). Expect BGRA [0, 0, 0, *].
    let outside = (5 * 100 + 5) * 4;
    assert_eq!(
        bytes[outside],
        0x00,
        "outside-rect B byte must be 0 (Map clear background); full pixel = {:02x?}",
        &bytes[outside..outside + 4],
    );
}

/// xts5 Xlib9/XFillRectangle TP1 compose-boundary repro: map a fresh
/// window, force one real scene compose, retire its page-flip ack,
/// then issue the foreground fill, force a second compose, and read
/// the drawable back via GetImage.
///
/// The non-compose harness (`for_tests_with_vk`) already proves that
/// "init fill + second fill + GetImage" works when no scene submit
/// happens in between. This variant is the load-bearing one for the
/// live XTS failure because it exercises the full
/// `fill -> compose -> fill -> compose` rhythm with a real scene ack
/// retirement between the two composes.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn compose_then_fill_then_get_image_returns_second_fill() {
    use yserver_core::{backend::WindowHandle, host_x11::HostSubwindowVisual};

    let mut b = match KmsBackend::for_tests_with_vk_live_scene() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk/live scene: {e}");
            return;
        }
    };

    let parent = WindowHandle::from_raw(1).expect("root WindowHandle");
    let win = b
        .create_subwindow(
            None,
            parent,
            0,
            0,
            100,
            90,
            1,
            HostSubwindowVisual::Explicit {
                depth: 24,
                visual_xid: 0,
                colormap_xid: 0,
            },
            Some(0x0000_0000),
            None,
        )
        .expect("create_subwindow");
    let xid = win.as_raw();
    b.map_window_for_tests(xid).expect("map_subwindow");

    let composite_submits_before = b.telemetry().lifetime.composite_submits;
    b.tick_maybe_composite_for_tests();
    let composite_submits_after = b.telemetry().lifetime.composite_submits;
    assert!(
        composite_submits_after > composite_submits_before,
        "fixture sanity: maybe_composite must perform a real compose submit before the second fill",
    );
    let retired = b
        .simulate_scene_page_flip_complete_for_tests()
        .expect("retire first compose ack");
    assert!(
        retired >= 1,
        "fixture sanity: first compose must leave a pending scene ack to retire",
    );

    let small_rect = {
        let mut buf = Vec::new();
        buf.extend_from_slice(&i16::to_le_bytes(20));
        buf.extend_from_slice(&i16::to_le_bytes(30));
        buf.extend_from_slice(&u16::to_le_bytes(70));
        buf.extend_from_slice(&u16::to_le_bytes(30));
        buf
    };
    b.poly_fill_rectangle(None, xid, 0x0000_0001, &small_rect)
        .expect("foreground fill");

    let composite_submits_before_second = b.telemetry().lifetime.composite_submits;
    b.tick_maybe_composite_for_tests();
    let composite_submits_after_second = b.telemetry().lifetime.composite_submits;
    assert!(
        composite_submits_after_second > composite_submits_before_second,
        "fixture sanity: second maybe_composite must submit after the foreground fill",
    );

    let bytes = b
        .get_image_pixels_for_tests(xid, 2, 0, 0, 100, 90, !0)
        .expect("get_image")
        .expect("Some(bytes)");
    assert_eq!(bytes.len(), 100 * 90 * 4, "depth-24 ZPixmap is BGRA8");

    let inside = (45 * 100 + 50) * 4;
    assert_eq!(
        bytes[inside],
        0x01,
        "inside-rect B byte must be 1 after compose-before-fill; pixel = {:02x?}",
        &bytes[inside..inside + 4],
    );

    let outside = 0;
    assert_eq!(
        &bytes[outside..outside + 4],
        &[0x00, 0x00, 0x00, 0xFF],
        "outside the fill rect the mapped background must remain black",
    );
}

// ── Task 2: content_version bump tests ───────────────────────────────────────

/// Task 2 (content_version bump): `fill_rect_batch` must bump
/// `content_version` on the target drawable after appending its
/// `RecordedOp` to the open frame.
#[test]
#[ignore = "needs Vulkan ICD (lavapipe)"]
fn content_version_bumps_on_fill_rect_batch() {
    let mut be = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let xid = be
        .allocate_test_pixmap_bgra(64, 64)
        .expect("allocate_test_pixmap_bgra");

    let v0 = be
        .drawable_content_version_for_tests(xid)
        .expect("drawable must exist");

    let rects = [ash::vk::Rect2D {
        offset: ash::vk::Offset2D { x: 0, y: 0 },
        extent: ash::vk::Extent2D {
            width: 32,
            height: 32,
        },
    }];
    be.engine_fill_rect_batch_for_tests(xid, [1.0, 0.0, 0.0, 1.0], &rects)
        .expect("fill_rect_batch");

    assert!(
        be.drawable_content_version_for_tests(xid)
            .expect("drawable must exist")
            > v0,
        "fill_rect_batch must bump content_version",
    );
}

/// Task 2 (content_version bump): `copy_area` must bump
/// `content_version` on the *destination* drawable after appending
/// its `RecordedOp` to the open frame.
#[test]
#[ignore = "needs Vulkan ICD (lavapipe)"]
fn content_version_bumps_on_copy_area() {
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

    let v0_dst = be
        .drawable_content_version_for_tests(dst)
        .expect("dst must exist");
    let v0_src = be
        .drawable_content_version_for_tests(src)
        .expect("src must exist");

    let src_rect = ash::vk::Rect2D {
        offset: ash::vk::Offset2D { x: 0, y: 0 },
        extent: ash::vk::Extent2D {
            width: 32,
            height: 32,
        },
    };
    be.engine_copy_area_for_tests(src, dst, src_rect, ash::vk::Offset2D { x: 0, y: 0 })
        .expect("copy_area");

    assert!(
        be.drawable_content_version_for_tests(dst)
            .expect("dst must exist")
            > v0_dst,
        "copy_area must bump content_version on the destination",
    );
    // src is a read, not a write — its version must NOT be bumped.
    assert_eq!(
        be.drawable_content_version_for_tests(src)
            .expect("src must exist"),
        v0_src,
        "copy_area must NOT bump content_version on the source",
    );
}

/// Task 2 (content_version bump): `put_image` must bump
/// `content_version` on the target drawable after appending its
/// `RecordedOp` to the open frame.
#[test]
#[ignore = "needs Vulkan ICD (lavapipe)"]
fn content_version_bumps_on_put_image() {
    let mut be = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let xid = be
        .allocate_test_pixmap_bgra(64, 64)
        .expect("allocate_test_pixmap_bgra");

    let v0 = be
        .drawable_content_version_for_tests(xid)
        .expect("drawable must exist");

    let bytes: Vec<u8> = vec![0xffu8; 32 * 32 * 4];
    be.engine_put_image_for_tests(
        xid,
        ash::vk::Offset2D { x: 0, y: 0 },
        ash::vk::Extent2D {
            width: 32,
            height: 32,
        },
        &bytes,
        32,
    )
    .expect("put_image");

    assert!(
        be.drawable_content_version_for_tests(xid)
            .expect("drawable must exist")
            > v0,
        "put_image must bump content_version",
    );
}

/// Task 2 (content_version bump): `render_composite` must bump
/// `content_version` on the destination drawable after appending its
/// `RecordedOp` to the open frame.
#[test]
#[ignore = "needs Vulkan ICD (lavapipe)"]
fn content_version_bumps_on_render_composite() {
    let mut be = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let xid = be
        .allocate_test_pixmap_bgra(64, 64)
        .expect("allocate_test_pixmap_bgra");

    let v0 = be
        .drawable_content_version_for_tests(xid)
        .expect("drawable must exist");

    be.render_composite_for_tests(xid, [0.0, 0.5, 1.0, 1.0], 32, 32)
        .expect("render_composite");

    assert!(
        be.drawable_content_version_for_tests(xid)
            .expect("drawable must exist")
            > v0,
        "render_composite must bump content_version",
    );
}

/// Task 2 (content_version bump): `render_traps_or_tris` must bump
/// `content_version` on the destination drawable after appending its
/// `RecordedOp` to the open frame.
#[test]
#[ignore = "needs Vulkan ICD (lavapipe)"]
fn content_version_bumps_on_render_traps_or_tris() {
    let mut be = match yserver::kms::render::KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let xid = be
        .allocate_test_pixmap_bgra(64, 64)
        .expect("allocate_test_pixmap_bgra");

    let v0 = be
        .drawable_content_version_for_tests(xid)
        .expect("drawable must exist");

    be.engine_render_traps_or_tris_for_tests(xid, [1.0, 0.0, 0.0, 1.0], 32, 32)
        .expect("render_traps_or_tris");

    assert!(
        be.drawable_content_version_for_tests(xid)
            .expect("drawable must exist")
            > v0,
        "render_traps_or_tris must bump content_version",
    );
}
