use super::*;

/// Stage 4d X11 Render `PictFormat` fix — marco-compositing widgets-
/// invisible repro.
///
/// Bug: redirected window backings end up with `α = 0x00` in their
/// storage (depth-24 padding byte) but marco's `Over` operator
/// samples that α=0 source, blends it with the dst, and produces
/// no contribution — widget contents stay invisible. The X11
/// Render spec says samples from a picture wrapping a depth-24
/// drawable must return `α = 1.0` (`PictFormat.alpha_mask = 0`).
///
/// Oracle: fill a depth-24 pixmap to a known RGB with α=0 in the
/// storage byte, then composite (`OP_SRC`) it onto a depth-32
/// pixmap pre-filled to transparent black. After the composite,
/// the dst must have `α = 0xFF` everywhere (force-opaque), not
/// `α = 0x00` (the pre-fix bug).
///
/// `OP_SRC` (op=1) was chosen because it's the simplest predicate:
/// `dst = src`. Any α-blending op would also work but introduces
/// more failure modes. The fix is exclusively shader-side, so the
/// minimal-blend op is the cleanest gate.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn render_composite_depth24_src_samples_opaque_alpha() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // Step 1: depth-24 src pixmap, 4×4. fill_rectangle with
    // foreground 0x00_AABBCC: alpha-byte = 0x00, R = 0xAA, G = 0xBB,
    // B = 0xCC. v2's RGB→storage path lays this down as BGRA
    // `[0xCC, 0xBB, 0xAA, 0x00]` — α byte 0x00, exactly the
    // depth-24 padding case the marco-compositing bug hits.
    let src_pix = b.create_pixmap(None, 24, 4, 4).expect("create src d24");
    let src_xid = src_pix.as_raw();
    b.fill_rectangle(None, src_xid, 0x00_AA_BB_CC, 0, 0, 4, 4)
        .expect("fill_rectangle src d24 with α=0 in storage");

    // Step 2: depth-32 dst pixmap, 4×4, pre-cleared to transparent
    // black (α=0). After Composite the dst's α byte is the gate:
    // pre-fix it remains 0x00 (sampled from src storage), post-fix
    // it must be 0xFF (forced by the shader on depth-24 sources).
    let dst_pix = b.create_pixmap(None, 32, 4, 4).expect("create dst d32");
    let dst_xid = dst_pix.as_raw();
    b.fill_rectangle(None, dst_xid, 0x00_00_00_00, 0, 0, 4, 4)
        .expect("fill_rectangle dst d32 to transparent black");

    // Step 3: Pictures. Default formats — the backend picks
    // depth-matched PictFormats per the standard X11 Render
    // table (depth-24 → x8r8g8b8; depth-32 → a8r8g8b8).
    let src_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(src_pix), 0, 0, &[])
        .expect("render_create_picture src")
        .expect("Some(src PictureHandle)");
    let dst_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(dst_pix), 0, 0, &[])
        .expect("render_create_picture dst")
        .expect("Some(dst PictureHandle)");

    // Step 4: Composite OP_SRC, full 4×4 cover. No mask, no
    // transform, no clip — the simplest path through
    // `RenderEngine::render_composite`. The src picture wraps a
    // depth-24 drawable; the force-opaque resolver flags it; the
    // shader pins sampled α = 1.0 (= src_uv.z, which is 1.0
    // everywhere inside the 4×4 cover); OP_SRC writes
    // `(R, G, B, 1.0)` into the dst.
    b.render_composite(
        None,
        1, // Src
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

    // Step 5: Read dst back. Every pixel's α byte must be 0xFF —
    // the post-fix invariant. Pre-fix, α would be 0x00 (the src
    // padding byte).
    let out = b
        .get_image_pixels_for_tests(dst_xid, 2, 0, 0, 4, 4, !0)
        .expect("get_image dst")
        .expect("Some(dst bytes)");
    assert_eq!(out.len(), 4 * 4 * 4, "4×4 BGRA8 readback");
    for y in 0..4 {
        for x in 0..4 {
            let off = (y * 4 + x) * 4;
            let px = &out[off..off + 4];
            // BGRA storage. The RGB channels come through from
            // src; the load-bearing assertion is α = 0xFF.
            assert_eq!(
                px[3], 0xFF,
                "dst ({x},{y}) α must be 0xFF (force-opaque); got {px:?}. \
                 Pre-fix this would be 0x00 — the depth-24 src padding byte.",
            );
        }
    }
}

