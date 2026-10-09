use super::*;

/// From the xeyes-resize bug (2026-05-16): the user resizes the
/// xeyes window larger; the new bigger eyes paint correctly but the
/// OLD small-eye-white pixels at the original (smaller) positions
/// remain visible in the upper-left. That is a window WITH a
/// background, which X11 says to tile on a size change — pinned by
/// `a_resize_still_retiles_a_window_that_has_a_background`.
///
/// This window has NO background, so #143 changed what it asserts:
/// X11 leaves such a window's existing contents alone ("if no
/// background is defined, the existing screen contents are not
/// altered"; `mi/miexpose.c:438-440` returns before painting), so the
/// red the client painted must SURVIVE the grow and only the region
/// the grow added is initialised.
///
/// What it still pins is the storage-orphan regression the fixture was
/// written for: the old storage's `destroy_now` must not remove
/// `by_xid[xid]` after the new allocation re-installed it, or the
/// `get_image` below comes back `None`.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn subwindow_resize_clears_old_paint() {
    use yserver_core::{
        backend::WindowHandle,
        host_x11::{HostSubwindowConfig, HostSubwindowVisual},
    };
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // Create depth-32 child window at 16×16 with no bg attributes.
    // The map allocates it; bg None seeds it from the parent (the root).
    let parent = WindowHandle::from_raw(1).expect("root WindowHandle");
    let child = b
        .create_subwindow(
            None,
            parent,
            0,
            0,
            16,
            16,
            0,
            HostSubwindowVisual::Explicit {
                depth: 32,
                visual_xid: 0,
                colormap_xid: 0,
            },
            None,
            None,
        )
        .expect("create_subwindow");
    let xid = child.as_raw();
    b.map_window_for_tests(xid).expect("map");

    // Paint red into the 16×16 window so the "old paint" exists.
    // Foreground 0xFFFF0000 = ARGB(0xFF, R=0xFF, G=0, B=0).
    b.fill_rectangle(None, xid, 0xFFFF0000, 0, 0, 16, 16)
        .expect("paint red");

    // Resize to 64×64 via configure_subwindow. This is the path
    // v2's WMs (e16 / fvwm / etc.) drive on window-frame resize.
    b.configure_subwindow(
        None,
        xid,
        HostSubwindowConfig {
            x: None,
            y: None,
            width: Some(64),
            height: Some(64),
            border_width: None,
            stack_mode: None,
            sibling: None,
        },
    )
    .expect("configure_subwindow resize");

    // Read back the resized storage at (5, 5) — inside the OLD
    // 16×16 region, which #143 keeps, and at (30, 30), which the
    // grow added and the storage init covers.
    //
    // get_image waits on its internal fence, which lets the
    // OLD storage's pending_retire entry actually retire via
    // destroy_now. The decref-PendingFence path detached
    // `by_xid[xid]` for the old drawable; the new storage's
    // allocate re-installed it. When the old storage's
    // destroy_now fires inside this get_image's drain, it MUST
    // NOT remove `by_xid[xid]` (which now points to the NEW
    // drawable). Pre-fix: destroy_now blindly removed the xid
    // mapping → new storage orphaned → get_image returns None.
    let out = b
        .get_image_pixels_for_tests(xid, 2, 0, 0, 64, 64, !0)
        .expect("get_image returned Err (storage orphaned by destroy_now?)")
        .expect("Some — by_xid[xid] resolved");
    let pixel = |x: usize, y: usize| -> [u8; 4] {
        let off = (y * 64 + x) * 4;
        [out[off], out[off + 1], out[off + 2], out[off + 3]]
    };
    // (5, 5) is well-inside the old 16×16 footprint.
    assert_eq!(
        pixel(5, 5),
        [0x00, 0x00, 0xFF, 0xFF],
        "post-resize storage at (5,5) must still hold the red the client \
         painted (got {:?}): this window has no background, so nothing is \
         allowed to alter its existing contents, and nothing will ask it to \
         repaint them either",
        pixel(5, 5),
    );
    // (30, 30) is outside the old footprint, well inside the new.
    assert_eq!(
        pixel(30, 30),
        [0x00, 0x00, 0x00, 0x00],
        "the region the grow ADDED is initialised, never pool garbage \
         (got {:?})",
        pixel(30, 30),
    );
}