/// As [`copy_into_depth24_child_of_depth32_backing_writes_opaque_alpha`],
/// but through a bitmap clip mask, which routes CopyArea to the GPU
/// masked blit — a second raw-copy path with the same leak. The mask's
/// excluded half is only checked for its colour: every pixel of the child
/// is opaque in the backing, so alpha there is not the masked copy's to
/// decide.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn masked_copy_into_depth24_child_of_depth32_backing_writes_opaque_alpha() {
    use yserver_core::{
        backend::{ClipState, PixmapHandle as ApplyPixmapHandle, WindowHandle},
        host_x11::HostSubwindowVisual,
    };

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let root = WindowHandle::from_raw(1).expect("root");
    let visual = |depth| HostSubwindowVisual::Explicit {
        depth,
        visual_xid: 0,
        colormap_xid: 0,
    };
    let p = b
        .create_subwindow(None, root, 0, 0, 64, 64, 0, visual(32), None, None)
        .expect("depth-32 parent");
    b.map_window_for_tests(p.as_raw()).expect("map P");
    let backing = b.create_pixmap(None, 32, 64, 64).expect("P backing");
    assert!(b.test_set_redirected_target(p.as_raw(), backing.as_raw()));
    let c = b
        .create_subwindow(None, p, 8, 8, 16, 16, 0, visual(24), None, None)
        .expect("depth-24 child");
    b.map_window_for_tests(c.as_raw()).expect("map C");
    b.fill_rectangle(None, backing.as_raw(), 0x0000_0000, 0, 0, 64, 64)
        .expect("clear P backing to transparent");

    let src = b.create_pixmap(None, 24, 16, 16).expect("depth-24 source");
    b.engine_put_image_for_tests(
        src.as_raw(),
        ash::vk::Offset2D { x: 0, y: 0 },
        ash::vk::Extent2D {
            width: 16,
            height: 16,
        },
        &[0x10u8, 0x20, 0x30, 0x00].repeat(16 * 16),
        32,
    )
    .expect("upload source with X byte 0");

    // Clip mask: top 8 rows copy, bottom 8 do not.
    let mask = b.create_pixmap(None, 1, 16, 16).unwrap().as_raw();
    let mut bits = vec![0u8; 4 * 16];
    for row in 0..8 {
        bits[row * 4] = 0xFF;
        bits[row * 4 + 1] = 0xFF;
    }
    b.put_image(None, mask, 1, 16, 16, 0, 0, &bits)
        .expect("put_image mask");
    b.apply_clip_state(
        None,
        &ClipState::Pixmap {
            origin: (0, 0),
            pixmap: ApplyPixmapHandle::from_raw(mask).expect("mask handle"),
        },
    )
    .expect("apply_clip_state Pixmap");

    let pre = b.telemetry().lifetime.copy_area_masked_draw;
    b.copy_area(None, src.as_raw(), c.as_raw(), 0, 0, 0, 0, 16, 16)
        .expect("copy into C");
    assert_eq!(
        b.telemetry().lifetime.copy_area_masked_draw - pre,
        1,
        "must exercise the masked-blit route, or this test proves nothing"
    );

    let px = b
        .get_image_pixels_for_tests(backing.as_raw(), 2, 8, 8, 16, 16, !0)
        .expect("get_image")
        .expect("pixels");
    for (i, p) in px.chunks_exact(4).enumerate() {
        let row = i / 16;
        if row < 8 {
            assert_eq!(
                &p[..3],
                &[0x10, 0x20, 0x30],
                "pixel {i}: masked-in colour must land"
            );
            assert_eq!(
                p[3], 0xFF,
                "pixel {i}: depth-24 child must be opaque in the depth-32 backing"
            );
        } else {
            assert_eq!(
                &p[..3],
                &[0, 0, 0],
                "pixel {i}: masked-out colour must not change"
            );
        }
    }
}