/// Stage 3f.14 follow-on: fresh pixmaps must read back as
/// transparent-black (depth-32) or opaque-black (depth-24),
/// NOT random Vk-undefined bytes.
///
/// Repro for the xeyes-resize artifact on mate + marco: xeyes
/// creates a depth-24 offscreen pixmap, sets a SHAPE clip
/// matching the eye outlines, paints eyes (only shape-clipped
/// pixels get content), then Present-Pixmaps the whole pixmap
/// to the window. Pre-fix: the non-eye-shape pixels of the
/// pixmap held undefined Vk memory → visible garbage in the
/// window. Post-fix: depth-appropriate safe-default clear on
/// create.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn fresh_pixmap_reads_back_zero() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let pix32 = b.create_pixmap(None, 32, 16, 16).expect("depth-32 pixmap");
    let pix24 = b.create_pixmap(None, 24, 16, 16).expect("depth-24 pixmap");

    let out32 = b
        .get_image_pixels_for_tests(pix32.as_raw(), 2, 0, 0, 16, 16, !0)
        .expect("get_image depth-32")
        .expect("Some");
    let out24 = b
        .get_image_pixels_for_tests(pix24.as_raw(), 2, 0, 0, 16, 16, !0)
        .expect("get_image depth-24")
        .expect("Some");

    // depth-32 = transparent black (premul no-op).
    for (i, px) in out32.chunks_exact(4).enumerate() {
        assert_eq!(
            &px[0..4],
            &[0, 0, 0, 0],
            "fresh depth-32 pixmap pixel #{i} should be (0,0,0,0); got {:?}",
            &px[0..4],
        );
    }
    // depth-24 = opaque black.
    for (i, px) in out24.chunks_exact(4).enumerate() {
        assert_eq!(
            &px[0..4],
            &[0, 0, 0, 0xFF],
            "fresh depth-24 pixmap pixel #{i} should be (0,0,0,0xFF); got {:?}",
            &px[0..4],
        );
    }
}

/// Stage 3f.3 acceptance: a `Tiled` fill driven through
/// `apply_fill_state` + `poly_fill_rectangle` replicates the tile
/// pixmap across the destination via the engine's RENDER composite
/// path (`OP_SRC`, `Repeat::Normal`). e16 popup chrome paint
/// depends on this exact shape.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn tiled_fill_replicates_tile_pixmap() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // 2×2 tile pixmap pre-filled with red.
    let tile = b.create_pixmap(None, 32, 2, 2).expect("tile pixmap");
    b.fill_rectangle(None, tile.as_raw(), 0xFFFF_0000, 0, 0, 2, 2)
        .expect("tile fill red");

    // 4×4 dst pre-filled with blue so untouched pixels are visibly
    // distinct from the tile colour.
    let dst = b.create_pixmap(None, 32, 4, 4).expect("dst pixmap");
    b.fill_rectangle(None, dst.as_raw(), 0xFF00_00FF, 0, 0, 4, 4)
        .expect("dst pre-fill blue");

    // Activate Tiled fill state with origin (0, 0).
    b.apply_fill_state(
        None,
        &FillState::Tiled {
            pixmap: tile,
            origin: (0, 0),
        },
    )
    .expect("apply Tiled fill");

    // poly_fill_rectangle over the whole 4×4 dst — fg ignored for
    // tiled fill; the tile colour is what lands.
    let rect_bytes = {
        let mut buf = Vec::new();
        buf.extend_from_slice(&i16::to_le_bytes(0));
        buf.extend_from_slice(&i16::to_le_bytes(0));
        buf.extend_from_slice(&u16::to_le_bytes(4));
        buf.extend_from_slice(&u16::to_le_bytes(4));
        buf
    };
    b.poly_fill_rectangle(None, dst.as_raw(), 0x0000_0000, &rect_bytes)
        .expect("poly_fill_rectangle tiled");

    let out = b
        .get_image_pixels_for_tests(dst.as_raw(), 2, 0, 0, 4, 4, !0)
        .expect("get_image")
        .expect("Some");
    // Every pixel should now be red (tile colour), not the blue
    // pre-fill. BGRA8 wire bytes: [B=0, G=0, R=0xFF, A=0xFF].
    for (i, px) in out.chunks_exact(4).enumerate() {
        assert_eq!(
            &px[0..4],
            &[0x00, 0x00, 0xFF, 0xFF],
            "tile-filled pixel {i} must be red (got {:?})",
            &px[0..4]
        );
    }

    // Reset fill state so trailing test wiring doesn't inherit it.
    b.set_gc_fill_solid(None).expect("reset solid");
}