/// A depth-24 window that paints into its depth-32 parent's redirect
/// backing must come out OPAQUE there, whatever its source's undefined X
/// byte says. X gives the window no alpha, and Xorg composites such a
/// child into its parent with alpha forced to 1 (`compWindowUpdateAutomatic`,
/// composite/compwindow.c). A GL client's buffer commonly has 0 in that
/// byte: glxgears under awesome + picom showed through to stale frames,
/// because a raw copy put alpha 0 into the depth-32 frame backing that
/// picom binds with alpha.
///
/// The backing is cleared to transparent first, so a fix that merely
/// masks alpha out of the write (and so keeps whatever was there) still
/// fails.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn copy_into_depth24_child_of_depth32_backing_writes_opaque_alpha() {
    use yserver_core::{backend::WindowHandle, host_x11::HostSubwindowVisual};

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let root = WindowHandle::from_raw(1).expect("root");

    // Depth-32 parent P (an ARGB WM frame), redirected.
    let p = b
        .create_subwindow(
            None,
            root,
            0,
            0,
            64,
            64,
            0,
            HostSubwindowVisual::Explicit {
                depth: 32,
                visual_xid: 0,
                colormap_xid: 0,
            },
            None,
            None,
        )
        .expect("depth-32 parent");
    b.map_window_for_tests(p.as_raw()).expect("map P");
    let backing = b.create_pixmap(None, 32, 64, 64).expect("P backing");
    assert!(
        b.test_set_redirected_target(p.as_raw(), backing.as_raw()),
        "redirect route must be recorded"
    );

    // Depth-24 child C at (8, 8): not redirected, so it paints into P's backing.
    let c = b
        .create_subwindow(
            None,
            p,
            8,
            8,
            16,
            16,
            0,
            HostSubwindowVisual::Explicit {
                depth: 24,
                visual_xid: 0,
                colormap_xid: 0,
            },
            None,
            None,
        )
        .expect("depth-24 child");
    b.map_window_for_tests(c.as_raw()).expect("map C");

    // Transparent backing, after any map-time background paint.
    b.fill_rectangle(None, backing.as_raw(), 0x0000_0000, 0, 0, 64, 64)
        .expect("clear P backing to transparent");

    // Depth-24 source whose X byte is 0, as a GL client's buffer has it.
    // Uploaded as depth-32 bytes so nothing forces the alpha on the way in.
    let src = b.create_pixmap(None, 24, 16, 16).expect("depth-24 source");
    let bgra: Vec<u8> = [0x10u8, 0x20, 0x30, 0x00].repeat(16 * 16);
    b.engine_put_image_for_tests(
        src.as_raw(),
        ash::vk::Offset2D { x: 0, y: 0 },
        ash::vk::Extent2D {
            width: 16,
            height: 16,
        },
        &bgra,
        32,
    )
    .expect("upload source with X byte 0");

    // The PresentPixmap fallback is exactly this: CopyArea with a default GC.
    b.copy_area(None, src.as_raw(), c.as_raw(), 0, 0, 0, 0, 16, 16)
        .expect("copy into C");

    let px = b
        .get_image_pixels_for_tests(backing.as_raw(), 2, 8, 8, 16, 16, !0)
        .expect("get_image")
        .expect("pixels");
    for (i, p) in px.chunks_exact(4).enumerate() {
        assert_eq!(
            [p[0], p[1], p[2]],
            [0x10, 0x20, 0x30],
            "pixel {i}: the copy's colour must land"
        );
        assert_eq!(
            p[3], 0xFF,
            "pixel {i}: depth-24 child must be opaque in the depth-32 backing"
        );
    }
}