/// Stage 3f.14 acceptance: `set_container_background_pixmap`
/// tiles the source pixmap across the **entire root extent**, not
/// just the top-left corner. Pre-3f.14 v2 did a single `copy_area`
/// at (0, 0) and left the rest of root unchanged — fvwm3's floral
/// wallpaper covered only the top-left of the screen on bee. v1
/// tiles via its compositor pipeline; v2 routes through
/// `engine.render_composite` with `OP_SRC + Repeat::Normal`.
///
/// Test: 4×4 pixmap pre-filled red, set as root bg, read back two
/// points on root storage: (0, 0) and (5, 5) (which maps to tile
/// (1, 1) under the wrap rule). Both should read red. A point
/// outside the for_tests fb (`fb_w` = 800) is not exercised — the
/// fb is much larger than the tile so any (x, y) within
/// [0, 800) × [0, 600) hits a tiled tile.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn set_container_background_pixmap_tiles_across_root() {
    // Root GetImage reads the on-screen *scanout* (matching Xorg's
    // root = framebuffer), so this needs the scanout-capable fixture and
    // a real compose to blit the tiled root storage onto the scanout —
    // `for_tests_with_vk` has no scanout pool and would read nothing.
    let mut b = match KmsBackend::for_tests_with_vk_live_scene() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk/live scene: {e}");
            return;
        }
    };

    let tile = b.create_pixmap(None, 32, 4, 4).expect("tile pixmap");
    b.fill_rectangle(None, tile.as_raw(), 0xFFFF_0000, 0, 0, 4, 4)
        .expect("tile fill red");

    b.set_container_background_pixmap(None, tile.as_raw())
        .expect("set bg pixmap");

    // Drive a real scene compose (tiled root storage → scanout) and
    // retire its page-flip ack before reading the scanout back.
    b.tick_maybe_composite_for_tests();
    let _ = b.simulate_scene_page_flip_complete_for_tests();

    // Read 8×8 of root from the origin. With a 4×4 red tile the
    // first 8×8 must be entirely red. The root xid is 1 in v2's
    // test fixture (`KmsCore.window_id`).
    let root_xid = 1u32;
    let out = b
        .get_image_pixels_for_tests(root_xid, 2, 0, 0, 8, 8, !0)
        .expect("get_image")
        .expect("Some");
    assert_eq!(out.len(), 8 * 8 * 4, "8×8 BGRA8");
    for (i, px) in out.chunks_exact(4).enumerate() {
        // BGRA wire bytes for red (alpha-pre-applied opaque):
        // B=0, G=0, R=0xFF, A=0xFF.
        assert_eq!(
            &px[0..4],
            &[0x00, 0x00, 0xFF, 0xFF],
            "tiled root pixel #{i} must be red (got {:?})",
            &px[0..4],
        );
    }
}

/// `ClearArea` on a window with `bg_pixmap` must tile the pixmap
/// relative to the window origin, not issue a one-shot copy from the
/// same `(x, y)` source offset. fvwm3 frame/panel clears rely on this.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn clear_area_with_bg_pixmap_tiles_window_background() {
    use yserver_core::{backend::WindowHandle, host_x11::HostSubwindowVisual};

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let tile = b.create_pixmap(None, 32, 2, 2).expect("tile");
    b.fill_rectangle(None, tile.as_raw(), 0xFFFF_0000, 0, 0, 2, 2)
        .expect("tile red");

    let root = WindowHandle::from_raw(1).expect("root");
    let window = b
        .create_subwindow(
            None,
            root,
            0,
            0,
            8,
            8,
            0,
            HostSubwindowVisual::Explicit {
                depth: 32,
                visual_xid: 0,
                colormap_xid: 0,
            },
            None,
            None,
        )
        .expect("window");
    let xid = window.as_raw();
    b.map_window_for_tests(xid).expect("map");

    b.fill_rectangle(None, xid, 0xFF00_00FF, 0, 0, 8, 8)
        .expect("window blue");
    b.clear_area(None, xid, 0, Some(tile.as_raw()), 3, 3, 4, 4, (0, 0))
        .expect("clear_area bg_pixmap");

    let out = b
        .get_image_pixels_for_tests(xid, 2, 0, 0, 8, 8, !0)
        .expect("get_image")
        .expect("Some bytes");
    let pixel = |x: usize, y: usize| -> [u8; 4] {
        let off = (y * 8 + x) * 4;
        [out[off], out[off + 1], out[off + 2], out[off + 3]]
    };

    assert_eq!(
        pixel(0, 0),
        [0xFF, 0x00, 0x00, 0xFF],
        "outside clear stays blue"
    );
    assert_eq!(
        pixel(3, 3),
        [0x00, 0x00, 0xFF, 0xFF],
        "clear origin tiles red"
    );
    assert_eq!(
        pixel(4, 3),
        [0x00, 0x00, 0xFF, 0xFF],
        "tile repeats horizontally inside clear"
    );
    assert_eq!(
        pixel(6, 6),
        [0x00, 0x00, 0xFF, 0xFF],
        "tile repeats over the whole cleared region"
    );
    assert_eq!(
        pixel(7, 7),
        [0xFF, 0x00, 0x00, 0xFF],
        "outside clear stays blue at bottom-right"
    );
}