/// Destination half of the same rule — sibling to
/// `render_composite_depth24_src_samples_opaque_alpha` above, which
/// covers the SOURCE side.
///
/// Bug: on X11 a depth-24 drawable has no alpha channel and is opaque
/// by definition. Xorg gets that for free — pixman's `x8r8g8b8` has
/// zero alpha bits, so a store drops the channel
/// (`pixman-access.c:254-261`) and every fetch substitutes `0xff`
/// (`:270-276`); RENDER states it outright, treating
/// `PICT_FORMAT_A(pDst->format) == 0` as "the destination alpha is
/// always 1" (`render/picture.c:1456-1457`, `:1487-1488`). We store
/// depth-24 as `B8G8R8A8_UNORM`, which has a real alpha byte, and the
/// composite pipeline wrote it: a `PictOpSrc` from a half-transparent
/// source left `α = 127` in the backing. A depth-32 compositing
/// client then blends a hole X11 says cannot exist.
///
/// Oracle values are the measured mate-terminal frame backing: body
/// BGRA `(27, 21, 0)` with `α = 127` where it must be 255, and
/// regions the client never painted reading `(0, 0, 0, 0)` instead of
/// opaque black.
///
/// Storage-level, not a round trip through our own encoder:
/// `get_image` on a depth-24 drawable is a verbatim memcpy of the
/// BGRA8 storage (`pack_from_storage`'s `32 | 24` arm), so `px[3]`
/// IS the stored alpha byte.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn render_composite_depth24_dst_keeps_opaque_alpha() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // Step 1: depth-24 destination, 4×4, straight out of
    // `create_pixmap` — NO paint of any kind yet.
    let dst_pix = b.create_pixmap(None, 24, 4, 4).expect("create dst d24");
    let dst_xid = dst_pix.as_raw();

    let fresh = b
        .get_image_pixels_for_tests(dst_xid, 2, 0, 0, 4, 4, !0)
        .expect("get_image fresh dst")
        .expect("Some(fresh dst bytes)");
    assert_eq!(fresh.len(), 4 * 4 * 4, "4×4 BGRA8 readback");
    for (i, px) in fresh.chunks_exact(4).enumerate() {
        assert_eq!(
            px[3], 0xFF,
            "fresh depth-24 pixmap pixel {i} must be OPAQUE before any paint; got {px:?}. \
             A depth-24 drawable has no alpha channel — 0x00 here is a hole X11 says \
             cannot exist.",
        );
    }

    // Step 2: depth-32 source carrying the measured mate-terminal
    // body pixel — X11 wire 0xAARRGGBB = 0x7F_00_15_1B, i.e. BGRA
    // storage [0x1B, 0x15, 0x00, 0x7F]: RGB (0, 21, 27), α = 127.
    let src_pix = b.create_pixmap(None, 32, 4, 4).expect("create src d32");
    let src_xid = src_pix.as_raw();
    b.fill_rectangle(None, src_xid, 0x7F_00_15_1B, 0, 0, 4, 4)
        .expect("fill_rectangle src d32 with α=127");

    let src_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(src_pix), 0, 0, &[])
        .expect("render_create_picture src")
        .expect("Some(src PictureHandle)");
    let dst_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(dst_pix), 0, 0, &[])
        .expect("render_create_picture dst")
        .expect("Some(dst PictureHandle)");

    // Step 3: Composite OP_SRC over the TOP-LEFT 2×2 only. `dst = src`
    // is the simplest predicate for the write side, and the partial
    // cover leaves the right/bottom of the destination as the
    // "region the client never painted" case.
    b.render_composite(
        None,
        1, // Src
        src_pic.as_raw(),
        0,
        dst_pic.as_raw(),
        0,
        0,
        0,
        0,
        0,
        0,
        2,
        2,
    )
    .expect("render_composite");

    let out = b
        .get_image_pixels_for_tests(dst_xid, 2, 0, 0, 4, 4, !0)
        .expect("get_image dst")
        .expect("Some(dst bytes)");
    assert_eq!(out.len(), 4 * 4 * 4, "4×4 BGRA8 readback");

    for y in 0..4usize {
        for x in 0..4usize {
            let off = (y * 4 + x) * 4;
            let px = &out[off..off + 4];
            let covered = x < 2 && y < 2;
            assert_eq!(
                px[3], 0xFF,
                "dst ({x},{y}) α must be 0xFF; got {px:?}. Covered={covered}. \
                 Pre-fix the covered pixels read 0x7F (=127, the measured \
                 mate-terminal failure) because the composite stored the \
                 source's alpha into a drawable that has no alpha channel.",
            );
            if covered {
                // OP_SRC copies the premultiplied source through.
                // ±1 for the UNORM8 → float → UNORM8 round trip.
                for (ch, want) in [(0usize, 27u8), (1, 21), (2, 0)] {
                    assert!(
                        px[ch].abs_diff(want) <= 1,
                        "dst ({x},{y}) channel {ch} want ≈{want}, got {px:?}",
                    );
                }
            } else {
                assert_eq!(
                    &px[0..3],
                    &[0u8, 0, 0],
                    "dst ({x},{y}) is outside the composite rect and must still be \
                     the create_pixmap init colour; got {px:?}",
                );
            }
        }
    }
}