/// Resizing a window that has a `bg_pixmap` must seed the fresh
/// storage from that pixmap, not from `bg_pixel`/default fill only.
/// The right-side fvwm panel exercises exactly this path when its
/// child window is resized from a small initial geometry to a tall
/// column.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn resize_with_bg_pixmap_reseeds_new_storage_from_background_pixmap() {
    use yserver_core::{
        backend::WindowHandle,
        host_x11::{HostSubwindowConfig, HostSubwindowVisual},
    };

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let tile = b.create_pixmap(None, 32, 2, 2).expect("tile");
    b.fill_rectangle(None, tile.as_raw(), 0xFFFF_0000, 0, 0, 2, 2)
        .expect("tile red");

    let root = WindowHandle::from_raw(1).expect("root");
    let window = b
        .create_subwindow(
            None,
            root,
            0,
            0,
            8,
            8,
            0,
            HostSubwindowVisual::Explicit {
                depth: 32,
                visual_xid: 0,
                colormap_xid: 0,
            },
            None,
            Some(tile.as_raw()),
        )
        .expect("window");
    let xid = window.as_raw();
    b.map_window_for_tests(xid).expect("map");

    b.fill_rectangle(None, xid, 0xFF00_00FF, 0, 0, 8, 8)
        .expect("window blue");
    b.configure_subwindow(
        None,
        xid,
        HostSubwindowConfig {
            width: Some(32),
            height: Some(32),
            ..HostSubwindowConfig::default()
        },
    )
    .expect("resize");

    let out = b
        .get_image_pixels_for_tests(xid, 2, 20, 20, 1, 1, !0)
        .expect("get_image")
        .expect("Some");
    assert_eq!(out.len(), 4, "single BGRA8 pixel");
    assert_eq!(
        &out[0..4],
        &[0x00, 0x00, 0xFF, 0xFF],
        "freshly grown storage must come from tiled bg_pixmap, not default white/black fill",
    );
}

/// Stage 3f.14 acceptance: a fresh window storage allocated with
/// `bg_pixel == None` (no `CWBackPixel` attribute) reads back as a
/// depth-appropriate safe-default colour, **not** whatever bytes
/// the pool returner left. Pre-3f.14 the alloc path skipped the
/// fill entirely when `bg_pixel.is_none()`, so the v2 PixmapPool
/// (3f.10) handed back stale content — caja's drag exhibited this
/// as widget-rect islands on black. Test: create a 16×16 depth-32
/// subwindow, register it through the Backend trait, then
/// get_image its xid and assert no pool garbage shows (step 5: the map seeds from the parent).
///
/// We don't directly exercise the pool here — the test fixture's
/// platform has no `pixmap_pool` attached, so fresh allocs always
/// come from a Vk allocator. The test still asserts the
/// fill-on-alloc invariant via the *initial* read: depth-32 →
/// `(0, 0, 0, 0)` BGRA bytes. Without the 3f.14 fill, the freshly
/// allocated Vk image would have UNDEFINED layout content and the
/// readback would be either driver-defined zero or
/// garbage — driver-dependent. The fill makes it explicit.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn window_storage_no_bg_pixel_inits_to_safe_default() {
    use yserver_core::{backend::WindowHandle, host_x11::HostSubwindowVisual};
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // create_subwindow with `background_pixel=None` +
    // `background_pixmap=None` (no CWBackPixel / CWBackPixmap on
    // the request — pre-3f.14 v2 left fresh storage at pool
    // returner content for this case).
    let parent = WindowHandle::from_raw(1).expect("root WindowHandle");
    let child = b
        .create_subwindow(
            None,
            parent,
            0, // x
            0, // y
            16,
            16,
            0, // border_width
            // Depth-32 (ARGB) needs an explicit visual config.
            HostSubwindowVisual::Explicit {
                depth: 32,
                visual_xid: 0,
                colormap_xid: 0,
            },
            None, // background_pixel
            None, // background_pixmap
        )
        .expect("create_subwindow");
    let child_xid = child.as_raw();
    b.map_window_for_tests(child_xid).expect("map");

    let out = b
        .get_image_pixels_for_tests(child_xid, 2, 0, 0, 16, 16, !0)
        .expect("get_image")
        .expect("Some");
    assert_eq!(out.len(), 16 * 16 * 4);
    // Background None: realize seeds over the safe default from the parent, the root's 0x505050.
    for (i, px) in out.chunks_exact(4).enumerate() {
        assert_eq!(
            &px[0..4],
            &[0x50, 0x50, 0x50, 0xFF],
            "fresh depth-32 storage pixel #{i} must hold the parent's pixels (got {:?})",
            &px[0..4],
        );
    }
}