/// Scene-path α-leak fix — sibling to
/// `render_composite_depth24_src_samples_opaque_alpha` above,
/// covering the scene compositor side instead of the engine RENDER
/// side.
///
/// Bug: `Storage::image_view` is created with IDENTITY component
/// swizzle (required by VUID-VkFramebufferCreateInfo-pAttachments-00891
/// because the same view doubles as a colour attachment). The
/// engine's RENDER path avoids the depth-24 α-leak by sampling via
/// a separate cached view with `BgraNoAlpha` swizzle
/// (`engine::ensure_drawable_view`), but the scene compositor binds
/// `storage.image_view` directly in every `CompositeDraw`
/// (`scene::build_scene` four sites — root, window subtree, COW,
/// cursor). With `alpha_passthrough=true` on window draws, the
/// shader samples raw padding bytes as α; for a depth-24 BGRA8
/// drawable that has been filled with α-byte = 0 in storage (any
/// `put_image` of `0x00RRGGBB` wire bytes, the depth-24 default),
/// the scene blends with α=0 and the layer below shows through —
/// matching the `mate-with-compositing wallpaper bleeds through
/// COW` and `bits appear/disappear` symptoms.
///
/// Fix: `Storage` carries a second view `sample_view` built with
/// format-aware swizzle (α=ONE for depth-24 BGRA8). Scene draws
/// bind `sample_view`. This test only proves the field exists,
/// differs from `image_view` for a depth-24 drawable, and is a
/// real (non-null) `vk::ImageView`. End-to-end pixel-level scene
/// verification needs scanout-dump test scaffolding the v2
/// acceptance harness does not yet have — but the swizzle helper
/// itself is the load-bearing piece and is also covered by
/// engine-side composite tests.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn storage_depth24_has_distinct_sample_view() {
    use ash::vk;

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // Depth-24 pixmap — the case where the BgraNoAlpha swizzle
    // (α=ONE) must differ from the identity attachment view.
    let pix24 = b.create_pixmap(None, 24, 4, 4).expect("create d24");
    let views24 = b
        .test_storage_views(pix24.as_raw())
        .expect("d24 storage resolves");
    assert_ne!(
        views24.0,
        vk::ImageView::null(),
        "d24 image_view must be non-null",
    );
    assert_ne!(
        views24.1,
        vk::ImageView::null(),
        "d24 sample_view must be non-null after the scene-α fix \
         (pre-fix: sample_view field did not exist, scene bound \
         image_view directly with identity swizzle, depth-24 \
         padding α leaked)",
    );
    assert_ne!(
        views24.0, views24.1,
        "d24 sample_view must be a different VkImageView than \
         image_view (different ComponentMapping — α=ONE vs \
         IDENTITY). Same handle would mean either the fix \
         wasn't applied or the format-aware swizzle defaulted \
         to identity for BGRA8/depth-24.",
    );

    // Depth-32 pixmap — sample_view's swizzle is also identity
    // (real α passes through), so the *swizzle* is the same as
    // image_view, but they must still be distinct VkImageView
    // handles (the attachment view must keep IDENTITY swizzle
    // unconditionally per VUID 00891, and the sample_view is
    // owned/destroyed separately by Storage). Asserting non-null
    // proves the plumbing is wired for depth-32 too.
    let pix32 = b.create_pixmap(None, 32, 4, 4).expect("create d32");
    let views32 = b
        .test_storage_views(pix32.as_raw())
        .expect("d32 storage resolves");
    assert_ne!(views32.0, vk::ImageView::null());
    assert_ne!(views32.1, vk::ImageView::null());
}
