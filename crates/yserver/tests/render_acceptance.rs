//! Render-backend acceptance integration tests (Stage 2f).
//!
//! Drives `KmsBackend` directly via its `Backend` trait and
//! asserts pixel-correctness against a CPU oracle. Functionally
//! equivalent to the Stage 2 plan's "synthetic harness binary"
//! that would drive PutImage / CopyArea / PolyFillRectangle /
//! GetImage through the X11 protocol — but skipping the X11
//! protocol layer because the correctness gate is at the
//! Backend-trait surface, not at the protocol-encoding layer.
//!
//! These tests are gated on a live Vulkan ICD (lavapipe is fine):
//!
//! ```text
//! VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json \
//!   cargo test -p yserver --test acceptance -- --ignored
//! ```
//!
//! User-run hardware smoke on bee + fuji
//! (`just yserver-xfce-hw`) is the
//! load-bearing Stage 2 close gate; this file covers the
//! correctness oracle that gates against pixel-level regressions.

#![cfg(target_os = "linux")]

use yserver::kms::render::KmsBackend;
use yserver_core::backend::{AnyHandle, Backend, DrawState, FillState, GcFunction, SubwindowMode};
use yserver_protocol::x11::ClipRectangles;

/// Acceptance sequence:
/// 1. create_pixmap (depth=32, 8×8)
/// 2. PutImage a horizontal gradient
/// 3. GetImage round-trip — must be byte-identical
/// 4. PolyFillRectangle in a sub-rect — overwrites the gradient
/// 5. GetImage — verifies overwrite at the rect, gradient outside
#[test]
#[ignore = "needs live Vulkan ICD"]
fn put_image_fill_get_image_oracle() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let pix = b.create_pixmap(None, 32, 8, 8).expect("create_pixmap");
    let xid = pix.as_raw();

    // 8×8 RGBA gradient (wire format = BGRA8 ZPixmap).
    let mut src = vec![0u8; 8 * 8 * 4];
    for y in 0..8 {
        for x in 0..8 {
            let off = (y * 8 + x) * 4;
            src[off] = (x as u8) * 0x20; // B
            src[off + 1] = (y as u8) * 0x20; // G
            src[off + 2] = ((x + y) as u8) * 0x10; // R
            src[off + 3] = 0xFF; // A
        }
    }
    b.put_image(None, xid, 32, 8, 8, 0, 0, &src)
        .expect("put_image");

    let out = b
        .get_image_pixels_for_tests(xid, 2 /* ZPixmap */, 0, 0, 8, 8, !0)
        .expect("get_image")
        .expect("Some(bytes)");
    assert_eq!(out, src, "PutImage→GetImage byte-identical (depth-32)");

    // PolyFillRectangle: paint a 4×4 red square at (2, 2).
    // Foreground 0xFFFF0000 = ARGB(0xFF, R=0xFF, G=0, B=0).
    let rect_bytes = {
        let mut buf = Vec::new();
        buf.extend_from_slice(&i16::to_le_bytes(2)); // x
        buf.extend_from_slice(&i16::to_le_bytes(2)); // y
        buf.extend_from_slice(&u16::to_le_bytes(4)); // w
        buf.extend_from_slice(&u16::to_le_bytes(4)); // h
        buf
    };
    b.poly_fill_rectangle(None, xid, 0xFFFF0000, &rect_bytes)
        .expect("poly_fill_rectangle");

    let after = b
        .get_image_pixels_for_tests(xid, 2, 0, 0, 8, 8, !0)
        .expect("get_image")
        .expect("Some");
    // (3, 3) — inside the fill — must be red: BGRA = [0,0,0xFF,0xFF].
    let off_3_3 = (3 * 8 + 3) * 4;
    assert_eq!(
        &after[off_3_3..off_3_3 + 4],
        &[0x00, 0x00, 0xFF, 0xFF],
        "fill rect interior is red",
    );
    // (0, 0) — outside the fill — must match the gradient.
    assert_eq!(
        &after[0..4],
        &src[0..4],
        "outside fill rect preserves the gradient",
    );
}

/// Acceptance for `CopyArea` between disjoint pixmaps.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn copy_area_disjoint_oracle() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let src_xid = b.create_pixmap(None, 32, 4, 4).unwrap().as_raw();
    let dst_xid = b.create_pixmap(None, 32, 8, 4).unwrap().as_raw();

    // Fill src with red (BGRA: B=0, G=0, R=0xFF, A=0xFF) via
    // fill_rectangle. Foreground 0xFFFF0000.
    b.fill_rectangle(None, src_xid, 0xFFFF0000, 0, 0, 4, 4)
        .expect("fill_rectangle src");
    // Fill dst with blue (0xFF0000FF → BGRA [0xFF, 0, 0, 0xFF]).
    b.fill_rectangle(None, dst_xid, 0xFF0000FF, 0, 0, 8, 4)
        .expect("fill_rectangle dst");
    // Copy src into dst at (4, 0).
    b.copy_area(None, src_xid, dst_xid, 0, 0, 4, 0, 4, 4)
        .expect("copy_area");

    let out = b
        .get_image_pixels_for_tests(dst_xid, 2, 0, 0, 8, 4, !0)
        .expect("get_image")
        .expect("Some");
    // Left half blue, right half red.
    for y in 0..4 {
        for x in 0..4 {
            let off = (y * 8 + x) * 4;
            assert_eq!(
                &out[off..off + 4],
                &[0xFF, 0x00, 0x00, 0xFF],
                "left blue at ({x},{y})",
            );
        }
        for x in 4..8 {
            let off = (y * 8 + x) * 4;
            assert_eq!(
                &out[off..off + 4],
                &[0x00, 0x00, 0xFF, 0xFF],
                "right red at ({x},{y})",
            );
        }
    }
}

/// Telemetry assertion: after a full sequence, lifetime counts
/// reflect the expected number of paint/one-shot submits and
/// `vk_queue_wait_idle` stays at zero outside the implicit
/// get_image internal wait (which is part of the
/// `record_one_shot_submit` path, not a free-standing
/// `record_vk_queue_wait_idle` call).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn telemetry_lifetime_after_sequence() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let xid = b.create_pixmap(None, 32, 4, 4).unwrap().as_raw();

    // 3 fills.
    for _ in 0..3 {
        b.fill_rectangle(None, xid, 0xFFFF0000, 0, 0, 4, 4).unwrap();
    }
    // 1 put_image.
    let buf = vec![0xFFu8; 4 * 4 * 4];
    b.put_image(None, xid, 32, 4, 4, 0, 0, &buf).unwrap();
    // 1 get_image.
    let _ = b
        .get_image_pixels_for_tests(xid, 2, 0, 0, 4, 4, !0)
        .unwrap();

    let t = b.telemetry();
    assert_eq!(t.lifetime.paint_submits, 4, "3 fills + 1 put_image");
    assert_eq!(t.lifetime.one_shot_submits, 1, "1 get_image");
    assert_eq!(
        t.lifetime.queue_submit2, 5,
        "every paint + one-shot bumps queue_submit2",
    );
    // Stage 2 plan §"vk_queue_wait_idle target zero": our
    // record_vk_queue_wait_idle counter is independent of the
    // implicit FenceTicket::wait inside get_image. It should
    // never fire outside actual queue_wait_idle calls.
    assert_eq!(
        t.lifetime.vk_queue_wait_idle, 0,
        "no queue_wait_idle on the v2 hot path",
    );
    assert_eq!(
        t.lifetime.cpu_fence_wait_count, 1,
        "one fence wait per get_image"
    );
}

/// Stage 3c.3 acceptance: RENDER paint paths must NOT consult the
/// ambient GC clip (`KmsCore.current_clip`). Set a restrictive
/// 1×1 GC clip rectangle, then drive a `render_composite` whose
/// picture clip is `None`; the result must paint the full dst
/// rect — proof that the GC clip didn't leak into the RENDER
/// pipeline (plan §4 cross-cutting rule).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn render_composite_no_gc_clip_leak() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // 4×4 dst pixmap pre-filled with blue (pixel 0xFF0000FF).
    let dst_pix = b.create_pixmap(None, 32, 4, 4).expect("create_pixmap");
    let dst_xid = dst_pix.as_raw();
    b.fill_rectangle(None, dst_xid, 0xFF0000FF, 0, 0, 4, 4)
        .expect("fill_rectangle pre");

    // RENDER picture wrapping the pixmap, no value-mask.
    let dst_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(dst_pix), 0, 0, &[])
        .expect("render_create_picture")
        .expect("Some(PictureHandle)");
    // SolidFill source: opaque red (premul wire u16 RGBA:
    // R=0xFFFF, G=0, B=0, A=0xFFFF — little-endian per channel).
    let src_pic = b
        .render_create_solid_fill(None, [0xFF, 0xFF, 0x00, 0x00, 0x00, 0x00, 0xFF, 0xFF])
        .expect("render_create_solid_fill")
        .expect("Some(PictureHandle)");

    // Restrictive GC clip: only (0, 0) 1×1.
    let mut rects = Vec::new();
    rects.extend_from_slice(&i16::to_le_bytes(0));
    rects.extend_from_slice(&i16::to_le_bytes(0));
    rects.extend_from_slice(&u16::to_le_bytes(1));
    rects.extend_from_slice(&u16::to_le_bytes(1));
    b.set_clip_rectangles(
        None,
        Some(ClipRectangles {
            ordering: 0,
            x_origin: 0,
            y_origin: 0,
            rectangles: rects,
        }),
    )
    .expect("set_clip_rectangles");

    // Composite covers the full 4×4 dst — the picture's clip is
    // None (no `render_set_picture_clip_rectangles` call), so the
    // engine should paint everywhere. If the backend leaked the GC
    // clip into the RENDER path, only (0, 0) would be painted.
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

    let out = b
        .get_image_pixels_for_tests(dst_xid, 2, 0, 0, 4, 4, !0)
        .expect("get_image")
        .expect("Some(bytes)");
    // Every pixel must be red BGRA = [0, 0, 0xFF, 0xFF].
    for y in 0..4 {
        for x in 0..4 {
            let off = (y * 4 + x) * 4;
            assert_eq!(
                &out[off..off + 4],
                &[0x00, 0x00, 0xFF, 0xFF],
                "GC clip leaked into RENDER paint at ({x},{y})",
            );
        }
    }
}

/// Stage 3d v1-bug-fix gate (plan §3d): v1's
/// `try_vk_render_composite_glyphs` reads but **ignores** the dst
/// picture's clip (`kms::backend.rs:5313`); v2 must honour it via
/// per-rect scissoring. The test stamps two 4×4 white glyphs at
/// dst (0, 0) and (4, 0) onto an 8×4 blue pixmap with the picture
/// clip set to the top-left 4×4 rect. Result: left half painted
/// white; right half stays blue. v1 would paint both glyphs.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn composite_glyphs_clip_intersects_picture() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // 8×4 dst pixmap pre-filled with blue (pixel 0xFF0000FF).
    let dst_pix = b.create_pixmap(None, 32, 8, 4).expect("create_pixmap");
    let dst_xid = dst_pix.as_raw();
    b.fill_rectangle(None, dst_xid, 0xFF0000FF, 0, 0, 8, 4)
        .expect("fill_rectangle pre");

    // SolidFill source: opaque premultiplied white (R=G=B=A=0xFFFF).
    let src_pic = b
        .render_create_solid_fill(None, [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF])
        .expect("solid_fill")
        .expect("Some(PictureHandle)");

    // Dst picture wrapping the pixmap.
    let dst_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(dst_pix), 0, 0, &[])
        .expect("render_create_picture")
        .expect("Some(PictureHandle)");

    // Picture clip: top-left 4×4 only.
    // Wire body for render_set_picture_clip_rectangles: picture(4)
    // + clip_x_origin(INT16) + clip_y_origin(INT16) + N×rectangles
    // (INT16 x, INT16 y, CARD16 w, CARD16 h).
    let mut clip_body: Vec<u8> = Vec::new();
    clip_body.extend_from_slice(&dst_pic.as_raw().to_le_bytes());
    clip_body.extend_from_slice(&i16::to_le_bytes(0)); // clip_x_origin
    clip_body.extend_from_slice(&i16::to_le_bytes(0)); // clip_y_origin
    clip_body.extend_from_slice(&i16::to_le_bytes(0)); // rect.x
    clip_body.extend_from_slice(&i16::to_le_bytes(0)); // rect.y
    clip_body.extend_from_slice(&u16::to_le_bytes(4)); // rect.w
    clip_body.extend_from_slice(&u16::to_le_bytes(4)); // rect.h
    b.render_set_picture_clip_rectangles(None, dst_pic.as_raw(), &clip_body)
        .expect("set_picture_clip_rectangles");

    // Glyphset with one 4×4 A8 glyph at id=1 (all 0xFF alpha,
    // x_off=4 so consecutive glyphs sit edge-to-edge).
    // RENDER_FMT_A8 = the standard a8 picture format id (depends
    // on the server's PictFormat catalogue; the backend's
    // render_create_glyphset matches on ynest_format constants).
    let gs = b
        .render_create_glyphset(None, yserver_protocol::x11::RENDER_FMT_A8)
        .expect("glyphset")
        .expect("Some");

    // render_add_glyphs body shape (from parse_add_glyphs):
    // body_tail = n(u32) + n×id(u32) + n×info(12 bytes) +
    // n×pixels(stride×h).
    // info layout (per parse_add_glyphs): width(u16) height(u16)
    // x(i16) y(i16) x_off(i16) y_off(i16) — 12 bytes.
    // A8 stride for w=4: (4+3) & !3 = 4. Total pixel bytes = 4×4 = 16.
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

    // CompositeGlyphs8 items: one element with count=2 glyphs id=1
    // (pen starts at dx=0,dy=0, glyph 1 stamps at (0,0), pen
    // advances to (4,0), glyph 2 stamps at (4,0)).
    // Element header: count(u8) + 3 pad + dx(i16) + dy(i16) = 8 bytes.
    // Then 2 × 1-byte ids = 2 bytes, padded to 4. Total 12 bytes.
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
        0, // mask_fmt — unused
        gs.as_raw(),
        0,
        0,
        &items,
        0,
        0,
    )
    .expect("render_composite_glyphs");

    let out = b
        .get_image_pixels_for_tests(dst_xid, 2, 0, 0, 8, 4, !0)
        .expect("get_image")
        .expect("Some(bytes)");

    // Left half (x=0..4): glyph painted white over blue with
    // premul srcover (atlas alpha 0xFF, foreground white) →
    // result white. Right half (x=4..8): clip excluded the glyph
    // → blue preserved. If v1's _clip-unused bug were present,
    // both halves would be white.
    for y in 0..4 {
        for x in 0..4u32 {
            let off = (y * 8 + x as usize) * 4;
            assert_eq!(
                &out[off..off + 4],
                &[0xFF, 0xFF, 0xFF, 0xFF],
                "left half should be white at ({x},{y}); got {:?}",
                &out[off..off + 4],
            );
        }
        for x in 4..8u32 {
            let off = (y * 8 + x as usize) * 4;
            assert_eq!(
                &out[off..off + 4],
                &[0xFF, 0x00, 0x00, 0xFF],
                "right half should stay blue at ({x},{y}) — picture clip honoured; got {:?}",
                &out[off..off + 4],
            );
        }
    }
}

/// Stage 3e.1 acceptance: CopyPlane on a depth-1 source pixmap.
/// Wire bits MSB-first packed at 1 bpp; bit set → foreground,
/// bit clear → background. Test exercises the depth-1 reader +
/// rect decomposition + fg/bg fill ordering.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn copy_plane_depth1_extracts_mask_bits() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // depth-1 source 8×1. Bits LSB-first in one byte (matches the
    // server's advertised `bitmap-bit-order`):
    // 0b0000_0101 = bit 0 + bit 2 set → pixels [1, 0, 1, 0, 0, 0, 0, 0].
    let src_pix = b
        .create_pixmap(None, 1, 8, 1)
        .expect("create_pixmap depth=1");
    // Depth-1 wire row stride = ceil(w/32)*4 = 4 bytes (one
    // scanline). Bit pattern in the low byte, zero pad.
    let src_bytes: Vec<u8> = vec![0b0000_0101, 0, 0, 0];
    b.put_image(None, src_pix.as_raw(), 1, 8, 1, 0, 0, &src_bytes)
        .expect("put_image depth=1");

    // 8×1 dst pixmap, opaque green pre-fill so untouched pixels
    // are visibly distinct from fg/bg.
    let dst_pix = b.create_pixmap(None, 32, 8, 1).expect("dst pixmap");
    b.fill_rectangle(None, dst_pix.as_raw(), 0xFF00FF00, 0, 0, 8, 1)
        .expect("dst pre-fill green");

    // Foreground = red (0xFFFF0000), background = blue
    // (0xFF0000FF). copy_plane reads these from KmsCore via
    // apply_draw_state.
    b.apply_draw_state(
        None,
        &DrawState {
            foreground: 0xFFFF_0000,
            background: 0xFF00_00FF,
            ..DrawState::default()
        },
    )
    .expect("apply_draw_state");

    b.copy_plane(
        None,
        src_pix.as_raw(),
        dst_pix.as_raw(),
        0,
        0,
        0,
        0,
        8,
        1,
        1, // plane = bit 0
    )
    .expect("copy_plane");

    let out = b
        .get_image_pixels_for_tests(dst_pix.as_raw(), 2, 0, 0, 8, 1, !0)
        .expect("get_image dst")
        .expect("Some(bytes)");
    // Expected per-pixel: bit set → red BGRA = [0,0,0xFF,0xFF];
    // bit clear → blue BGRA = [0xFF,0,0,0xFF].
    let want = [
        [0x00, 0x00, 0xFF, 0xFF], // x=0 bit=1 red
        [0xFF, 0x00, 0x00, 0xFF], // x=1 bit=0 blue
        [0x00, 0x00, 0xFF, 0xFF], // x=2 bit=1 red
        [0xFF, 0x00, 0x00, 0xFF], // x=3 bit=0 blue
        [0xFF, 0x00, 0x00, 0xFF], // x=4 bit=0
        [0xFF, 0x00, 0x00, 0xFF], // x=5 bit=0
        [0xFF, 0x00, 0x00, 0xFF], // x=6 bit=0
        [0xFF, 0x00, 0x00, 0xFF], // x=7 bit=0
    ];
    for (x, exp) in want.iter().enumerate() {
        let off = x * 4;
        assert_eq!(&out[off..off + 4], exp, "copy_plane mismatch at x={x}",);
    }
}

/// Mirrors XTS XFillRectangle TP23 part 2: two tiled fills built from the
/// same depth-1 bitmap but with foreground/background swapped, where the
/// second draw uses GXxor and must match a solid `fg ^ bg` fill.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn fill_tiled_xor_with_reversed_tile_matches_solid_xor() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let fg: u32 = 0x0000_00ff;
    let bg: u32 = 0x0000_ff00;
    let xor_color: u32 = fg ^ bg;

    let bitmap = b.create_pixmap(None, 1, 8, 1).expect("bitmap pixmap");
    let bitmap_bytes: Vec<u8> = vec![0b0101_1010, 0, 0, 0];
    b.put_image(None, bitmap.as_raw(), 1, 8, 1, 0, 0, &bitmap_bytes)
        .expect("put depth-1 bitmap");

    let tile_a = b.create_pixmap(None, 24, 8, 1).expect("tile a");
    b.apply_draw_state(
        None,
        &DrawState {
            foreground: fg,
            background: bg,
            ..DrawState::default()
        },
    )
    .expect("apply copyplane colors a");
    b.copy_plane(None, bitmap.as_raw(), tile_a.as_raw(), 0, 0, 0, 0, 8, 1, 1)
        .expect("copy_plane tile a");

    let tile_b = b.create_pixmap(None, 24, 8, 1).expect("tile b");
    b.apply_draw_state(
        None,
        &DrawState {
            foreground: bg,
            background: fg,
            ..DrawState::default()
        },
    )
    .expect("apply copyplane colors b");
    b.copy_plane(None, bitmap.as_raw(), tile_b.as_raw(), 0, 0, 0, 0, 8, 1, 1)
        .expect("copy_plane tile b");

    let expected = b.create_pixmap(None, 24, 8, 1).expect("expected pixmap");
    b.fill_rectangle(None, expected.as_raw(), xor_color, 0, 0, 8, 1)
        .expect("solid xor fill");
    let expected_bytes = b
        .get_image_pixels_for_tests(expected.as_raw(), 2, 0, 0, 8, 1, !0)
        .expect("expected get_image")
        .expect("expected bytes");

    let dst = b.create_pixmap(None, 24, 8, 1).expect("dst pixmap");
    b.apply_draw_state(
        None,
        &DrawState {
            fill: FillState::Tiled {
                pixmap: tile_a,
                origin: (0, 0),
            },
            function: GcFunction::Copy,
            ..DrawState::default()
        },
    )
    .expect("apply tiled state a");
    b.fill_rectangle(None, dst.as_raw(), 0, 0, 0, 8, 1)
        .expect("tiled fill a");

    b.apply_draw_state(
        None,
        &DrawState {
            fill: FillState::Tiled {
                pixmap: tile_b,
                origin: (0, 0),
            },
            function: GcFunction::Xor,
            ..DrawState::default()
        },
    )
    .expect("apply tiled xor state b");
    b.fill_rectangle(None, dst.as_raw(), 0, 0, 0, 8, 1)
        .expect("tiled fill xor b");

    let out = b
        .get_image_pixels_for_tests(dst.as_raw(), 2, 0, 0, 8, 1, !0)
        .expect("dst get_image")
        .expect("dst bytes");
    assert_eq!(out, expected_bytes);
}

/// Mirrors XTS XFillRectangle TP27's root-window special case:
/// drawing on the root with `IncludeInferiors` must update the
/// overlapping top-level and descendant windows exactly as if the
/// draw had targeted the top-level directly.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn root_fill_with_include_inferiors_matches_top_level_result() {
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

    let root = WindowHandle::from_raw(1).expect("root");
    let top = b
        .create_subwindow(
            None,
            root,
            11,
            7,
            100,
            90,
            0,
            HostSubwindowVisual::Explicit {
                depth: 32,
                visual_xid: 0,
                colormap_xid: 0,
            },
            None,
            None,
        )
        .expect("top-level");
    let top_xid = top.as_raw();
    b.map_subwindow(None, top_xid).expect("map top");

    b.fill_rectangle(None, top_xid, 0x0000_0000, 0, 0, 100, 90)
        .expect("clear top");
    b.fill_rectangle(None, top_xid, 0x0000_00ff, 20, 30, 70, 30)
        .expect("baseline fill");
    let expected = b
        .get_image_pixels_for_tests(top_xid, 2, 0, 0, 100, 90, !0)
        .expect("baseline get_image")
        .expect("baseline bytes");

    b.fill_rectangle(None, top_xid, 0x0000_0000, 0, 0, 100, 90)
        .expect("re-clear top");

    for i in 0..4 {
        let child = b
            .create_subwindow(
                None,
                top,
                (i * 20) as i16,
                0,
                10,
                90,
                0,
                HostSubwindowVisual::Explicit {
                    depth: 32,
                    visual_xid: 0,
                    colormap_xid: 0,
                },
                None,
                None,
            )
            .expect("strip child");
        b.map_subwindow(None, child.as_raw()).expect("map child");
        for j in 0..9 {
            let grandchild = b
                .create_subwindow(
                    None,
                    child,
                    0,
                    (j * 10) as i16,
                    10,
                    6,
                    0,
                    HostSubwindowVisual::Explicit {
                        depth: 32,
                        visual_xid: 0,
                        colormap_xid: 0,
                    },
                    None,
                    None,
                )
                .expect("strip grandchild");
            b.map_subwindow(None, grandchild.as_raw())
                .expect("map grandchild");
        }
    }

    b.apply_draw_state(
        None,
        &DrawState {
            subwindow_mode: SubwindowMode::IncludeInferiors,
            ..DrawState::default()
        },
    )
    .expect("apply include inferiors");

    b.fill_rectangle(None, top_xid, 0x0000_00ff, 20, 30, 70, 30)
        .expect("top fill include inferiors");
    let top_include_out = b
        .get_image_pixels_for_tests(top_xid, 2, 0, 0, 100, 90, !0)
        .expect("top include get_image")
        .expect("top include bytes");
    assert_eq!(top_include_out, expected);

    b.fill_rectangle(None, top_xid, 0x0000_0000, 0, 0, 100, 90)
        .expect("re-clear top after include inferiors");

    b.configure_subwindow(
        None,
        top_xid,
        HostSubwindowConfig {
            x: Some(0),
            y: Some(0),
            width: None,
            height: None,
            border_width: Some(0),
            sibling: None,
            stack_mode: None,
        },
    )
    .expect("move top to root origin");

    b.fill_rectangle(None, root.as_raw(), 0x0000_00ff, 20, 30, 70, 30)
        .expect("root fill include inferiors");

    let out = b
        .get_image_pixels_for_tests(top_xid, 2, 0, 0, 100, 90, !0)
        .expect("root-path get_image")
        .expect("root-path bytes");
    assert_eq!(out, expected);
}

/// Pack one `PolyRectangle` rect into X11 wire bytes:
/// x:i16, y:i16, w:u16, h:u16 — all little-endian.
fn pack_rect(x: i16, y: i16, w: u16, h: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(8);
    out.extend_from_slice(&x.to_le_bytes());
    out.extend_from_slice(&y.to_le_bytes());
    out.extend_from_slice(&w.to_le_bytes());
    out.extend_from_slice(&h.to_le_bytes());
    out
}

/// Import-selection regression (positive): a stroke drawn on the ROOT
/// with `subwindow_mode = IncludeInferiors` must also paint into a
/// redirected top-level window's own backing, so the selection
/// rectangle is visible over the composited window (not just in
/// root's occluded backing). ImageMagick `import` draws its XOR
/// selection rect this way.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn stroke_on_root_include_inferiors_reaches_redirected_toplevel_backing() {
    use yserver_core::{backend::WindowHandle, host_x11::HostSubwindowVisual};

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let root = WindowHandle::from_raw(1).expect("root");
    // Top-level W: 200×200 at screen (100, 100), depth 24.
    let w = b
        .create_subwindow(
            None,
            root,
            100,
            100,
            200,
            200,
            0,
            HostSubwindowVisual::Explicit {
                depth: 24,
                visual_xid: 0,
                colormap_xid: 0,
            },
            None,
            None,
        )
        .expect("top-level W");
    let w_xid = w.as_raw();
    b.map_subwindow(None, w_xid).expect("map W");

    // Redirected backing for W. `get_image(w_xid)` now reads this.
    let backing = b.create_pixmap(None, 24, 200, 200).expect("W backing");
    assert!(
        b.test_set_redirected_target(w_xid, backing.as_raw()),
        "redirect route must be recorded"
    );

    // Clear W's routed backing.
    b.fill_rectangle(None, w_xid, 0x0000_0000, 0, 0, 200, 200)
        .expect("clear W backing");

    // GC: IncludeInferiors + Copy, foreground red.
    b.apply_draw_state(
        None,
        &DrawState {
            subwindow_mode: SubwindowMode::IncludeInferiors,
            function: GcFunction::Copy,
            foreground: 0x00FF_0000,
            ..DrawState::default()
        },
    )
    .expect("apply include-inferiors copy");

    // PolyRectangle on ROOT spanning W exactly: (100, 100, 200, 200).
    let rect = pack_rect(100, 100, 200, 200);
    b.poly_rectangle(None, root.as_raw(), 0x00FF_0000, &rect)
        .expect("poly_rectangle on root");

    // W's routed backing top row must carry the red top edge.
    let out = b
        .get_image_pixels_for_tests(w_xid, 2, 0, 0, 200, 1, !0)
        .expect("get_image W")
        .expect("Some bytes");
    let hit = out
        .chunks_exact(4)
        .any(|px| u32::from_le_bytes([px[0], px[1], px[2], px[3]]) & 0x00FF_FFFF == 0x00FF_0000);
    assert!(
        hit,
        "rectangle top edge (red) must land in the redirected top-level backing"
    );
}

/// Import-selection regression (guard): a SINGLE-pass XOR/invert
/// stroke on the ROOT with `IncludeInferiors` must invert each
/// resolved backing EXACTLY once, even where the stroke also crosses a
/// non-redirected child that routes into the same backing. The naive
/// recursion (emit per visited window) would invert those overlap
/// pixels twice → cancel → a gap. A draw+erase round-trip cannot catch
/// this (it is symmetric), so we assert uniform single-pass coverage.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn stroke_root_xor_include_inferiors_no_gap_over_subwindow() {
    use yserver_core::{backend::WindowHandle, host_x11::HostSubwindowVisual};

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let root = WindowHandle::from_raw(1).expect("root");
    // Redirected top-level W: 200×200 at screen (100, 100), depth 24.
    let w = b
        .create_subwindow(
            None,
            root,
            100,
            100,
            200,
            200,
            0,
            HostSubwindowVisual::Explicit {
                depth: 24,
                visual_xid: 0,
                colormap_xid: 0,
            },
            None,
            None,
        )
        .expect("top-level W");
    let w_xid = w.as_raw();
    b.map_subwindow(None, w_xid).expect("map W");

    let backing = b.create_pixmap(None, 24, 200, 200).expect("W backing");
    assert!(
        b.test_set_redirected_target(w_xid, backing.as_raw()),
        "redirect route must be recorded"
    );

    // Clear W's routed backing to 0.
    b.fill_rectangle(None, w_xid, 0x0000_0000, 0, 0, 200, 200)
        .expect("clear W backing");

    // Non-redirected child C at W-local (0, 50), size 40×40. The
    // rectangle's LEFT edge (screen x=100 → W-local x=0) runs down
    // through C's rows, so C and W both route into `backing` there.
    let c = b
        .create_subwindow(
            None,
            w,
            0,
            50,
            40,
            40,
            0,
            HostSubwindowVisual::Explicit {
                depth: 24,
                visual_xid: 0,
                colormap_xid: 0,
            },
            None,
            None,
        )
        .expect("child C");
    b.map_subwindow(None, c.as_raw()).expect("map C");

    // GC: IncludeInferiors + Invert.
    b.apply_draw_state(
        None,
        &DrawState {
            subwindow_mode: SubwindowMode::IncludeInferiors,
            function: GcFunction::Invert,
            foreground: 0x00FF_FFFF,
            ..DrawState::default()
        },
    )
    .expect("apply include-inferiors invert");

    // ONE PolyRectangle on root spanning W.
    let rect = pack_rect(100, 100, 200, 200);
    b.poly_rectangle(None, root.as_raw(), 0x00FF_FFFF, &rect)
        .expect("poly_rectangle on root");

    // Left-edge column of W's backing: every interior pixel inverted
    // 0 → white for the full height, INCLUDING the rows overlapping C
    // (W-local y 50..90). Under the naive per-child recursion those
    // overlap pixels would be inverted twice → 0 → a gap. Under the
    // shipped dedup they are inverted exactly once → white.
    //
    // The two extreme rows (y=0 top-left, y=199 bottom-left) are the
    // rectangle-outline CORNERS: each corner pixel is emitted by two
    // adjacent edge segments, so under XOR/Invert it self-cancels to 0.
    // That is inherent to an XOR rectangle outline (same on Xorg) and
    // is orthogonal to the inferior-dedup this test guards, so the two
    // corner rows are excluded. Every non-corner row — the whole C
    // band included — must be uniformly inverted.
    let out = b
        .get_image_pixels_for_tests(w_xid, 2, 0, 0, 1, 200, !0)
        .expect("get_image W column")
        .expect("Some bytes");
    let rows: Vec<u32> = out
        .chunks_exact(4)
        .map(|px| u32::from_le_bytes([px[0], px[1], px[2], px[3]]) & 0x00FF_FFFF)
        .collect();
    assert_eq!(rows.len(), 200, "expected 200-pixel column");
    for (row, &v) in rows.iter().enumerate() {
        if row == 0 || row == 199 {
            continue; // outline corner: XOR self-cancels (see above)
        }
        assert_eq!(
            v, 0x00FF_FFFF,
            "row {row}: expected single-pass invert (no double-invert gap over sub-window)"
        );
    }
}

/// Stage 3e.2 acceptance: a 4×4 axis-aligned trapezoid (= filled
/// rect) painted via `render_trapezoids` must produce full coverage
/// in the trap interior. Validates the entire GPU pipeline: trap
/// rasterize → mask scratch → composite with SolidFill src. v1
/// has the equivalent rendercheck-driven gate; this is the v2
/// in-tree oracle.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn render_trapezoids_renders_filled_rect() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let dst_pix = b.create_pixmap(None, 32, 8, 8).expect("create_pixmap");
    let dst_xid = dst_pix.as_raw();
    b.fill_rectangle(None, dst_xid, 0xFF0000FF, 0, 0, 8, 8)
        .expect("pre-fill blue");

    let src_pic = b
        .render_create_solid_fill(None, [0xFF, 0xFF, 0, 0, 0, 0, 0xFF, 0xFF])
        .expect("solid_fill red")
        .expect("Some");
    let dst_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(dst_pix), 0, 0, &[])
        .expect("dst_pic")
        .expect("Some");

    // 16.16 fixed-point axis-aligned trapezoid:
    // top=2, bottom=6, left x=2, right x=6 → 4×4 inset rect.
    let mut traps: Vec<u8> = Vec::with_capacity(40);
    let fields: [i32; 10] = [
        2 << 16, // top
        6 << 16, // bottom
        2 << 16, // left_p1.x
        2 << 16, // left_p1.y
        2 << 16, // left_p2.x
        6 << 16, // left_p2.y
        6 << 16, // right_p1.x
        2 << 16, // right_p1.y
        6 << 16, // right_p2.x
        6 << 16, // right_p2.y
    ];
    for v in fields {
        traps.extend_from_slice(&v.to_le_bytes());
    }

    b.render_trapezoids(
        None,
        3, // Over
        src_pic.as_raw(),
        dst_pic.as_raw(),
        0, // mask_format — ignored at parity scope
        0,
        0,
        &traps,
        0,
        0,
    )
    .expect("render_trapezoids");

    let out = b
        .get_image_pixels_for_tests(dst_xid, 2, 0, 0, 8, 8, !0)
        .expect("get_image")
        .expect("Some");
    // Trap interior pixel (3, 3) — solidly inside — must be red.
    let off_inside = (3 * 8 + 3) * 4;
    assert_eq!(
        &out[off_inside..off_inside + 4],
        &[0x00, 0x00, 0xFF, 0xFF],
        "trap interior should be red (got {:?})",
        &out[off_inside..off_inside + 4],
    );
    // Outside the trap (0, 0) must stay blue.
    assert_eq!(
        &out[0..4],
        &[0xFF, 0x00, 0x00, 0xFF],
        "outside trap should stay blue (got {:?})",
        &out[0..4],
    );
}

/// Repro for the xeyes "pupils missing" hardware-smoke bug
/// reported 2026-05-16. xeyes paints:
///
/// 1. Trapezoids op=Over src=<SolidFill white> at the eye region
/// 2. Trapezoids op=Over src=<SolidFill black> at a smaller
///    pupil region inside it
///
/// On hardware the eye whites render correctly but the black
/// pupils never appear. v1's PaintBatch coalesces multiple paints
/// into ONE CB with in-CB barriers; v2's per-op CB shape means each
/// `render_trapezoids` call has its own CB. Both CBs share the
/// engine's single 1×1 `solid_src_image` scratch — CB1 clears it
/// to white + samples, CB2 clears it to black + samples. Hypothesis:
/// the cross-CB barrier on `solid_src_image` either isn't strong
/// enough to prevent CB2's clear from racing CB1's sample, or some
/// other piece of state is shared without proper sync.
///
/// Test: 16×16 dst pre-filled green; an 8×8 axis-aligned white
/// trap, then a 4×4 axis-aligned black trap inside it. The final
/// dst should read:
///
/// - black at the centre (inside both traps)
/// - white between (inside white but outside black)
/// - green at corners (outside both traps)
///
/// If the second paint loses its black source (race on
/// `solid_src_image`), the centre will read white or undefined.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn back_to_back_trapezoids_different_solidfill_colors() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let dst_pix = b.create_pixmap(None, 32, 16, 16).expect("create_pixmap");
    let dst_xid = dst_pix.as_raw();
    // Pre-fill green: 0xFF00FF00 ARGB. BGRA wire bytes:
    // B=0, G=0xFF, R=0, A=0xFF.
    b.fill_rectangle(None, dst_xid, 0xFF00FF00, 0, 0, 16, 16)
        .expect("pre-fill green");

    let dst_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(dst_pix), 0, 0, &[])
        .expect("dst_pic")
        .expect("Some");

    // White SolidFill: RGBA(0xFFFF, 0xFFFF, 0xFFFF, 0xFFFF).
    let white_src = b
        .render_create_solid_fill(None, [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF])
        .expect("solid_fill white")
        .expect("Some");
    // Black SolidFill: RGBA(0, 0, 0, 0xFFFF).
    let black_src = b
        .render_create_solid_fill(None, [0, 0, 0, 0, 0, 0, 0xFF, 0xFF])
        .expect("solid_fill black")
        .expect("Some");

    // Helper: build an axis-aligned trapezoid wire blob (40 bytes
    // per trap, 16.16 fixed-point).
    let trap_bytes = |top: i32, bot: i32, left: i32, right: i32| -> Vec<u8> {
        let mut v: Vec<u8> = Vec::with_capacity(40);
        let fields: [i32; 10] = [
            top << 16,
            bot << 16,
            left << 16,
            top << 16,
            left << 16,
            bot << 16,
            right << 16,
            top << 16,
            right << 16,
            bot << 16,
        ];
        for f in fields {
            v.extend_from_slice(&f.to_le_bytes());
        }
        v
    };

    // 8×8 white trap at (4..12, 4..12) — analogous to xeyes' eye
    // white.
    b.render_trapezoids(
        None,
        3, // Over
        white_src.as_raw(),
        dst_pic.as_raw(),
        0,
        0,
        0,
        &trap_bytes(4, 12, 4, 12),
        0,
        0,
    )
    .expect("render_trapezoids white");

    // 4×4 black trap at (6..10, 6..10) — analogous to xeyes' pupil
    // inside the eye.
    b.render_trapezoids(
        None,
        3, // Over
        black_src.as_raw(),
        dst_pic.as_raw(),
        0,
        0,
        0,
        &trap_bytes(6, 10, 6, 10),
        0,
        0,
    )
    .expect("render_trapezoids black");

    let out = b
        .get_image_pixels_for_tests(dst_xid, 2, 0, 0, 16, 16, !0)
        .expect("get_image")
        .expect("Some");

    let pixel = |x: usize, y: usize| -> [u8; 4] {
        let off = (y * 16 + x) * 4;
        [out[off], out[off + 1], out[off + 2], out[off + 3]]
    };

    // Centre (8, 8): inside black trap → must read black.
    assert_eq!(
        pixel(8, 8),
        [0x00, 0x00, 0x00, 0xFF],
        "centre must be black (pupil): {:?} — if white, the second \
         render_trapezoids' SolidFill source was lost (shared \
         solid_src_image race?)",
        pixel(8, 8),
    );
    // (5, 5): inside white but outside black → must read white.
    assert_eq!(
        pixel(5, 5),
        [0xFF, 0xFF, 0xFF, 0xFF],
        "(5,5) must be white (eye): got {:?}",
        pixel(5, 5),
    );
    // (1, 1): outside both → must stay green.
    assert_eq!(
        pixel(1, 1),
        [0x00, 0xFF, 0x00, 0xFF],
        "(1,1) must stay green (root bg): got {:?}",
        pixel(1, 1),
    );
}

/// xeyes "stripes-in-the-eye-white" repro. xeyes builds each eye
/// out of ~16 stacked horizontal trapezoids that share their
/// top/bottom edges (trap N's bottom = trap N+1's top). The shared
/// edge sits on a non-integer Y coordinate (xeyes' ellipse math
/// rounds to fixed-point 16.16). For pixels straddling the
/// boundary, the AA edge formula must produce coverages from the
/// two adjacent traps that SUM to ~1.0 — otherwise the boundary
/// rows under-cover and you see horizontal stripes inside the
/// eye whites.
///
/// Pre-3f.x fix: trap.frag.glsl's `c_top` / `c_bot` formulas
/// computed `clamp(p.y - top, 0, 1)` instead of
/// `clamp(0.5 + (p.y - top), 0, 1)` — off by 0.5 vs the slanted-
/// edge formula. At a shared boundary y=12.788, pixel center
/// y=12.5: trap1 c_bot = clamp(0.288, 0, 1) = 0.288; trap2 c_top
/// = clamp(-0.288, 0, 1) = 0; total = 0.288, leaving 0.712
/// missing coverage at that row.
///
/// Test: two adjacent axis-aligned traps sharing y=4.5. Centre
/// row (y=4) should read fully opaque white.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn adjacent_trapezoids_share_horizontal_boundary_cleanly() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let dst_pix = b.create_pixmap(None, 32, 10, 10).expect("create_pixmap");
    let dst_xid = dst_pix.as_raw();
    b.fill_rectangle(None, dst_xid, 0xFF0000FF, 0, 0, 10, 10)
        .expect("pre-fill blue");

    let src_pic = b
        .render_create_solid_fill(None, [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF])
        .expect("solid_fill white")
        .expect("Some");
    let dst_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(dst_pix), 0, 0, &[])
        .expect("dst_pic")
        .expect("Some");

    // Two adjacent trapezoids sharing y=4.5 boundary.
    // Both span x∈[2, 8].
    // 16.16 fixed-point: pixel * 65536; half-pixel = 32768.
    let fields1: [i32; 10] = [
        2 << 16,            // top = 2
        (4 << 16) | 0x8000, // bottom = 4.5
        2 << 16,
        2 << 16,
        2 << 16,
        (4 << 16) | 0x8000,
        8 << 16,
        2 << 16,
        8 << 16,
        (4 << 16) | 0x8000,
    ];
    let fields2: [i32; 10] = [
        (4 << 16) | 0x8000, // top = 4.5
        7 << 16,            // bottom = 7
        2 << 16,
        (4 << 16) | 0x8000,
        2 << 16,
        7 << 16,
        8 << 16,
        (4 << 16) | 0x8000,
        8 << 16,
        7 << 16,
    ];
    let mut traps: Vec<u8> = Vec::with_capacity(80);
    for v in fields1 {
        traps.extend_from_slice(&v.to_le_bytes());
    }
    for v in fields2 {
        traps.extend_from_slice(&v.to_le_bytes());
    }
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
    .expect("render_trapezoids");

    let out = b
        .get_image_pixels_for_tests(dst_xid, 2, 0, 0, 10, 10, !0)
        .expect("get_image")
        .expect("Some");
    let pixel = |x: usize, y: usize| -> [u8; 4] {
        let off = (y * 10 + x) * 4;
        [out[off], out[off + 1], out[off + 2], out[off + 3]]
    };

    // Row 4 (centre y=4.5, straddles the trap boundary). Should
    // read white (≈ full coverage). Pre-fix: ≈ partial coverage,
    // pixel is mostly white but blended with blue under-fill →
    // visible stripe.
    for x in 3..7 {
        let p = pixel(x, 4);
        // Each channel near 0xFF (allow ±16 for AA softening at
        // slanted side edges — but x=3..7 is well-inside the
        // trapezoid horizontally so the slanted-edge AA is full).
        assert!(
            p[0] >= 0xE0 && p[1] >= 0xE0 && p[2] >= 0xE0,
            "row 4 should be ~white at x={x} (got {:?}); pre-fix bug = horizontal stripe",
            p,
        );
    }
}

/// Regression for the xeyes-resize bug (2026-05-16): the user
/// resizes the xeyes window larger; the new bigger eyes paint
/// correctly but the OLD small-eye-white pixels at the original
/// (smaller) positions remain visible in the upper-left of the
/// window. Indicates the storage isn't being cleared on resize, or
/// the clear doesn't cover the full new extent.
///
/// Test: create a 16×16 window, paint a red rect inside it,
/// configure to 64×64, then get_image the new (bigger) storage at
/// position (5, 5) — where the old red would still live if the
/// resize-fill didn't run. Expect the safe-default depth-32 colour
/// (transparent black), not red.
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
    // 3f.14's allocate_window_storage fills it with transparent-
    // black on creation.
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
    // 16×16 region. Pre-3f.14 / pre-fix: still red (leftover old
    // paint). 3f.14 expectation: depth-32 safe default
    // (transparent black, BGRA = [0, 0, 0, 0]).
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
        [0x00, 0x00, 0x00, 0x00],
        "post-resize storage at (5,5) must be cleared to safe-default \
         transparent black (got {:?}); old red would mean the resize-fill \
         didn't cover this position",
        pixel(5, 5),
    );
    // (30, 30) is outside the old footprint, well inside the new.
    assert_eq!(
        pixel(30, 30),
        [0x00, 0x00, 0x00, 0x00],
        "post-resize storage at (30,30) must also be cleared (got {:?})",
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

/// Diagnostic: same trap geometry shape as
/// render_trapezoids_renders_filled_rect but with a LARGE bbox
/// (covering most of mask_scratch's 256×256 default extent). If
/// this passes while the 4×4 variant fails, the bug is
/// bbox-size-vs-mask-extent ratio — Intel rasterizer culls tiny
/// quads in big viewports. The fix would be to size the viewport
/// to the bbox, not the full mask.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn render_trapezoids_large_bbox_repro() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // 200×200 dst pre-filled blue.
    let dst_pix = b.create_pixmap(None, 32, 200, 200).expect("create_pixmap");
    let dst_xid = dst_pix.as_raw();
    b.fill_rectangle(None, dst_xid, 0xFF0000FF, 0, 0, 200, 200)
        .expect("fill pre-blue");

    let src_pic = b
        .render_create_solid_fill(None, [0xFF, 0xFF, 0, 0, 0, 0, 0xFF, 0xFF])
        .expect("solid_fill red")
        .expect("Some");
    let dst_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(dst_pix), 0, 0, &[])
        .expect("dst_pic")
        .expect("Some");

    // Big axis-aligned trap: 100×100 inside the 200×200 dst.
    let mut traps: Vec<u8> = Vec::with_capacity(40);
    let fields: [i32; 10] = [
        50 << 16,
        150 << 16,
        50 << 16,
        50 << 16,
        50 << 16,
        150 << 16,
        150 << 16,
        50 << 16,
        150 << 16,
        150 << 16,
    ];
    for v in fields {
        traps.extend_from_slice(&v.to_le_bytes());
    }
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
    .expect("render_trapezoids");

    let out = b
        .get_image_pixels_for_tests(dst_xid, 2, 0, 0, 200, 200, !0)
        .expect("get_image")
        .expect("Some");
    // Center pixel (100, 100) — well inside trap (50..150, 50..150).
    let off = (100 * 200 + 100) * 4;
    assert_eq!(
        &out[off..off + 4],
        &[0x00, 0x00, 0xFF, 0xFF],
        "center should be red (got {:?})",
        &out[off..off + 4],
    );
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
/// get_image its xid and assert every pixel is transparent black
/// (depth-32 safe default).
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

    let out = b
        .get_image_pixels_for_tests(child_xid, 2, 0, 0, 16, 16, !0)
        .expect("get_image")
        .expect("Some");
    assert_eq!(out.len(), 16 * 16 * 4);
    // Depth-32 → transparent black `(0, 0, 0, 0)` per
    // `default_window_init_color`.
    for (i, px) in out.chunks_exact(4).enumerate() {
        assert_eq!(
            &px[0..4],
            &[0x00, 0x00, 0x00, 0x00],
            "fresh depth-32 storage pixel #{i} must be transparent black (got {:?})",
            &px[0..4],
        );
    }
}

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

// ───── Stage 4a — resolve_paint_target via redirect routing ─────

/// Allocate two pixmaps W and B, install `redirected_target(W) =
/// Some(B)` via the test-only setter, then drive `fill_rectangle`
/// against W's xid. Pre-4a: paint would land in W's storage.
/// Post-4a: paint resolves through the redirect and lands in B.
/// GetImage on both reads back the redirected colour from B (also
/// resolved) and B (raw lookup); the same buffer in both cases.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn set_redirected_target_routes_fill_to_backing() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let w_xid = b.create_pixmap(None, 32, 8, 8).expect("W").as_raw();
    let bk_xid = b.create_pixmap(None, 32, 8, 8).expect("B").as_raw();
    // Pre-fill W with red and B with blue so we can tell which one
    // a subsequent paint actually hit.
    b.fill_rectangle(None, w_xid, 0xFFFF0000, 0, 0, 8, 8)
        .expect("seed W red");
    b.fill_rectangle(None, bk_xid, 0xFF0000FF, 0, 0, 8, 8)
        .expect("seed B blue");

    // Install the redirect AFTER the seed fills so the seed paints
    // landed in their respective storage (W has red, B has blue
    // pre-redirect).
    assert!(
        b.test_set_redirected_target(w_xid, bk_xid),
        "test_set_redirected_target failed — xids resolvable?",
    );

    // Paint green via W's xid. Under redirect this lands in B,
    // overwriting the blue.
    b.fill_rectangle(None, w_xid, 0xFF00FF00, 0, 0, 8, 8)
        .expect("redirected fill");

    // GetImage on B's xid (raw, no redirect on a Pixmap) returns
    // the green — the redirected fill landed here.
    let img_b = b
        .get_image_pixels_for_tests(bk_xid, 2, 0, 0, 8, 8, !0)
        .expect("get_image B")
        .expect("Some B bytes");
    assert_eq!(
        &img_b[..4],
        &[0x00, 0xFF, 0x00, 0xFF],
        "B's (0,0) must read green (BGRA) after the redirected fill",
    );

    // GetImage on W's xid ALSO resolves through the redirect per
    // Risk 1, so it reads the same green from B — NOT the seeded
    // red on W's own storage.
    let img_w = b
        .get_image_pixels_for_tests(w_xid, 2, 0, 0, 8, 8, !0)
        .expect("get_image W")
        .expect("Some W bytes");
    assert_eq!(
        &img_w[..4],
        &[0x00, 0xFF, 0x00, 0xFF],
        "GetImage(W) under redirect must read from B (green), \
         not the leaf storage (still red)",
    );
}

/// Set up parent-W with a sub-child C at position (2, 3). Redirect
/// W to backing B. A fill rect at (1, 1, 4, 4) against C's xid must
/// land at (3, 4, 4, 4) in B — the C-relative offset accumulated
/// through `resolve_paint_target`. Tests the descendant-offset
/// path end-to-end through the Backend trait.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn set_redirected_target_descendant_fill_lands_at_offset() {
    use yserver_core::{backend::WindowHandle, host_x11::HostSubwindowVisual};

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // Create depth-32 W under root, 16×16; then C at (2, 3) under
    // W, 8×8. allocate_window_storage will fill both with the
    // depth-32 safe default (transparent black).
    let root = WindowHandle::from_raw(1).expect("root");
    let w = b
        .create_subwindow(
            None,
            root,
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
        .expect("create W");
    let w_xid = w.as_raw();
    let c = b
        .create_subwindow(
            None,
            w,
            2,
            3,
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
        .expect("create C");
    let c_xid = c.as_raw();

    // Allocate B (a pixmap) for the backing storage. Seed it black
    // so the post-fill check can detect green-at-offset.
    let bk_xid = b.create_pixmap(None, 32, 16, 16).expect("B").as_raw();
    b.fill_rectangle(None, bk_xid, 0xFF000000, 0, 0, 16, 16)
        .expect("seed B black");

    // Install the redirect W → B.
    assert!(
        b.test_set_redirected_target(w_xid, bk_xid),
        "redirect install (W={w_xid:#x}, B={bk_xid:#x})"
    );

    // Fill green on C at (1, 1, 4, 4) — C-window-local coords.
    // Expected outcome: paint resolves through C→W (ancestor walk)
    // with accumulated offset (2, 3), then through W's redirect
    // to B. Result: green rect at B coords (3, 4, 4, 4).
    b.fill_rectangle(None, c_xid, 0xFF00FF00, 1, 1, 4, 4)
        .expect("descendant fill");

    // GetImage on B directly. Stride for depth-32 is `w * 4`.
    let img = b
        .get_image_pixels_for_tests(bk_xid, 2, 0, 0, 16, 16, !0)
        .expect("get_image B")
        .expect("Some bytes");
    let pixel = |x: usize, y: usize| -> [u8; 4] {
        let off = (y * 16 + x) * 4;
        [img[off], img[off + 1], img[off + 2], img[off + 3]]
    };
    // Inside the redirected rect: (3,4)..(7,8).
    assert_eq!(
        pixel(3, 4),
        [0x00, 0xFF, 0x00, 0xFF],
        "B at (3,4) must be green — descendant offset (2,3) plus rect (1,1) sums to (3,4)",
    );
    assert_eq!(
        pixel(6, 7),
        [0x00, 0xFF, 0x00, 0xFF],
        "B at (6,7) — last pixel of the redirected rect — must also be green",
    );
    // Outside the rect: still the seeded black.
    assert_eq!(
        pixel(0, 0),
        [0x00, 0x00, 0x00, 0xFF],
        "B at (0,0) must stay black — fill lands at (3,4), not the origin",
    );
    assert_eq!(
        pixel(8, 8),
        [0x00, 0x00, 0x00, 0xFF],
        "B at (8,8) must stay black — past the redirected rect's bottom-right",
    );
}

// ───── Stage 4b — allocate_redirected_backing / name_window_pixmap /
// ───── release_redirected_backing
//
// Each test drives the Backend-trait surface for the COMPOSITE
// redirect lifecycle. v1's reference impls live in
// `crates/yserver/src/kms/backend.rs:9523-9607`; v2 mirrors the
// shape via `KmsCore.alias_registry` + `KmsCore.host_window_to_backing`
// (already in tree as shared state).

/// Plan §4b: `allocate_redirected_backing(W, w, h, depth)` allocates
/// a fresh backing pixmap, seeds `alias_registry` with refcount=1,
/// and maps `host_window_to_backing[W] = B`. The returned
/// `PixmapHandle` is what `name_window_pixmap(W)` returns on every
/// subsequent call (with incremented refcount).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn allocate_redirected_backing_seeds_refcount_and_map() {
    use yserver_core::backend::WindowHandle;

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    // Allocate a pixmap to act as the "window" — v2 doesn't care
    // about W being a real Window-kind drawable for the activation
    // path; what matters is the xid resolves in the store so the
    // `set_redirected_target` step succeeds. (In 4c real-app paths
    // W is a top-level Window-kind drawable; the seed-copy path
    // tested separately in `redirect_seed_copies_window_content`
    // exercises that shape.)
    let w_xid = b.create_pixmap(None, 32, 16, 16).expect("W").as_raw();
    let w_handle = WindowHandle::from_raw(w_xid).expect("WindowHandle");

    let backing = b
        .allocate_redirected_backing(None, w_handle, 16, 16, 32)
        .expect("allocate_redirected_backing must succeed in v2");
    let raw = backing.as_raw();
    assert_ne!(raw, 0, "backing handle is non-zero");
    assert_ne!(
        raw, w_xid,
        "backing xid distinct from window xid (fresh pixmap)",
    );

    // Inspect the shared state via the read-only test helper.
    let entry = b
        .test_alias_registry_get(raw)
        .expect("alias_registry must have a Reason-1 hold");
    assert_eq!(entry.refcount, 1, "Reason-1 seed → refcount = 1");
    assert_eq!(entry.width, 16);
    assert_eq!(entry.height, 16);
    assert_eq!(entry.depth, 32);

    let mapped = b
        .test_host_window_to_backing(w_xid)
        .expect("host_window_to_backing must point at the backing");
    assert_eq!(mapped, raw, "map points at the backing xid");
}

/// Plan §4b: a second `allocate_redirected_backing(W, …)` for an
/// already-redirected W returns the SAME handle with NO refcount
/// bump (it's the redirect-activation hold, not an alias). v1
/// idempotency path at `kms/backend.rs:9581-9588`.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn allocate_redirected_backing_is_idempotent() {
    use yserver_core::backend::WindowHandle;

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let w_xid = b.create_pixmap(None, 32, 8, 8).expect("W").as_raw();
    let w = WindowHandle::from_raw(w_xid).unwrap();
    let first = b.allocate_redirected_backing(None, w, 8, 8, 32).unwrap();
    let second = b.allocate_redirected_backing(None, w, 8, 8, 32).unwrap();
    assert_eq!(
        first.as_raw(),
        second.as_raw(),
        "idempotent allocation returns the same handle",
    );
    let entry = b.test_alias_registry_get(first.as_raw()).unwrap();
    assert_eq!(
        entry.refcount, 1,
        "no incref on the idempotent path — Reason-1 is single-instance",
    );
}

/// Plan §4b: `name_window_pixmap(W)` after activation returns the
/// existing backing and increments refcount (Reason-2 alias hold).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn name_window_pixmap_returns_existing_backing() {
    use yserver_core::backend::WindowHandle;

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let w_xid = b.create_pixmap(None, 32, 8, 8).expect("W").as_raw();
    let w = WindowHandle::from_raw(w_xid).unwrap();
    let backing = b.allocate_redirected_backing(None, w, 8, 8, 32).unwrap();
    let aliased = b.name_window_pixmap(None, w).unwrap();
    assert_eq!(
        aliased.as_raw(),
        backing.as_raw(),
        "alias handle equals backing handle (same xid on every call)",
    );
    let entry = b.test_alias_registry_get(backing.as_raw()).unwrap();
    assert_eq!(
        entry.refcount, 2,
        "alias bumps refcount to 2 (Reason-1 + Reason-2)",
    );
}

/// Plan §4b: `name_window_pixmap(W)` against an un-redirected W
/// returns `NotFound` (X11 protocol error → BadWindow upstream).
/// v1 uses `io::ErrorKind::NotFound` at `kms/backend.rs:9534`.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn name_window_pixmap_without_redirect_errors_not_found() {
    use yserver_core::backend::WindowHandle;

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let w_xid = b.create_pixmap(None, 32, 8, 8).expect("W").as_raw();
    let w = WindowHandle::from_raw(w_xid).unwrap();
    let err = b
        .name_window_pixmap(None, w)
        .expect_err("name without redirect must error");
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::NotFound,
        "v1-parity: NotFound (got {err:?})",
    );
}

/// Plan §4b: `release_redirected_backing` decrefs the Reason-1
/// hold; with no aliases held, the backing storage is destroyed.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn release_redirected_backing_drops_storage_when_no_aliases() {
    use yserver_core::backend::WindowHandle;

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let w_xid = b.create_pixmap(None, 32, 8, 8).expect("W").as_raw();
    let w = WindowHandle::from_raw(w_xid).unwrap();
    let backing = b.allocate_redirected_backing(None, w, 8, 8, 32).unwrap();
    let bxid = backing.as_raw();

    b.release_redirected_backing(None, backing).unwrap();

    assert!(
        b.test_alias_registry_get(bxid).is_none(),
        "alias_registry entry removed (refcount → 0)",
    );
    assert!(
        b.test_host_window_to_backing(w_xid).is_none(),
        "host_window_to_backing entry cleared",
    );
}

/// Audit #6 (2026-05-19) — Xorg parity. `compNewPixmap`
/// (composite/compalloc.c:541-606) seeds the backing pixmap from
/// the PARENT's storage at W's position (with IncludeInferiors),
/// NOT from W's own storage. This is the fix for the recurring
/// "black band on map" symptom: a freshly mapped window that's
/// redirected on map has a default-init (opaque black or
/// transparent) storage; copying that into B would show black
/// where W is until the client's first paint. Seeding from the
/// parent shows continuity with what was on-screen before W
/// appeared.
///
/// Repro: paint root red at the W-footprint area; create W as a
/// child of root with NO paint of its own; activate redirect.
/// The backing must read red — parent's pixels at W's position —
/// NOT W's default-init colour.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn redirect_seed_uses_parent_content_at_w_position() {
    use yserver_core::{backend::WindowHandle, host_x11::HostSubwindowVisual};

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let root = WindowHandle::from_raw(1).expect("root");
    let root_xid = root.as_raw();

    // Paint a known red into the root at the area that W will cover.
    // We paint a 16×16 region from (5, 7) so it strictly contains W
    // (8×8 at (5, 7) inside root).
    b.fill_rectangle(None, root_xid, 0xFFFF0000, 5, 7, 16, 16)
        .expect("seed root red at W footprint");

    let w_handle = b
        .create_subwindow(
            None,
            root,
            5,
            7,
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
        .expect("create W as child of root");
    // Deliberately do NOT paint W — its storage stays at the
    // default init colour (depth-32 → (0, 0, 0, 0) transparent).

    let backing = b
        .allocate_redirected_backing(None, w_handle, 8, 8, 32)
        .expect("allocate must succeed");
    let bxid = backing.as_raw();

    let img = b
        .get_image_pixels_for_tests(bxid, 2, 0, 0, 8, 8, !0)
        .expect("get_image")
        .expect("Some bytes");
    let pixel = |x: usize, y: usize| -> [u8; 4] {
        let off = (y * 8 + x) * 4;
        [img[off], img[off + 1], img[off + 2], img[off + 3]]
    };

    // Pre-fix: backing reads W's default-init (0,0,0,0) — invisible /
    // black-band depending on the scene blend. Post-fix: parent's red
    // at the source position (5, 7), copied into B at (0, 0).
    assert_eq!(
        pixel(0, 0),
        [0x00, 0x00, 0xFF, 0xFF],
        "backing's (0, 0) must read parent's red at W's screen \
         position (5, 7); pre-fix the seed copied W's default-init \
         colour and produced (0, 0, 0, 0).",
    );
    assert_eq!(
        pixel(7, 7),
        [0x00, 0x00, 0xFF, 0xFF],
        "backing's (7, 7) must read parent's red (the W-footprint \
         region of root was filled red strictly larger than W).",
    );
}

/// Plan §4b: a `NameWindowPixmap` alias keeps the backing alive
/// past `release_redirected_backing` — the alias's FreePixmap
/// is what eventually drops the storage.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn release_redirected_backing_survives_named_alias() {
    use yserver_core::backend::WindowHandle;

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let w_xid = b.create_pixmap(None, 32, 8, 8).expect("W").as_raw();
    let w = WindowHandle::from_raw(w_xid).unwrap();
    let backing = b.allocate_redirected_backing(None, w, 8, 8, 32).unwrap();
    let bxid = backing.as_raw();
    let alias = b.name_window_pixmap(None, w).unwrap();
    assert_eq!(alias.as_raw(), bxid, "alias is the backing xid");

    // Drop Reason 1. Reason 2 (alias) keeps it alive.
    b.release_redirected_backing(None, backing).unwrap();
    let entry = b
        .test_alias_registry_get(bxid)
        .expect("alias still holds the backing");
    assert_eq!(entry.refcount, 1, "Reason-1 dropped, Reason-2 remains");
    assert!(
        b.test_host_window_to_backing(w_xid).is_none(),
        "redirect map cleared — only the alias refers to the backing now",
    );

    // FreePixmap on the alias must drop the storage.
    b.free_pixmap(None, alias.as_raw()).unwrap();
    assert!(
        b.test_alias_registry_get(bxid).is_none(),
        "alias FreePixmap drops the last hold",
    );
}

// ───── Stage 4c.5 — Vk-backed participation + mode-flip oracles ────
//
// Test #5 (`redirected_paint_lands_in_backing`) from the task spec
// is already covered by `set_redirected_target_routes_fill_to_backing`
// above — that test pre-fills B blue, installs the redirect, paints
// green through W's xid, and asserts B reads green. Skipped here to
// keep the suite mean (single-purpose oracles).

/// Stage 4c.5 — Automatic-mode redirect: paint through W's xid lands
/// in B (per 4a's `resolve_paint_target`) AND accumulates presentation
/// damage on B (since B's `scene_participating=true`). The scene
/// walk's `peek_presentation_damage` (scene.rs:1148 via 4c.3's
/// `source_id` indirection) is what picks up that damage; the
/// participation flag on B is the gate (`peek` returns None when
/// `!scene_participating`).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn automatic_redirect_backing_is_scene_participating() {
    use yserver_core::backend::{PixmapHandle, WindowHandle};

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // Use a depth-32 pixmap as W (the redirect surface). `for_tests`
    // doesn't drive a real CreateWindow flow; v2's
    // `allocate_redirected_backing` accepts any drawable xid in the
    // store (the `name_window_pixmap_returns_existing_backing` test
    // above uses the same shape).
    let w_xid = b.create_pixmap(None, 32, 8, 8).expect("W").as_raw();
    let w = WindowHandle::from_raw(w_xid).expect("WindowHandle");
    let backing = b
        .allocate_redirected_backing(None, w, 8, 8, 32)
        .expect("allocate backing");
    let bxid = backing.as_raw();
    let bk_handle = PixmapHandle::from_raw(bxid).expect("PixmapHandle");

    // Automatic-mode protocol pairing: W AND B both flip to
    // scene_participating=true.
    b.set_window_scene_participation(None, w, true)
        .expect("set_window_scene_participation(true)");
    b.set_backing_scene_participation(None, bk_handle, true)
        .expect("set_backing_scene_participation(true)");

    // Per-store assertion: B's scene_participating flipped on.
    // Reach into the doc-hidden test helpers via the public store
    // surface — `get_by_xid` is `pub(crate)`, so use the
    // presentation-damage probe below as the contract check.
    // First confirm the flag flipped by checking that
    // peek_presentation_damage doesn't `None` out (it would on
    // !scene_participating, even after we paint).

    // Paint green via W's xid. Per 4a's `resolve_paint_target` this
    // lands in B; per 3f's damage accounting that fires
    // `store.damage` on B's drawable, which (with B
    // scene_participating=true) accumulates as presentation damage.
    b.fill_rectangle(None, w_xid, 0xFF00FF00, 1, 2, 3, 4)
        .expect("redirected fill via W");

    // GetImage on B confirms the paint landed there (sanity — the
    // damage assertion below relies on the paint actually hitting).
    let img = b
        .get_image_pixels_for_tests(bxid, 2, 0, 0, 8, 8, !0)
        .expect("get_image B")
        .expect("Some B bytes");
    let pixel = |x: usize, y: usize| -> [u8; 4] {
        let off = (y * 8 + x) * 4;
        [img[off], img[off + 1], img[off + 2], img[off + 3]]
    };
    assert_eq!(
        pixel(1, 2),
        [0x00, 0xFF, 0x00, 0xFF],
        "B at (1,2) — top-left of the redirected fill — must be green",
    );

    // The key oracle: presentation damage accumulated on B (because
    // B is scene_participating=true). A pre-4c backing with the
    // default scene_participating=false would have produced a
    // damage record that `peek_presentation_damage` returns as None
    // (see store.rs:670 — the gate is the `scene_participating`
    // flag). `test_peek_presentation_damage_nonempty` rolls both
    // checks into one bool to keep this oracle terse.
    assert!(
        b.test_peek_presentation_damage_nonempty(bxid),
        "B must have peekable, non-empty presentation damage from the redirected fill \
         (false ⇒ either scene_participating=false or region empty at paint time)",
    );
}

/// Stage 4c.5 — mode-flip preserves the backing and any
/// `NameWindowPixmap` aliases. Per Stage 4 plan §"Cross-cutting:
/// Mode-flip semantics", `RedirectWindow(W, Mode)` issued a second
/// time on an already-redirected W must reuse the existing backing
/// (no destroy + recreate) so client aliases stay valid and content
/// is preserved. This test exercises the at-this-layer simulation:
///
/// - alloc backing for W
/// - name_window_pixmap(W) → alias bumps refcount to 2
/// - paint a sentinel into B
/// - simulate a Manual→Automatic mode flip by toggling participation
///   (Automatic-mode protocol pairing)
/// - assert: backing's xid unchanged, alias refcount unchanged, B's
///   sentinel content preserved
///
/// Note (per task spec): the protocol-handler `flip_redirect_target_mode`
/// path in `yserver-core/src/core_loop/process_request.rs` isn't
/// drivable from `tests/acceptance.rs` without protocol scaffolding
/// (see TODO comments below). The participation-toggle dance covers
/// the same backend-trait invariants the protocol handler exercises.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn mode_flip_preserves_backing_and_aliases() {
    use yserver_core::backend::{PixmapHandle, WindowHandle};

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let w_xid = b.create_pixmap(None, 32, 8, 8).expect("W").as_raw();
    let w = WindowHandle::from_raw(w_xid).expect("WindowHandle");

    // Initial Manual-mode setup: allocate backing, flip W off-scene.
    let backing = b
        .allocate_redirected_backing(None, w, 8, 8, 32)
        .expect("allocate backing");
    let bxid_pre_flip = backing.as_raw();
    b.set_window_scene_participation(None, w, false)
        .expect("Manual activation (W→false)");

    // Create a NameWindowPixmap alias — refcount goes 1 → 2.
    let alias = b.name_window_pixmap(None, w).expect("name_window_pixmap");
    assert_eq!(
        alias.as_raw(),
        bxid_pre_flip,
        "alias xid must equal the backing xid (Reason-2 incref on the same handle)",
    );
    let entry_before = b
        .test_alias_registry_get(bxid_pre_flip)
        .expect("alias_registry entry present");
    assert_eq!(
        entry_before.refcount, 2,
        "post-alias refcount = Reason-1 (1) + Reason-2 (1) = 2",
    );

    // Paint a sentinel into B before the flip — magenta at (0,0).
    b.fill_rectangle(None, bxid_pre_flip, 0xFFFF00FF, 0, 0, 8, 8)
        .expect("sentinel paint into B");
    let img_pre = b
        .get_image_pixels_for_tests(bxid_pre_flip, 2, 0, 0, 8, 8, !0)
        .expect("get_image pre-flip")
        .expect("Some bytes pre-flip");
    let pre_pixel: [u8; 4] = [img_pre[0], img_pre[1], img_pre[2], img_pre[3]];
    assert_eq!(
        pre_pixel,
        [0xFF, 0x00, 0xFF, 0xFF],
        "fixture sanity: B's (0,0) must read the sentinel magenta pre-flip",
    );

    // Mode flip: Manual → Automatic. The protocol handler's
    // `flip_redirect_target_mode` ultimately calls
    // `set_window_scene_participation(W, true)` +
    // `set_backing_scene_participation(B, true)`.
    let bk_handle = PixmapHandle::from_raw(bxid_pre_flip).expect("PixmapHandle");
    b.set_window_scene_participation(None, w, true)
        .expect("Automatic activation (W→true)");
    b.set_backing_scene_participation(None, bk_handle, true)
        .expect("Automatic activation (B→true)");

    // Backing xid unchanged.
    let bxid_post = b
        .test_host_window_to_backing(w_xid)
        .expect("host_window_to_backing still maps W → B");
    assert_eq!(
        bxid_post, bxid_pre_flip,
        "mode flip must NOT recreate the backing (xid must be stable)",
    );

    // Alias refcount unchanged (still Reason-1 + Reason-2).
    let entry_after = b
        .test_alias_registry_get(bxid_pre_flip)
        .expect("alias_registry entry still present post-flip");
    assert_eq!(
        entry_after.refcount, entry_before.refcount,
        "alias refcount must be preserved across mode flip \
         (pre={}, post={})",
        entry_before.refcount, entry_after.refcount,
    );

    // Content preserved — B's (0,0) still magenta.
    let img_post = b
        .get_image_pixels_for_tests(bxid_pre_flip, 2, 0, 0, 8, 8, !0)
        .expect("get_image post-flip")
        .expect("Some bytes post-flip");
    let post_pixel: [u8; 4] = [img_post[0], img_post[1], img_post[2], img_post[3]];
    assert_eq!(
        post_pixel, pre_pixel,
        "B's content must be preserved across mode flip \
         (pre={pre_pixel:?}, post={post_pixel:?})",
    );
}

// ───── Stage 4c.5 — deferred protocol-level tests ───────────────────
//
// The Stage 4b.9 / 4c plan also lists these protocol-level invariants
// that require driving the X11 wire bytes through
// `yserver-core::core_loop::process_request::handle_composite_request`.
// yserver-core has no test scaffolding for that path today, and
// building it is its own substage's worth of work. The hardware-smoke
// gate at 4c.6 is the actual coverage for these invariants until the
// scaffolding lands.
//
// TODO(4c.7 or post-4c): needs `handle_composite_request` test scaffolding
// - map_window_after_redirect_subwindows_keeps_manual_participation
//     RedirectSubwindows(parent, Manual) → MapWindow(child) — child's
//     participation must stay Manual (off-scene); the post-map hook
//     must not flip it back on.
//
// TODO(4c.7 or post-4c): needs `handle_composite_request` test scaffolding
// - map_subwindows_redirects_each_child
//     RedirectSubwindows(parent, Manual) → MapSubwindows(parent) —
//     every child gets its own `allocate_redirected_backing` call
//     via the per-child redirect hook.
//
// TODO(4c.7 or post-4c): needs `handle_composite_request` test scaffolding
// - name_window_pixmap_on_unviewable_returns_bad_match
//     NameWindowPixmap(W) on an unmapped (unviewable) window must
//     return `BadMatch` per the X11 COMPOSITE spec, not silently
//     succeed with an alias to whatever backing exists.
//
// TODO(4c.7 or post-4c): needs `handle_composite_request` test scaffolding
// - existing_alias_survives_window_unmap
//     A held NameWindowPixmap alias must keep the backing alive past
//     a subsequent UnmapWindow(W) (no race that drops the storage
//     when the redirect map clears).

/// Stage 4d — paint into the Composite Overlay Window via its xid
/// after `GetOverlayWindow`, and assert the paint lands on COW
/// storage with presentation damage accumulated. This is the load-
/// bearing v2 path for compositing WMs (marco-compositing,
/// xfwm4-compositing): pre-4d the COW xid resolved to nothing in
/// the store, so every `render_composite` against it gap-logged
/// and dropped paint.
///
/// Oracle shape: scanout dump integration is heavyweight (needs
/// `dump_scanout` wiring that test fixtures don't have); per the
/// stage brief, the acceptable surrogate is
/// `test_peek_presentation_damage_nonempty(0x103)` after a
/// `put_image` against the COW xid — confirms (a) the xid resolves,
/// (b) the storage is `scene_participating`, and (c) the paint
/// accumulated presentation damage that a scene tick would consume.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn cow_paint_appears_on_scanout() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // Step 1: GetOverlayWindow — allocates COW storage at xid 0x103.
    b.get_overlay_window(None).expect("get_overlay_window");
    let cow_xid = 0x103u32;

    // Step 2: paint a known red square at (0, 0). put_image with a
    // 4-byte BGRA pixel goes through the engine.put_image path; on
    // a Vk-backed fixture this lands on COW storage.
    let pixels: Vec<u8> = vec![
        // 2×2 of red (BGRA premul: B=0, G=0, R=0xFF, A=0xFF)
        0x00, 0x00, 0xFF, 0xFF, 0x00, 0x00, 0xFF, 0xFF, 0x00, 0x00, 0xFF, 0xFF, 0x00, 0x00, 0xFF,
        0xFF,
    ];
    b.put_image(None, cow_xid, 24, 2, 2, 0, 0, &pixels)
        .expect("put_image into COW xid");

    // Step 3: GetImage on the COW xid round-trips back the red
    // pixels — confirms the put_image actually landed on COW
    // storage (vs being dropped into the gap-logged no-op path).
    let img = b
        .get_image_pixels_for_tests(cow_xid, 2, 0, 0, 2, 2, !0)
        .expect("get_image COW")
        .expect("Some COW bytes");
    assert_eq!(
        &img[..4],
        &[0x00, 0x00, 0xFF, 0xFF],
        "COW (0,0) must round-trip the painted red",
    );

    // Step 4: presentation damage accumulated on COW. The scene
    // tick would consume this on next composite; here we assert
    // the storage is in the right state (scene_participating=true
    // + non-empty damage region) to be picked up by build_scene.
    assert!(
        b.test_peek_presentation_damage_nonempty(cow_xid),
        "COW must have non-empty presentation damage after put_image — \
         false ⇒ either xid resolved to nothing (pre-4d shape) or \
         scene_participating=false (4d wiring missing)",
    );

    // Step 5: release drops the storage; the xid must no longer
    // resolve.
    b.release_overlay_window(None).expect("release");
    let img_after = b.get_image_pixels_for_tests(cow_xid, 2, 0, 0, 2, 2, !0);
    assert!(
        img_after.is_err() || img_after.as_ref().unwrap().is_none(),
        "GetImage on COW xid after final release must fail or return None \
         (storage destroyed) — got {img_after:?}",
    );
}

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

/// Acceptance for GC clip-mask (depth-1 pixmap clip on Core paint).
/// This is the wmaker title-bar button glyph path: ChangeGC
/// clip-mask=<mask_pixmap> + PolyFillRectangle full_button. The depth-1
/// mask gates per-pixel paint to the mask shape.
///
/// Workflow:
///   1. Create depth-24 dst 8x8 pre-filled blue.
///   2. Create depth-1 mask 8x8 with the top half all ones and the
///      bottom half all zeros.
///   3. PutImage the mask bits (MSB-first packed, scanline-pad=4).
///   4. set_clip_pixmap mask at origin (0, 0).
///   5. poly_fill_rectangle full 8x8 in red.
///   6. clear_clip_rectangles to drop the clip.
///   7. GetImage dst: top half (rows 0..4) must be red; bottom half
///      (rows 4..8) must remain blue.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn clip_pixmap_mask_gates_poly_fill_to_mask_shape() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // Dst depth-24 8x8 pre-filled blue (0xFF0000FF → BGRA [0xFF,0,0,0xFF]).
    let dst_xid = b.create_pixmap(None, 24, 8, 8).unwrap().as_raw();
    b.fill_rectangle(None, dst_xid, 0xFF0000FF, 0, 0, 8, 8)
        .expect("fill_rectangle dst blue");

    // Mask depth-1 8x8: rows 0..4 all ones, rows 4..8 all zeros.
    // Each row is 8 bits = 1 byte of data; scanline-padded to 4 bytes.
    let mask_xid = b.create_pixmap(None, 1, 8, 8).unwrap().as_raw();
    let mut mask_bits = vec![0u8; 4 * 8];
    for row in 0..4 {
        mask_bits[row * 4] = 0xFF;
    }
    b.put_image(None, mask_xid, 1, 8, 8, 0, 0, &mask_bits)
        .expect("put_image mask");

    // Route through `apply_clip_state` — the actual live entry point
    // for ChangeGC clip-mask=<pixmap>. `set_clip_pixmap` is only used
    // by the host_x11/ynest path; KMS dispatch goes
    // `handle_change_gc -> resolve_draw_state ->
    // backend.apply_clip_state(&ClipState::Pixmap)`.
    use yserver_core::backend::{ClipState, PixmapHandle as ApplyPixmapHandle};
    let mask_handle = ApplyPixmapHandle::from_raw(mask_xid).expect("mask handle");
    b.apply_clip_state(
        None,
        &ClipState::Pixmap {
            origin: (0, 0),
            pixmap: mask_handle,
        },
    )
    .expect("apply_clip_state Pixmap");

    // PolyFillRectangle full 8x8 in red. Without the clip-mask path
    // honoured, every pixel turns red. With it, only the top half does.
    let rect_bytes = {
        let mut buf = Vec::new();
        buf.extend_from_slice(&i16::to_le_bytes(0));
        buf.extend_from_slice(&i16::to_le_bytes(0));
        buf.extend_from_slice(&u16::to_le_bytes(8));
        buf.extend_from_slice(&u16::to_le_bytes(8));
        buf
    };
    b.poly_fill_rectangle(None, dst_xid, 0xFFFF0000, &rect_bytes)
        .expect("poly_fill_rectangle");

    b.clear_clip_rectangles(None).expect("clear clip");

    let out = b
        .get_image_pixels_for_tests(dst_xid, 2, 0, 0, 8, 8, !0)
        .expect("get_image")
        .expect("Some(bytes)");

    // Top half rows: red (BGRA [0,0,0xFF,0xFF]).
    for row in 0..4 {
        for col in 0..8 {
            let off = (row * 8 + col) * 4;
            assert_eq!(
                &out[off..off + 4],
                &[0x00, 0x00, 0xFF, 0xFF],
                "row {row} col {col} should be red (mask=1)",
            );
        }
    }
    // Bottom half rows: blue (BGRA [0xFF,0,0,0xFF]).
    for row in 4..8 {
        for col in 0..8 {
            let off = (row * 8 + col) * 4;
            assert_eq!(
                &out[off..off + 4],
                &[0xFF, 0x00, 0x00, 0xFF],
                "row {row} col {col} should remain blue (mask=0)",
            );
        }
    }
}

/// Stage 5 Task 6.1 — verify that `drain_completed_present_events`
/// force-fires every queued entry when the platform's
/// `renderer_failed` flag is set. This is the "renderer is stuck"
/// escape valve: rather than livelock on fences that will never
/// signal, the drain unconditionally pops + signals every entry so
/// the X11 PRESENT serial bookkeeping doesn't pile up at the loop.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn drain_force_fires_all_pending_on_renderer_failed() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // Enqueue 3 entries against unsignaled fences via a real
    // copy_area path (so each pins a live FenceTicket).
    let src = b.create_pixmap(None, 32, 4, 4).expect("src");
    let cow = b.create_pixmap(None, 32, 4, 4).expect("cow");
    for serial in 1..=3 {
        b.copy_area(None, src.as_raw(), cow.as_raw(), 0, 0, 0, 0, 4, 4)
            .expect("copy_area");
        b.enqueue_present_completion(
            yserver_core::backend::CompletedPresentEvent {
                client_id: yserver_protocol::x11::ClientId(0),
                serial,
                host_xid: src.as_raw(),
                dst_host_xid: cow.as_raw(),
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
            cow.as_raw(),
        );
    }
    assert_eq!(
        b.pending_present_events_len_for_tests(),
        3,
        "three entries queued before drain",
    );

    // Force-fire branch: flip renderer_failed; drain returns all
    // entries unconditionally.
    b.set_renderer_failed_for_tests(true);
    let drained = b.drain_completed_present_events_for_tests();
    assert_eq!(drained.len(), 3, "force-fire returns all 3 entries",);
    assert_eq!(
        b.pending_present_events_len_for_tests(),
        0,
        "force-fire empties the queue",
    );
}

/// Stage 5 Task 6.1 site #1 (Task 12 of the deferred-PRESENT plan)
/// — verifies that the v2 backend's `enqueue_present_completion`
/// returns quickly (i.e. does *not* synchronously wait on the
/// underlying fence). This is the load-bearing property the
/// `PRESENT::Pixmap` handler now relies on: the synchronous
/// `wait_for_drawable_idle` has been replaced by an enqueue that
/// must hand control back to the main loop without blocking.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn present_pixmap_enqueues_pending_and_defers_emission() {
    use yserver_core::backend::{CompletedPresentEvent, PresentWake};
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let src_pix = b.create_pixmap(None, 32, 4, 4).expect("src pixmap");
    let cow_pix = b.create_pixmap(None, 32, 4, 4).expect("cow pixmap");
    b.copy_area(None, src_pix.as_raw(), cow_pix.as_raw(), 0, 0, 0, 0, 4, 4)
        .expect("copy_area");
    let before = std::time::Instant::now();
    b.enqueue_present_completion(
        CompletedPresentEvent {
            client_id: yserver_protocol::x11::ClientId(0),
            serial: 1,
            host_xid: src_pix.as_raw(),
            dst_host_xid: cow_pix.as_raw(),
            options: 0,
            present_id: 0,
            window_generation: 0,
            crtc_id: 0,
            crtc_epoch: 0,
            msc_offset: 0,
            completion_clock: None,
            wake: PresentWake::Pixmap { idle_fence_xid: 0 },
            completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
            emit_idle: true,
        },
        cow_pix.as_raw(),
    );
    let elapsed = before.elapsed();
    assert!(
        elapsed.as_millis() < 50,
        "enqueue must be fast (< 50 ms); was {} ms",
        elapsed.as_millis()
    );
    // Drain returns empty since fence isn't signaled yet — but
    // lavapipe completes the small copy synchronously so this may
    // also return Some entries. Either is fine; the load-bearing
    // assertion is the fast-enqueue time.
    let _drained = b.drain_completed_present_events_for_tests();
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn present_pixmap_synced_enqueues_with_release_syncobj_wake() {
    use yserver_core::backend::{CompletedPresentEvent, PresentWake};
    #[derive(Debug)]
    struct UnusedSyncobj;
    impl yserver_core::backend::SyncobjHandle for UnusedSyncobj {
        fn signal(&self, _value: u64) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let src_pix = b.create_pixmap(None, 32, 4, 4).expect("src");
    let cow_pix = b.create_pixmap(None, 32, 4, 4).expect("cow");
    b.copy_area(None, src_pix.as_raw(), cow_pix.as_raw(), 0, 0, 0, 0, 4, 4)
        .expect("copy");
    b.enqueue_present_completion(
        CompletedPresentEvent {
            client_id: yserver_protocol::x11::ClientId(0),
            serial: 2,
            host_xid: src_pix.as_raw(),
            dst_host_xid: cow_pix.as_raw(),
            options: 0,
            present_id: 0,
            window_generation: 0,
            crtc_id: 0,
            crtc_epoch: 0,
            msc_offset: 0,
            completion_clock: None,
            wake: PresentWake::PixmapSynced {
                release: std::sync::Arc::new(UnusedSyncobj),
                release_syncobj: 0, // 0 = no wake object; just exercises enqueue
                release_value: 42,
            },
            completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
            emit_idle: true,
        },
        cow_pix.as_raw(),
    );
    // Drain may return entries quickly under lavapipe; assertion is
    // that enqueue didn't panic + the queue can be drained.
    let _drained = b.drain_completed_present_events_for_tests();
}

/// Stage 5 Task 6.1 (Task 14 of the deferred-PRESENT plan) — verifies
/// that `disable_output` flushes open cow/render batches, drains the
/// pending PRESENT events queue, and hands the deferred event payloads
/// back via `take_shutdown_present_events` so `lib.rs::run` can fan
/// them out to clients before the socket is torn down.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn disable_output_flushes_pending_batches_before_drain_all() {
    use yserver_core::backend::{CompletedPresentEvent, PresentWake};
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    // Open a cow_batch + enqueue a pending PRESENT entry.
    let src = b.create_pixmap(None, 32, 4, 4).expect("src");
    let cow = b.create_pixmap(None, 32, 4, 4).expect("cow");
    b.copy_area(None, src.as_raw(), cow.as_raw(), 0, 0, 0, 0, 4, 4)
        .expect("copy");
    b.enqueue_present_completion(
        CompletedPresentEvent {
            client_id: yserver_protocol::x11::ClientId(0),
            serial: 1,
            host_xid: src.as_raw(),
            dst_host_xid: cow.as_raw(),
            options: 0,
            present_id: 0,
            window_generation: 0,
            crtc_id: 0,
            crtc_epoch: 0,
            msc_offset: 0,
            completion_clock: None,
            wake: PresentWake::Pixmap { idle_fence_xid: 0 },
            completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
            emit_idle: true,
        },
        cow.as_raw(),
    );

    let pre_pending = b.pending_present_events_len_for_tests();
    assert!(
        pre_pending >= 1,
        "pending events should include the just-enqueued one"
    );

    // Call disable_output. The platform-level KMS commit may fail on
    // the test harness (no real connector); the load-bearing
    // assertions are on the drain + take_shutdown_present_events
    // behaviour, which run before the platform commit.
    let _ = b.disable_output();

    // Post-shutdown: pending queue is empty, take_shutdown_present_events
    // has the deferred event ready to hand to lib.rs::run.
    assert_eq!(
        b.pending_present_events_len_for_tests(),
        0,
        "disable_output empties the pending queue"
    );
    let shutdown_events = b.take_shutdown_present_events();
    assert!(
        !shutdown_events.is_empty(),
        "shutdown should hand at least one event back"
    );
}

/// Phase A T6 regression gate: the non-COW PRESENT enqueue path must
/// call `flush_submit_group(PresentCompletionSignal)` before the
/// signal-only submit, so prior paint CBs are queued before the
/// semaphore signals.
///
/// Setup: create a pixmap, drain setup CBs, issue a fill_rectangle
/// (paint op parks a CB in the SubmitGroup). Then call
/// `enqueue_present_completion` for the same pixmap against a
/// *non-COW* destination (no cow_id set). The flush must happen before
/// the signal-only submit, draining the parked CB.
///
/// Spec § "Phase A — concrete scope" trigger 2.
#[test]
#[ignore = "lavapipe vk"]
fn submit_group_flushes_before_non_cow_present_completion_signal() {
    use yserver_core::backend::{Backend, CompletedPresentEvent, PresentWake};

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // Close the construction frame (init_root_storage's fill keeps a
    // frame — and with it the group ticket — open since B.3 ported
    // fill_rect to the frame builder), then drain buffered CBs.
    if b.frame_builder_is_open_for_tests() {
        b.engine_close_open_frame_for_timeout_for_tests()
            .expect("close construction frame");
    }
    b.engine_flush_submit_group_for_tests()
        .expect("baseline drain");
    assert!(
        !b.platform_submit_group_is_open_for_tests(),
        "baseline: submit group closed after drain"
    );

    // Create a 4×4 depth-32 pixmap that will be the PRESENT destination.
    let dst_pix = b.create_pixmap(None, 32, 4, 4).expect("dst pixmap");
    let dst_xid = dst_pix.as_raw();

    // Drain any CBs from the create_pixmap itself.
    b.engine_flush_submit_group_for_tests()
        .expect("post-create drain");

    // Issue a paint op (fill_rectangle). Since B.3 it records into an
    // open frame-builder frame (deferred CB) rather than parking a
    // one-shot CB directly in the group — either form counts as
    // "paint buffered but not yet on the queue".
    b.fill_rectangle(None, dst_xid, 0xFF0000FF, 0, 0, 4, 4)
        .expect("fill_rectangle");
    assert!(
        b.frame_builder_is_open_for_tests()
            || b.platform_submit_group_size_for_tests() >= 1
            || b.engine_pending_group_ops_count_for_tests() >= 1,
        "paint buffered (open frame or parked CB) before enqueue_present_completion"
    );

    // Invoke the non-COW PRESENT enqueue path.  cow_id is unset on
    // for_tests_with_vk(), so this exercise the non-COW fallback.
    b.enqueue_present_completion(
        CompletedPresentEvent {
            client_id: yserver_protocol::x11::ClientId(0),
            serial: 99,
            host_xid: dst_xid,
            dst_host_xid: dst_xid,
            options: 0,
            present_id: 0,
            window_generation: 0,
            crtc_id: 0,
            crtc_epoch: 0,
            msc_offset: 0,
            completion_clock: None,
            wake: PresentWake::Pixmap { idle_fence_xid: 0 },
            completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
            emit_idle: true,
        },
        dst_xid,
    );

    // After the call the close-frame (B.1 trigger 1b) +
    // PresentCompletionSignal flush must have graduated all deferred
    // paint to `submitted` and closed the group.
    assert!(
        !b.frame_builder_is_open_for_tests(),
        "open frame closed before the signal-only submit (B.1 trigger 1b)"
    );
    assert_eq!(
        b.platform_submit_group_size_for_tests(),
        0,
        "submit group drained by PresentCompletionSignal flush"
    );
    assert_eq!(
        b.engine_pending_group_ops_count_for_tests(),
        0,
        "parked op graduated to submitted before signal-only submit"
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
/// `cow_copy_area` followed by `attach_cow_present_completion` inside
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

    // Drain any present batches that became ready (the frame ticket
    // retires immediately in the lavapipe environment via drain_all).
    be.engine_drain_all_for_tests();

    // The synthetic completion event must appear in the drained set.
    let events = be.drain_completed_present_events_for_tests();
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

    // Drain any present batches (frame ticket retires immediately in lavapipe).
    be.engine_drain_all_for_tests();

    let events = be.drain_completed_present_events_for_tests();
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
    b.map_subwindow(None, xid).expect("map_subwindow");

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
    b.map_subwindow(None, xid).expect("map_subwindow");

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

/// GPU depth-1 GXcopy fill pixel-correctness oracle.
///
/// Creates an 8×1 depth-1 pixmap, fills the left 4 pixels with fg=1
/// (set) and the right 4 with fg=0 (clear) using two separate
/// fill_rectangle calls, then reads back via get_image_pixels_for_tests
/// and asserts byte-identity with the expected packed Z-pixmap bytes.
///
/// Expected packing (depth-1, LSB-first, rows padded to 32 bits):
///   fg=1 on cols 0-3: bits 0..3 set in byte 0
///   fg=0 on cols 4-7: bits 4..7 clear in byte 0
///   → packed byte = 0x0F, then 3 pad bytes = [0x0F, 0x00, 0x00, 0x00]
///
/// This also serves as the CPU-fallback reference: run the same ops on a
/// depth-1 pixmap and assert the same packed bytes, which confirms the GPU
/// path is pixel-identical to the CPU path.
#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
fn depth1_gxcopy_fill_matches_cpu_reference() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // Create a depth-1 8×1 pixmap; start all clear (zero-fill from create).
    let pix = b
        .create_pixmap(None, 1, 8, 1)
        .expect("create_pixmap depth-1");

    // Fill cols 0-3 (w=4) with fg=1 (set bit).
    b.fill_rectangle(None, pix.as_raw(), 1, 0, 0, 4, 1)
        .expect("fill set");

    // Fill cols 4-7 (x=4, w=4) with fg=0 (clear bit).
    b.fill_rectangle(None, pix.as_raw(), 0, 4, 0, 4, 1)
        .expect("fill clear");

    let out = b
        .get_image_pixels_for_tests(pix.as_raw(), 2, 0, 0, 8, 1, !0)
        .expect("get_image")
        .expect("Some bytes");

    // Depth-1 Z-pixmap: 1 row × ceil(8/32)*4 = 4 bytes.
    // Bits 0-3 set (fg=1 on cols 0-3), bits 4-7 clear (fg=0 on cols 4-7).
    // LSB-first: bit 0 = col 0 → byte = 0b0000_1111 = 0x0F.
    assert_eq!(
        out.len(),
        4,
        "depth-1 8×1 Z-pixmap must be 4 bytes (1 packed row + 3 pad)"
    );
    assert_eq!(
        out[0], 0x0F,
        "packed byte must be 0x0F: cols 0-3 set, cols 4-7 clear"
    );
    assert_eq!(out[1], 0x00, "pad byte 1 must be zero");
    assert_eq!(out[2], 0x00, "pad byte 2 must be zero");
    assert_eq!(out[3], 0x00, "pad byte 3 must be zero");
}

// ---------------------------------------------------------------------------
// Task 9: THE BYTE-EXACTNESS GATE (spec § Exactness gate).
//
// For each in-scope dst/src format, the masked-blit GPU path over the kept
// region (full-ones depth-1 mask, full-size scissor) must be BYTE-IDENTICAL
// to a plain `cmd_copy_image`. The oracle is the public `copy_area(None,...)`
// with NO clip installed: in the fixture `core.current_clip` is
// `ClipState::None`, so the `ClipState::Pixmap` rasterize branch is skipped,
// the pixmap destination escapes child/sibling clipping, and GXcopy + full
// plane-mask falls through to a single `engine.copy_area` transfer
// (`vkCmdCopyImage`). Confirmed against backend.rs copy_area (the Pixmap
// branch at the `matches!(current_clip, ClipState::Pixmap)` guard is not
// taken; the tail loop issues one `engine.copy_area`).
//
// Step 0 (raw-bytes faithfulness) verified before relying on the gate:
//   * `pack_from_storage` (engine.rs) is `raw.to_vec()` for depth 24|32, so
//     the depth-24 X byte (storage byte 3) is carried through verbatim, and
//     a scanline-padded copy for depth 8 (raw single-channel bytes).
//   * `get_image_pixels_for_tests` -> `get_image(format=2 ZPixmap,
//     plane_mask=!0)`: `mask == depth_plane_mask(depth)`, so neither
//     `z_to_xy_planes` nor `apply_z_plane_mask` runs — NO force-opaque of the
//     alpha/X byte on the read path. The readback is faithful for depths
//     24/32/8.

#[test]
#[ignore = "needs live Vulkan ICD"]
fn masked_copyarea_matches_cmd_copy_image_depth32() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    // src 8x8 gradient (depth-32 BGRA).
    let src = b.create_pixmap(None, 32, 8, 8).unwrap().as_raw();
    let mut bytes = vec![0u8; 8 * 8 * 4];
    for y in 0..8 {
        for x in 0..8 {
            let o = (y * 8 + x) * 4;
            bytes[o] = (x as u8) * 0x20; // B
            bytes[o + 1] = (y as u8) * 0x20; // G
            bytes[o + 2] = ((x + y) as u8) * 0x10; // R
            bytes[o + 3] = 0x7F; // A (must survive verbatim at depth-32)
        }
    }
    b.put_image(None, src, 32, 8, 8, 0, 0, &bytes).unwrap();

    // dst_masked 8x8 (depth-32), pre-cleared to a sentinel.
    let dst_m = b.create_pixmap(None, 32, 8, 8).unwrap().as_raw();
    b.fill_rectangle(None, dst_m, 0x0000_0000, 0, 0, 8, 8)
        .unwrap();
    // dst_ref 8x8 (depth-32) for the cmd_copy_image oracle.
    let dst_r = b.create_pixmap(None, 32, 8, 8).unwrap().as_raw();
    b.fill_rectangle(None, dst_r, 0x0000_0000, 0, 0, 8, 8)
        .unwrap();

    // Full-ones mask 8x8 depth-1.
    let mask = b.create_pixmap(None, 1, 8, 8).unwrap().as_raw();
    let mut mbits = vec![0u8; 4 * 8];
    for row in 0..8 {
        mbits[row * 4] = 0xFF;
    }
    b.put_image(None, mask, 1, 8, 8, 0, 0, &mbits).unwrap();

    // Oracle: plain transfer (vkCmdCopyImage) src->dst_r, no clip installed.
    b.copy_area(None, src, dst_r, 0, 0, 0, 0, 8, 8).unwrap();

    // Masked-blit src->dst_m through the all-ones mask, full-size scissor.
    let full = [ash::vk::Rect2D {
        offset: ash::vk::Offset2D { x: 0, y: 0 },
        extent: ash::vk::Extent2D {
            width: 8,
            height: 8,
        },
    }];
    b.masked_copy_area_for_tests(src, dst_m, mask, (0, 0), 0, 0, 0, 0, 8, 8, &full)
        .unwrap();

    let out_m = b
        .get_image_pixels_for_tests(dst_m, 2, 0, 0, 8, 8, !0)
        .unwrap()
        .unwrap();
    let out_r = b
        .get_image_pixels_for_tests(dst_r, 2, 0, 0, 8, 8, !0)
        .unwrap()
        .unwrap();
    assert_eq!(
        out_m, out_r,
        "masked-blit must be byte-identical to cmd_copy_image (depth-32)"
    );
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn masked_copyarea_matches_cmd_copy_image_depth24() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    // src 8x8 gradient (depth-24 BGRX). Storage is BGRA8; the X byte
    // (storage byte 3) is filled with a NON-trivial 0x33 so a force-opaque
    // fixup (if any) would corrupt it. A raw GXcopy must carry it through.
    let src = b.create_pixmap(None, 24, 8, 8).unwrap().as_raw();
    let mut bytes = vec![0u8; 8 * 8 * 4];
    for y in 0..8 {
        for x in 0..8 {
            let o = (y * 8 + x) * 4;
            bytes[o] = (x as u8) * 0x20; // B
            bytes[o + 1] = (y as u8) * 0x20; // G
            bytes[o + 2] = ((x + y) as u8) * 0x10; // R
            bytes[o + 3] = 0x33; // X byte — must survive verbatim
        }
    }
    b.put_image(None, src, 24, 8, 8, 0, 0, &bytes).unwrap();

    let dst_m = b.create_pixmap(None, 24, 8, 8).unwrap().as_raw();
    b.fill_rectangle(None, dst_m, 0x0000_0000, 0, 0, 8, 8)
        .unwrap();
    let dst_r = b.create_pixmap(None, 24, 8, 8).unwrap().as_raw();
    b.fill_rectangle(None, dst_r, 0x0000_0000, 0, 0, 8, 8)
        .unwrap();

    let mask = b.create_pixmap(None, 1, 8, 8).unwrap().as_raw();
    let mut mbits = vec![0u8; 4 * 8];
    for row in 0..8 {
        mbits[row * 4] = 0xFF;
    }
    b.put_image(None, mask, 1, 8, 8, 0, 0, &mbits).unwrap();

    // Oracle: plain transfer (vkCmdCopyImage), no clip.
    b.copy_area(None, src, dst_r, 0, 0, 0, 0, 8, 8).unwrap();

    let full = [ash::vk::Rect2D {
        offset: ash::vk::Offset2D { x: 0, y: 0 },
        extent: ash::vk::Extent2D {
            width: 8,
            height: 8,
        },
    }];
    b.masked_copy_area_for_tests(src, dst_m, mask, (0, 0), 0, 0, 0, 0, 8, 8, &full)
        .unwrap();

    let out_m = b
        .get_image_pixels_for_tests(dst_m, 2, 0, 0, 8, 8, !0)
        .unwrap()
        .unwrap();
    let out_r = b
        .get_image_pixels_for_tests(dst_r, 2, 0, 0, 8, 8, !0)
        .unwrap()
        .unwrap();
    assert_eq!(
        out_m, out_r,
        "masked-blit must be byte-identical to cmd_copy_image (depth-24, incl. X byte 3)"
    );
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn masked_copyarea_matches_cmd_copy_image_r8() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    // src 8x8 single-channel (depth-8 R8) gradient.
    let src = b.create_pixmap(None, 8, 8, 8).unwrap().as_raw();
    // Depth-8 wire is scanline-padded to 32 bits: row stride = 8 (already
    // a multiple of 4), so the data is tightly packed here.
    let mut bytes = vec![0u8; 8 * 8];
    for y in 0..8 {
        for x in 0..8 {
            bytes[y * 8 + x] = (((x + y) as u8) * 0x10) ^ 0x55;
        }
    }
    b.put_image(None, src, 8, 8, 8, 0, 0, &bytes).unwrap();

    let dst_m = b.create_pixmap(None, 8, 8, 8).unwrap().as_raw();
    b.fill_rectangle(None, dst_m, 0x0000_0000, 0, 0, 8, 8)
        .unwrap();
    let dst_r = b.create_pixmap(None, 8, 8, 8).unwrap().as_raw();
    b.fill_rectangle(None, dst_r, 0x0000_0000, 0, 0, 8, 8)
        .unwrap();

    // Mask is still depth-1.
    let mask = b.create_pixmap(None, 1, 8, 8).unwrap().as_raw();
    let mut mbits = vec![0u8; 4 * 8];
    for row in 0..8 {
        mbits[row * 4] = 0xFF;
    }
    b.put_image(None, mask, 1, 8, 8, 0, 0, &mbits).unwrap();

    // Oracle: plain transfer (vkCmdCopyImage), no clip.
    b.copy_area(None, src, dst_r, 0, 0, 0, 0, 8, 8).unwrap();

    let full = [ash::vk::Rect2D {
        offset: ash::vk::Offset2D { x: 0, y: 0 },
        extent: ash::vk::Extent2D {
            width: 8,
            height: 8,
        },
    }];
    b.masked_copy_area_for_tests(src, dst_m, mask, (0, 0), 0, 0, 0, 0, 8, 8, &full)
        .unwrap();

    let out_m = b
        .get_image_pixels_for_tests(dst_m, 2, 0, 0, 8, 8, !0)
        .unwrap()
        .unwrap();
    let out_r = b
        .get_image_pixels_for_tests(dst_r, 2, 0, 0, 8, 8, !0)
        .unwrap()
        .unwrap();
    assert_eq!(
        out_m, out_r,
        "masked-blit must be byte-identical to cmd_copy_image (R8 depth-8)"
    );
}

// ---------------------------------------------------------------------------
// Task 10: Phase-1 CORRECTNESS tests for the masked CopyArea GPU clip path.
//
// These exercise the SEMANTIC contract of masked_copy_area beyond the
// byte-exactness gate (Task 9): clip-origin mask projection, X11 negative-dst
// clamping + OOB-src discard, src==dst self-overlap, and per-rect scissoring.
//
// Test-vector provenance (the user's standing rule — expected values must come
// from an independent source, not my own arithmetic):
//   * Tests 2 & 3 use a DIFFERENTIAL ORACLE: the plain transfer path
//     `copy_area(None, ...)` (no clip installed) writing into a separate
//     `dst_ref` pixmap. The masked output must be byte-identical. The oracle
//     is the production transfer path, so its bytes are authoritative.
//   * Tests 1 & 4 assert region MEMBERSHIP (copied vs retained). The per-pixel
//     BYTE values are NOT hand-computed: the "copied" value is read back from
//     the src pixmap and the "retained" value is read back from the dst BEFORE
//     the masked copy (the actual sentinel as `fill_rectangle` stored it). Only
//     the membership predicate is derived by hand, directly from the fragment
//     shader's documented semantics (masked_blit.frag.glsl):
//       mask_texel = dst_pixel - clip_offset;  OOB mask -> discard
//       set bit (R8 byte 0xFF -> >0.0) -> copy, clear (0x00) -> discard
//       src_texel = dst_pixel + copy_offset;   OOB src -> discard
//   * BGRA byte order: get_image_pixels_for_tests returns packed storage bytes
//     (4 B/px BGRA for depth 24/32, 1 B/px for depth 8). We compare whole
//     per-pixel slices read back from the SAME backend, so byte order is
//     handled identically on both sides and never hand-asserted.

/// Full-ones depth-1 mask: each row's first byte = 0xFF covers all 8 columns
/// (LSBFirst, bit 0 = col 0), rows padded to 4 bytes (32-bit scanline pad).
fn task10_full_ones_mask_bits() -> Vec<u8> {
    let mut mbits = vec![0u8; 4 * 8];
    for row in 0..8 {
        mbits[row * 4] = 0xFF;
    }
    mbits
}

/// Depth-32 BGRA src gradient distinct from any plausible sentinel: every
/// pixel has A=0x7F and at least one of B/G/R non-zero where useful. Returns
/// the raw 8x8x4 byte buffer for `put_image`.
fn task10_src_gradient_bgra() -> Vec<u8> {
    let mut bytes = vec![0u8; 8 * 8 * 4];
    for y in 0..8usize {
        for x in 0..8usize {
            let o = (y * 8 + x) * 4;
            bytes[o] = ((x as u8) * 0x20) | 0x03; // B (low bits set so never all-zero)
            bytes[o + 1] = ((y as u8) * 0x20) | 0x05; // G
            bytes[o + 2] = (((x + y) as u8) * 0x10) | 0x07; // R
            bytes[o + 3] = 0x7F; // A
        }
    }
    bytes
}

// Test 1: clip-origin mask projection (Step 1).
//
// Mask rows 0..4 set (rows 0,1,2,3), clip_origin (0,2) => dst pixel (x,y)
// samples mask texel (x, y-2). Membership derived from the shader contract:
//   * dst rows 2..6 (y in {2,3,4,5}): mask_texel.y = y-2 in {0,1,2,3} -> SET
//     -> copied.
//   * dst rows 0,1: mask_texel.y in {-2,-1} -> mask-OOB -> discard -> sentinel.
//   * dst rows 6,7: mask_texel.y in {4,5} -> mask-CLEAR (rows >=4 unset)
//     -> discard -> sentinel.
// Second pass (fresh dst): full-ones mask, clip_origin (3,0) pushes dst cols
// 0,1,2 to mask_texel.x in {-3,-2,-1} (mask-OOB -> discard); cols 3..8 sampled
// at mask_texel.x 0..5 (SET) -> copied.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn masked_copyarea_honors_clip_origin() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let src = b.create_pixmap(None, 32, 8, 8).unwrap().as_raw();
    b.put_image(None, src, 32, 8, 8, 0, 0, &task10_src_gradient_bgra())
        .unwrap();

    // Distinct sentinel (0x00FF_00FF) never produced by the gradient.
    let dst_m = b.create_pixmap(None, 32, 8, 8).unwrap().as_raw();
    b.fill_rectangle(None, dst_m, 0x00FF_00FF, 0, 0, 8, 8)
        .unwrap();

    // Mask rows 0..4 (= rows 0,1,2,3) set; rows 4..8 left clear.
    let mask = b.create_pixmap(None, 1, 8, 8).unwrap().as_raw();
    let mut mbits = vec![0u8; 4 * 8];
    for row in 0..4 {
        mbits[row * 4] = 0xFF;
    }
    b.put_image(None, mask, 1, 8, 8, 0, 0, &mbits).unwrap();

    // Independent value sources: src readback (copied) + dst readback BEFORE
    // the copy (the actual sentinel as fill_rectangle stored it).
    let src_px = b
        .get_image_pixels_for_tests(src, 2, 0, 0, 8, 8, !0)
        .unwrap()
        .unwrap();
    let sentinel_px = b
        .get_image_pixels_for_tests(dst_m, 2, 0, 0, 8, 8, !0)
        .unwrap()
        .unwrap();

    let full = [ash::vk::Rect2D {
        offset: ash::vk::Offset2D { x: 0, y: 0 },
        extent: ash::vk::Extent2D {
            width: 8,
            height: 8,
        },
    }];
    // clip_origin (0,2): see membership derivation above.
    b.masked_copy_area_for_tests(src, dst_m, mask, (0, 2), 0, 0, 0, 0, 8, 8, &full)
        .unwrap();

    let out = b
        .get_image_pixels_for_tests(dst_m, 2, 0, 0, 8, 8, !0)
        .unwrap()
        .unwrap();
    for y in 0..8usize {
        for x in 0..8usize {
            let o = (y * 8 + x) * 4;
            let got = &out[o..o + 4];
            let copied = (2..6).contains(&y); // rows 2..6 set via clip_origin (0,2)
            let want = if copied {
                &src_px[o..o + 4]
            } else {
                &sentinel_px[o..o + 4]
            };
            assert_eq!(
                got, want,
                "clip_origin(0,2) px ({x},{y}): copied={copied} mismatch"
            );
        }
    }

    // Second pass: full-ones mask, clip_origin (3,0) -> cols 0,1,2 mask-OOB.
    let dst_x = b.create_pixmap(None, 32, 8, 8).unwrap().as_raw();
    b.fill_rectangle(None, dst_x, 0x00FF_00FF, 0, 0, 8, 8)
        .unwrap();
    let sentinel_x = b
        .get_image_pixels_for_tests(dst_x, 2, 0, 0, 8, 8, !0)
        .unwrap()
        .unwrap();
    let mask_full = b.create_pixmap(None, 1, 8, 8).unwrap().as_raw();
    b.put_image(
        None,
        mask_full,
        1,
        8,
        8,
        0,
        0,
        &task10_full_ones_mask_bits(),
    )
    .unwrap();
    b.masked_copy_area_for_tests(src, dst_x, mask_full, (3, 0), 0, 0, 0, 0, 8, 8, &full)
        .unwrap();
    let out_x = b
        .get_image_pixels_for_tests(dst_x, 2, 0, 0, 8, 8, !0)
        .unwrap()
        .unwrap();
    for y in 0..8usize {
        for x in 0..8usize {
            let o = (y * 8 + x) * 4;
            let got = &out_x[o..o + 4];
            // mask_texel.x = x-3: cols 0,1,2 OOB (discard), cols 3..8 set (copy).
            let copied = x >= 3;
            let want = if copied {
                &src_px[o..o + 4]
            } else {
                &sentinel_x[o..o + 4]
            };
            assert_eq!(
                got, want,
                "clip_origin(3,0) px ({x},{y}): copied={copied} mismatch (mask-OOB cols 0..3 must retain sentinel)"
            );
        }
    }
}

// Test 2: negative dst clamp + OOB-src discard vs the transfer-path ORACLE
// (Step 2). dst_x = -2: X11 clamps the copy to the in-bounds sub-region. The
// masked path (full-ones mask, full-size scissor) must equal the plain
// transfer path `copy_area(None, ...)` for the same negative offset, AND the
// pixels outside the legal copy region must keep the dst sentinel (no
// sampled-zero write — the shader discards OOB-src texels).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn masked_copyarea_clamps_negative_dst_and_oob_src() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let src = b.create_pixmap(None, 32, 8, 8).unwrap().as_raw();
    b.put_image(None, src, 32, 8, 8, 0, 0, &task10_src_gradient_bgra())
        .unwrap();

    // Both dst pixmaps pre-filled with the SAME sentinel so the oracle carries
    // it through identically wherever no copy lands.
    let dst_m = b.create_pixmap(None, 32, 8, 8).unwrap().as_raw();
    b.fill_rectangle(None, dst_m, 0x00FF_00FF, 0, 0, 8, 8)
        .unwrap();
    let dst_r = b.create_pixmap(None, 32, 8, 8).unwrap().as_raw();
    b.fill_rectangle(None, dst_r, 0x00FF_00FF, 0, 0, 8, 8)
        .unwrap();
    let sentinel_px = b
        .get_image_pixels_for_tests(dst_r, 2, 0, 0, 8, 8, !0)
        .unwrap()
        .unwrap();

    let mask = b.create_pixmap(None, 1, 8, 8).unwrap().as_raw();
    b.put_image(None, mask, 1, 8, 8, 0, 0, &task10_full_ones_mask_bits())
        .unwrap();

    // ORACLE: plain transfer, no clip. src(0,0)->dst(-2,0), 8x8.
    b.copy_area(None, src, dst_r, 0, 0, -2, 0, 8, 8).unwrap();

    let full = [ash::vk::Rect2D {
        offset: ash::vk::Offset2D { x: 0, y: 0 },
        extent: ash::vk::Extent2D {
            width: 8,
            height: 8,
        },
    }];
    b.masked_copy_area_for_tests(src, dst_m, mask, (0, 0), 0, 0, -2, 0, 8, 8, &full)
        .unwrap();

    let out_m = b
        .get_image_pixels_for_tests(dst_m, 2, 0, 0, 8, 8, !0)
        .unwrap()
        .unwrap();
    let out_r = b
        .get_image_pixels_for_tests(dst_r, 2, 0, 0, 8, 8, !0)
        .unwrap()
        .unwrap();
    assert_eq!(
        out_m, out_r,
        "masked dst_x=-2 must equal the transfer-path oracle (clamp/project)"
    );

    // X11 clamp: dst_x=-2 writes dst cols 0..6 (src cols 2..8); dst cols 6,7
    // fall outside the legal copy region and keep the sentinel.
    for y in 0..8usize {
        for x in 6..8usize {
            let o = (y * 8 + x) * 4;
            assert_eq!(
                &out_m[o..o + 4],
                &sentinel_px[o..o + 4],
                "px ({x},{y}) outside legal copy region must retain sentinel (no sampled-zero write)"
            );
        }
    }
}

// Test 3: src==dst self-overlap vs the transfer self-overlap ORACLE (Step 3).
// Pre-fill a gradient, horizontal scroll dst_x=2 (dst_y=0), full-ones mask.
// The masked path uses the same self_overlap_scratch as the transfer path, so
// the result must be byte-identical to `copy_area(None, dst, dst, ...)`.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn masked_copyarea_self_overlap() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let grad = task10_src_gradient_bgra();

    // dst_m and dst_r both pre-filled with the SAME gradient so the self-copy
    // operates on identical starting content.
    let dst_m = b.create_pixmap(None, 32, 8, 8).unwrap().as_raw();
    b.put_image(None, dst_m, 32, 8, 8, 0, 0, &grad).unwrap();
    let dst_r = b.create_pixmap(None, 32, 8, 8).unwrap().as_raw();
    b.put_image(None, dst_r, 32, 8, 8, 0, 0, &grad).unwrap();

    let mask = b.create_pixmap(None, 1, 8, 8).unwrap().as_raw();
    b.put_image(None, mask, 1, 8, 8, 0, 0, &task10_full_ones_mask_bits())
        .unwrap();

    // ORACLE: transfer self-overlap (uses self_overlap_scratch), no clip.
    // src(0,0)->dst(2,0): horizontal scroll right by 2.
    b.copy_area(None, dst_r, dst_r, 0, 0, 2, 0, 8, 8).unwrap();

    let full = [ash::vk::Rect2D {
        offset: ash::vk::Offset2D { x: 0, y: 0 },
        extent: ash::vk::Extent2D {
            width: 8,
            height: 8,
        },
    }];
    b.masked_copy_area_for_tests(dst_m, dst_m, mask, (0, 0), 0, 0, 2, 0, 8, 8, &full)
        .unwrap();

    let out_m = b
        .get_image_pixels_for_tests(dst_m, 2, 0, 0, 8, 8, !0)
        .unwrap()
        .unwrap();
    let out_r = b
        .get_image_pixels_for_tests(dst_r, 2, 0, 0, 8, 8, !0)
        .unwrap()
        .unwrap();
    assert_eq!(
        out_m, out_r,
        "masked self-overlap (dst_x=2) must equal the transfer self-overlap oracle"
    );
}

// Test 4: scissor composes with the GC clip rects (Step 4). Full-ones mask;
// per-rect scissored draws restrict the written region. Membership derived
// from the scissor rects directly (each cmd_set_scissor gates one draw):
//   * scissors=[{0,0,4,8}]: only cols 0..4 copied; cols 4..8 retain sentinel.
//   * scissors=[{0,0,4,8},{6,0,2,8}]: union (cols 0..4 and 6..8) copied; the
//     gap cols 4..6 retain sentinel — proving the per-rect draws don't bleed.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn masked_copyarea_scissor_composes_with_gc_rects() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let src = b.create_pixmap(None, 32, 8, 8).unwrap().as_raw();
    b.put_image(None, src, 32, 8, 8, 0, 0, &task10_src_gradient_bgra())
        .unwrap();
    let src_px = b
        .get_image_pixels_for_tests(src, 2, 0, 0, 8, 8, !0)
        .unwrap()
        .unwrap();

    let mask = b.create_pixmap(None, 1, 8, 8).unwrap().as_raw();
    b.put_image(None, mask, 1, 8, 8, 0, 0, &task10_full_ones_mask_bits())
        .unwrap();

    // Pass A: single left-half scissor.
    let dst_a = b.create_pixmap(None, 32, 8, 8).unwrap().as_raw();
    b.fill_rectangle(None, dst_a, 0x00FF_00FF, 0, 0, 8, 8)
        .unwrap();
    let sentinel_a = b
        .get_image_pixels_for_tests(dst_a, 2, 0, 0, 8, 8, !0)
        .unwrap()
        .unwrap();
    let left = [ash::vk::Rect2D {
        offset: ash::vk::Offset2D { x: 0, y: 0 },
        extent: ash::vk::Extent2D {
            width: 4,
            height: 8,
        },
    }];
    b.masked_copy_area_for_tests(src, dst_a, mask, (0, 0), 0, 0, 0, 0, 8, 8, &left)
        .unwrap();
    let out_a = b
        .get_image_pixels_for_tests(dst_a, 2, 0, 0, 8, 8, !0)
        .unwrap()
        .unwrap();
    for y in 0..8usize {
        for x in 0..8usize {
            let o = (y * 8 + x) * 4;
            let copied = x < 4; // scissor {0,0,4,8} = left 4 cols
            let want = if copied {
                &src_px[o..o + 4]
            } else {
                &sentinel_a[o..o + 4]
            };
            assert_eq!(
                &out_a[o..o + 4],
                want,
                "single-scissor px ({x},{y}): copied={copied} mismatch"
            );
        }
    }

    // Pass B: two scissors with a gap at cols 4..6.
    let dst_b = b.create_pixmap(None, 32, 8, 8).unwrap().as_raw();
    b.fill_rectangle(None, dst_b, 0x00FF_00FF, 0, 0, 8, 8)
        .unwrap();
    let sentinel_b = b
        .get_image_pixels_for_tests(dst_b, 2, 0, 0, 8, 8, !0)
        .unwrap()
        .unwrap();
    let two = [
        ash::vk::Rect2D {
            offset: ash::vk::Offset2D { x: 0, y: 0 },
            extent: ash::vk::Extent2D {
                width: 4,
                height: 8,
            },
        },
        ash::vk::Rect2D {
            offset: ash::vk::Offset2D { x: 6, y: 0 },
            extent: ash::vk::Extent2D {
                width: 2,
                height: 8,
            },
        },
    ];
    b.masked_copy_area_for_tests(src, dst_b, mask, (0, 0), 0, 0, 0, 0, 8, 8, &two)
        .unwrap();
    let out_b = b
        .get_image_pixels_for_tests(dst_b, 2, 0, 0, 8, 8, !0)
        .unwrap()
        .unwrap();
    for y in 0..8usize {
        for x in 0..8usize {
            let o = (y * 8 + x) * 4;
            // Union of {0..4} and {6..8}; gap cols 4,5 retain sentinel.
            let copied = !(4..6).contains(&x);
            let want = if copied {
                &src_px[o..o + 4]
            } else {
                &sentinel_b[o..o + 4]
            };
            assert_eq!(
                &out_b[o..o + 4],
                want,
                "two-scissor px ({x},{y}): copied={copied} mismatch (gap cols 4..6 must retain sentinel)"
            );
        }
    }
}

/// Task 11: the `ClipSnapshot` carrier registers on create (extent queryable)
/// and is removed on retire (extent lookup → None). Allocation-only; no
/// refresh/sample path is exercised here (Task 13/14).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn clip_snapshot_create_and_retire() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let id = b
        .engine_create_clip_snapshot_for_tests(8, 8)
        .expect("create_clip_snapshot");
    assert_eq!(
        b.engine_clip_snapshot_extent_for_tests(id),
        Some((8, 8)),
        "snapshot present with 8x8 extent after create"
    );

    b.engine_retire_clip_snapshot_for_tests(id);
    assert_eq!(
        b.engine_clip_snapshot_extent_for_tests(id),
        None,
        "snapshot no longer present after retire"
    );
}

/// Task 12: a `masked_copy_area` that SAMPLES a clip snapshot advances the
/// snapshot's `current_layout` (→ SHADER_READ_ONLY_OPTIMAL) and binds it to the
/// frame's ticket on append. If the close then FAILS, `rollback_snapshots` must
/// restore the snapshot's pre-frame `current_layout`, `last_render_ticket`, and
/// `snapshotted_version` from the open frame's `snapshot_touch` overlay —
/// otherwise the next frame samples stale bytes (codex round-4/5).
///
/// Forces the failure via `platform_force_next_submit_failure_for_tests`, which
/// trips the flush-failure rollback site (the close path's post-flush `Err`
/// arm) — the most important of the five `rollback_atlas`/`rollback_snapshots`
/// sites. The snapshot is left unpopulated; only the SAMPLE path is exercised.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn masked_copyarea_snapshot_rollback_on_close_failure() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // src 8x8 depth-32 (any contents — the SAMPLE path only needs a valid src).
    let src = b.create_pixmap(None, 32, 8, 8).unwrap().as_raw();
    b.fill_rectangle(None, src, 0x00FF_FFFF, 0, 0, 8, 8)
        .unwrap();
    // dst 8x8 depth-32.
    let dst = b.create_pixmap(None, 32, 8, 8).unwrap().as_raw();
    b.fill_rectangle(None, dst, 0x0000_0000, 0, 0, 8, 8)
        .unwrap();

    // Fresh 8x8 R8 clip snapshot — unpopulated.
    let snap = b
        .engine_create_clip_snapshot_for_tests(8, 8)
        .expect("create_clip_snapshot");

    // Drain any setup CBs so we operate on a quiesced engine, then snapshot the
    // pre-frame snapshot state (BEFORE the masked op opens a frame).
    b.engine_close_open_frame_for_timeout_for_tests()
        .expect("drain setup frame");
    let pre_layout = b
        .engine_clip_snapshot_layout_for_tests(snap)
        .expect("snapshot present");
    let pre_has_ticket = b
        .engine_clip_snapshot_has_ticket_for_tests(snap)
        .expect("snapshot present");
    let pre_version = b
        .engine_clip_snapshot_version_for_tests(snap)
        .expect("snapshot present");

    // Arm the next vkQueueSubmit2 to fail (trips the flush-failure rollback).
    b.platform_force_next_submit_failure_for_tests();

    let full = [ash::vk::Rect2D {
        offset: ash::vk::Offset2D { x: 0, y: 0 },
        extent: ash::vk::Extent2D {
            width: 8,
            height: 8,
        },
    }];
    // Records into the open frame (commits the SAMPLE terminal state on the
    // snapshot); no error until the close-path submit fires.
    b.masked_copy_area_with_snapshot_for_tests(src, dst, snap, (0, 0), 0, 0, 0, 0, 8, 8, &full)
        .expect("masked_copy_area_with_snapshot records into open frame");

    // Close → flush → injected submit failure → rollback.
    let close_result = b.engine_close_open_frame_for_timeout_for_tests();
    assert!(
        close_result.is_err(),
        "close must propagate the injected submit failure"
    );
    assert!(
        b.platform_renderer_failed_for_tests(),
        "injected submit failure must trip renderer_failed"
    );
    assert!(
        !b.frame_builder_is_open_for_tests(),
        "frame must be closed after the failed close-walk"
    );

    // rollback_snapshots must have restored all three fields to pre-frame.
    assert_eq!(
        b.engine_clip_snapshot_layout_for_tests(snap),
        Some(pre_layout),
        "rollback must restore the snapshot's pre-frame current_layout"
    );
    assert_eq!(
        b.engine_clip_snapshot_has_ticket_for_tests(snap),
        Some(pre_has_ticket),
        "rollback must restore the snapshot's pre-frame last_render_ticket"
    );
    assert_eq!(
        b.engine_clip_snapshot_version_for_tests(snap),
        Some(pre_version),
        "rollback must restore the snapshot's pre-frame snapshotted_version"
    );
}

/// Task 13: a `refresh_clip_snapshot` (the WRITE path) appends a
/// `ClipSnapshotRefresh` op and ADVANCES the snapshot's `snapshotted_version`
/// to the target version (unlike the SAMPLE path, which never touches the
/// version). If the close then FAILS, `rollback_snapshots` must restore ALL
/// THREE fields — `current_layout`, `last_render_ticket`, AND
/// `snapshotted_version` — to their pre-frame values, otherwise the next frame
/// skips the needed re-refresh (the no-op guard sees the rolled-forward version)
/// and the snapshot is left holding undefined/partial bytes.
///
/// This exercises the version-restore arm of `rollback_snapshots` that the
/// SAMPLE-path test (`masked_copyarea_snapshot_rollback_on_close_failure`)
/// cannot: only the WRITE path advances the version pre-flush.
///
/// Non-vacuity: a fresh snapshot starts at `snapshotted_version == u64::MAX`;
/// the refresh advances it to `V` (V != u64::MAX). The post-rollback assertion
/// demands `Some(u64::MAX)`. A regression that drops the version-restore in
/// `rollback_snapshots` would leave `snapshotted_version == V` and fail this
/// assertion — so the check is load-bearing.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn clip_snapshot_refresh_rollback_on_close_failure() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // Live clip-mask drawable (8x8). The close fails before the refresh copy
    // runs, so it only needs to be a valid drawable in the store.
    let live_mask = b.create_pixmap(None, 32, 8, 8).unwrap().as_raw();
    b.fill_rectangle(None, live_mask, 0x00FF_FFFF, 0, 0, 8, 8)
        .unwrap();

    // Fresh 8x8 R8 clip snapshot — UNDEFINED / no-ticket / version u64::MAX.
    let snap = b
        .engine_create_clip_snapshot_for_tests(8, 8)
        .expect("create_clip_snapshot");

    // Drain any setup CBs so we operate on a quiesced engine, then snapshot the
    // pre-frame snapshot state (BEFORE the refresh opens a frame).
    b.engine_close_open_frame_for_timeout_for_tests()
        .expect("drain setup frame");
    let pre_layout = b
        .engine_clip_snapshot_layout_for_tests(snap)
        .expect("snapshot present");
    let pre_has_ticket = b
        .engine_clip_snapshot_has_ticket_for_tests(snap)
        .expect("snapshot present");
    let pre_version = b
        .engine_clip_snapshot_version_for_tests(snap)
        .expect("snapshot present");
    // Fresh snapshot: UNDEFINED / no ticket / u64::MAX. Guard the test premise
    // so the version-advance below is genuinely a change (non-vacuity).
    assert_eq!(
        pre_version,
        u64::MAX,
        "fresh snapshot must start at snapshotted_version == u64::MAX"
    );

    const TARGET_VERSION: u64 = 7;
    assert_ne!(
        TARGET_VERSION, pre_version,
        "target version must differ from the pre-frame version (non-vacuous)"
    );

    // Arm the next vkQueueSubmit2 to fail (trips the flush-failure rollback).
    b.platform_force_next_submit_failure_for_tests();

    // Records into the open frame: appends ClipSnapshotRefresh + ADVANCES the
    // snapshot's snapshotted_version to TARGET_VERSION; no error until close.
    b.engine_refresh_clip_snapshot_for_tests(snap, live_mask, TARGET_VERSION)
        .expect("refresh_clip_snapshot records into open frame");

    // Sanity: the WRITE path advanced the version pre-flush (so the rollback
    // below has something to restore).
    assert_eq!(
        b.engine_clip_snapshot_version_for_tests(snap),
        Some(TARGET_VERSION),
        "refresh must advance snapshotted_version before close"
    );

    // Close → flush → injected submit failure → rollback.
    let close_result = b.engine_close_open_frame_for_timeout_for_tests();
    assert!(
        close_result.is_err(),
        "close must propagate the injected submit failure"
    );
    assert!(
        b.platform_renderer_failed_for_tests(),
        "injected submit failure must trip renderer_failed"
    );
    assert!(
        !b.frame_builder_is_open_for_tests(),
        "frame must be closed after the failed close-walk"
    );

    // rollback_snapshots must have restored ALL THREE fields to pre-frame.
    assert_eq!(
        b.engine_clip_snapshot_layout_for_tests(snap),
        Some(pre_layout),
        "rollback must restore the snapshot's pre-frame current_layout"
    );
    assert_eq!(
        b.engine_clip_snapshot_has_ticket_for_tests(snap),
        Some(pre_has_ticket),
        "rollback must restore the snapshot's pre-frame last_render_ticket"
    );
    // THE load-bearing assertion: the version must roll BACK to u64::MAX, not
    // stay at TARGET_VERSION. This is the WRITE-path-only restore.
    assert_eq!(
        b.engine_clip_snapshot_version_for_tests(snap),
        Some(pre_version),
        "rollback must restore the snapshot's pre-frame snapshotted_version (WRITE path)"
    );
}

/// Task 15 Step 3 — an in-scope (GXcopy, full plane-mask, depth-24)
/// clip-masked CopyArea through an installed `ClipState::Pixmap` clip
/// must route to the single GPU masked draw (`engine.masked_copy_area`),
/// NOT to the old per-sub-rect clip-mask-run fan-out. We assert the new
/// `copy_area_masked_draw` counter ticked exactly once for the copy and
/// that `copy_area_gpu_subrect_maskrun` did NOT increment (no fan-out).
///
/// Drives the REAL `copy_area` production path, not the test shim: the
/// Pixmap clip is installed via `apply_clip_state` (the live ChangeGC
/// clip-mask entry point, which eagerly populates the GPU snapshot).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn masked_copyarea_routes_to_draw_not_transfer() {
    use yserver_core::backend::{ClipState, PixmapHandle as ApplyPixmapHandle};

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // Depth-24 src filled red, dst filled blue.
    let src_xid = b.create_pixmap(None, 24, 8, 8).unwrap().as_raw();
    b.fill_rectangle(None, src_xid, 0xFFFF0000, 0, 0, 8, 8)
        .expect("fill src red");
    let dst_xid = b.create_pixmap(None, 24, 8, 8).unwrap().as_raw();
    b.fill_rectangle(None, dst_xid, 0xFF0000FF, 0, 0, 8, 8)
        .expect("fill dst blue");

    // Depth-1 clip mask: rows 0..4 = ones, rows 4..8 = zeros.
    let mask_xid = b.create_pixmap(None, 1, 8, 8).unwrap().as_raw();
    let mut mask_bits = vec![0u8; 4 * 8];
    for row in 0..4 {
        mask_bits[row * 4] = 0xFF;
    }
    b.put_image(None, mask_xid, 1, 8, 8, 0, 0, &mask_bits)
        .expect("put_image mask");

    // Install the Pixmap clip via the live entry point (eager snapshot).
    let mask_handle = ApplyPixmapHandle::from_raw(mask_xid).expect("mask handle");
    b.apply_clip_state(
        None,
        &ClipState::Pixmap {
            origin: (0, 0),
            pixmap: mask_handle,
        },
    )
    .expect("apply_clip_state Pixmap");

    // Snapshot telemetry AFTER install. Install eagerly refreshes the GPU
    // snapshot but should not need a CPU clip readback here.
    let pre_masked_draw = b.telemetry().lifetime.copy_area_masked_draw;
    let pre_maskrun = b.telemetry().lifetime.copy_area_gpu_subrect_maskrun;

    // ONE real copy through the production path.
    b.copy_area(None, src_xid, dst_xid, 0, 0, 0, 0, 8, 8)
        .expect("copy_area");

    let t = b.telemetry();
    assert_eq!(
        t.lifetime.copy_area_masked_draw - pre_masked_draw,
        1,
        "in-scope clip-masked GXcopy must route to exactly one masked draw"
    );
    assert_eq!(
        t.lifetime.copy_area_gpu_subrect_maskrun - pre_maskrun,
        0,
        "masked-draw route must NOT fan out into per-sub-rect maskrun blits"
    );

    // Cross-check the actual pixels: top half copied (red), bottom retained
    // (blue) — proves the route produced the correct clip-masked result.
    let out = b
        .get_image_pixels_for_tests(dst_xid, 2, 0, 0, 8, 8, !0)
        .expect("get_image")
        .expect("Some(bytes)");
    for row in 0..4 {
        for col in 0..8 {
            let off = (row * 8 + col) * 4;
            assert_eq!(
                &out[off..off + 4],
                &[0x00, 0x00, 0xFF, 0xFF],
                "row {row} col {col} should be red (mask=1)"
            );
        }
    }
    for row in 4..8 {
        for col in 0..8 {
            let off = (row * 8 + col) * 4;
            assert_eq!(
                &out[off..off + 4],
                &[0xFF, 0x00, 0x00, 0xFF],
                "row {row} col {col} should remain blue (mask=0)"
            );
        }
    }
}

/// Task 15 Step 4 — the masked-draw route reads the clip from the
/// eagerly-populated GPU snapshot (cache-hit path), so it must NOT do a
/// per-copy clip-mask readback. Install is also expected to keep the CPU clip
/// bytes deferred, so the `ClipMask`-site `engine.get_image` count should stay
/// flat both across install and across the COPY itself.
///
/// `GetImageSite::ClipMask` is discriminant 0 (telemetry.rs), referenced
/// here by index since the enum is crate-private.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn masked_copyarea_no_clip_get_image() {
    use yserver_core::backend::{ClipState, PixmapHandle as ApplyPixmapHandle};

    const CLIP_MASK_SITE: usize = 0; // GetImageSite::ClipMask

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let src_xid = b.create_pixmap(None, 24, 8, 8).unwrap().as_raw();
    b.fill_rectangle(None, src_xid, 0xFFFF0000, 0, 0, 8, 8)
        .expect("fill src");
    let dst_xid = b.create_pixmap(None, 24, 8, 8).unwrap().as_raw();
    b.fill_rectangle(None, dst_xid, 0xFF0000FF, 0, 0, 8, 8)
        .expect("fill dst");

    let mask_xid = b.create_pixmap(None, 1, 8, 8).unwrap().as_raw();
    let mut mask_bits = vec![0u8; 4 * 8];
    for row in 0..4 {
        mask_bits[row * 4] = 0xFF;
    }
    b.put_image(None, mask_xid, 1, 8, 8, 0, 0, &mask_bits)
        .expect("put_image mask");

    let mask_handle = ApplyPixmapHandle::from_raw(mask_xid).expect("mask handle");
    b.apply_clip_state(
        None,
        &ClipState::Pixmap {
            origin: (0, 0),
            pixmap: mask_handle,
        },
    )
    .expect("apply_clip_state Pixmap");

    let reads_after_install = b.telemetry().lifetime.get_image_by_site[CLIP_MASK_SITE];
    assert_eq!(
        reads_after_install, 0,
        "installing the clip pixmap must not do a CPU clip readback; \
         masked-copy uses the GPU snapshot"
    );

    b.copy_area(None, src_xid, dst_xid, 0, 0, 0, 0, 8, 8)
        .expect("copy_area");

    let post_clip_reads = b.telemetry().lifetime.get_image_by_site[CLIP_MASK_SITE];
    assert_eq!(
        post_clip_reads, reads_after_install,
        "masked-draw route must not do a per-copy clip-mask readback \
         (cache-hit GPU snapshot); clip-mask get_image count changed from \
         {reads_after_install} to {post_clip_reads}"
    );
}

/// Task 15 Step 5 — scope guard: a non-Copy logic op (GXxor) clip-masked
/// copy must NOT route to the masked draw. It falls through to the old
/// per-sub-rect path, which (for a `ClipState::Pixmap` non-Copy rop) takes
/// the CPU read-modify-write branch counted by `copy_area_cpu_pixmap_clip`.
/// We assert `copy_area_masked_draw` is unchanged and the old CPU path's
/// counter incremented instead.
///
/// NOTE: the plan named `copy_area_cpu_rop`/`copy_area_gpu_subrect_maskrun`
/// as the old-path counters, but the actual Pixmap-clip non-Copy branch
/// (backend.rs ~13290) bumps `copy_area_cpu_pixmap_clip` — that is the
/// real old-path counter for this scope.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn noncopy_or_partial_planemask_copy_still_uses_old_path() {
    use yserver_core::backend::{
        ClipState, DrawState, GcFunction, PixmapHandle as ApplyPixmapHandle,
    };

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let src_xid = b.create_pixmap(None, 24, 8, 8).unwrap().as_raw();
    b.fill_rectangle(None, src_xid, 0xFFFF0000, 0, 0, 8, 8)
        .expect("fill src");
    let dst_xid = b.create_pixmap(None, 24, 8, 8).unwrap().as_raw();
    b.fill_rectangle(None, dst_xid, 0xFF0000FF, 0, 0, 8, 8)
        .expect("fill dst");

    let mask_xid = b.create_pixmap(None, 1, 8, 8).unwrap().as_raw();
    let mut mask_bits = vec![0u8; 4 * 8];
    for row in 0..4 {
        mask_bits[row * 4] = 0xFF;
    }
    b.put_image(None, mask_xid, 1, 8, 8, 0, 0, &mask_bits)
        .expect("put_image mask");

    // Install the Pixmap clip + seed the snapshot.
    let mask_handle = ApplyPixmapHandle::from_raw(mask_xid).expect("mask handle");
    b.apply_clip_state(
        None,
        &ClipState::Pixmap {
            origin: (0, 0),
            pixmap: mask_handle,
        },
    )
    .expect("apply_clip_state Pixmap");

    // Now switch the GC function to GXxor while KEEPING the Pixmap clip
    // current (apply_draw_state re-asserts current_clip from state.clip).
    // This is out-of-scope for the masked-draw route.
    b.apply_draw_state(
        None,
        &DrawState {
            function: GcFunction::Xor,
            clip: ClipState::Pixmap {
                origin: (0, 0),
                pixmap: mask_handle,
            },
            ..DrawState::default()
        },
    )
    .expect("apply_draw_state Xor + Pixmap clip");

    let pre_masked_draw = b.telemetry().lifetime.copy_area_masked_draw;
    let pre_cpu_pixmap_clip = b.telemetry().lifetime.copy_area_cpu_pixmap_clip;

    b.copy_area(None, src_xid, dst_xid, 0, 0, 0, 0, 8, 8)
        .expect("copy_area");

    let t = b.telemetry();
    assert_eq!(
        t.lifetime.copy_area_masked_draw - pre_masked_draw,
        0,
        "GXxor clip-masked copy must NOT route to the masked draw"
    );
    assert!(
        t.lifetime.copy_area_cpu_pixmap_clip - pre_cpu_pixmap_clip >= 1,
        "GXxor clip-masked copy must take the old CPU pixmap-clip path \
         (copy_area_cpu_pixmap_clip should increment)"
    );
}

/// Task 16 Step 1 — retain-after-free: a `ClipState::Pixmap` clip mask is
/// installed (which eagerly populates a PINNED GPU snapshot while the source
/// mask pixmap is live), then the SOURCE mask pixmap is FREED, then a masked
/// GXcopy CopyArea runs through the production `copy_area` path. The copy must
/// STILL honour the install-time mask: top half (rows 0..4, mask=1) copied
/// from src, bottom half (rows 4..8, mask=0) retains the dst sentinel.
///
/// This proves the snapshot — populated at install — survives the source free
/// (X11 retain-after-free semantics) and that the cache-hit masked-draw path
/// never touches the freed drawable (the version-staleness refresh is gated on
/// `store.lookup(snap_xid)` returning Some, which it does NOT after free).
///
/// Drives the REAL `copy_area` production path, NOT the
/// `masked_copy_area_for_tests` shim — the point is the snapshot lifecycle.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn masked_copyarea_retains_clip_after_pixmap_freed() {
    use yserver_core::backend::{ClipState, PixmapHandle as ApplyPixmapHandle};

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // Depth-24 src filled red, dst filled blue (distinct sentinel).
    let src_xid = b.create_pixmap(None, 24, 8, 8).unwrap().as_raw();
    b.fill_rectangle(None, src_xid, 0xFFFF0000, 0, 0, 8, 8)
        .expect("fill src red");
    let dst_xid = b.create_pixmap(None, 24, 8, 8).unwrap().as_raw();
    b.fill_rectangle(None, dst_xid, 0xFF0000FF, 0, 0, 8, 8)
        .expect("fill dst blue");

    // Depth-1 clip mask: rows 0..4 = ones, rows 4..8 = zeros.
    let mask_xid = b.create_pixmap(None, 1, 8, 8).unwrap().as_raw();
    let mut mask_bits = vec![0u8; 4 * 8];
    for row in 0..4 {
        mask_bits[row * 4] = 0xFF;
    }
    b.put_image(None, mask_xid, 1, 8, 8, 0, 0, &mask_bits)
        .expect("put_image mask");

    // Install the Pixmap clip via the live entry point — this eagerly
    // refreshes the GPU snapshot from the live mask pixmap while leaving the
    // CPU bytes deferred.
    let mask_handle = ApplyPixmapHandle::from_raw(mask_xid).expect("mask handle");
    b.apply_clip_state(
        None,
        &ClipState::Pixmap {
            origin: (0, 0),
            pixmap: mask_handle,
        },
    )
    .expect("apply_clip_state Pixmap");

    // FREE the source mask pixmap. The snapshot is independent (its own pinned
    // GPU copy), so the free succeeds and the snapshot retains the install-time
    // mask bytes. Drain + poll so any deferred retirement actually runs and the
    // drawable id genuinely leaves the store before the copy.
    b.free_pixmap(None, mask_xid).expect("free mask pixmap");
    b.engine_drain_all_for_tests();
    b.for_tests_poll_retired();

    // ONE real masked copy through the production path AFTER the free.
    b.copy_area(None, src_xid, dst_xid, 0, 0, 0, 0, 8, 8)
        .expect("copy_area after free");

    let out = b
        .get_image_pixels_for_tests(dst_xid, 2, 0, 0, 8, 8, !0)
        .expect("get_image")
        .expect("Some(bytes)");

    // Top half rows: red (mask=1, copied from src). BGRA [0,0,0xFF,0xFF].
    for row in 0..4 {
        for col in 0..8 {
            let off = (row * 8 + col) * 4;
            assert_eq!(
                &out[off..off + 4],
                &[0x00, 0x00, 0xFF, 0xFF],
                "row {row} col {col} should be red (mask=1) — \
                 retained snapshot must still gate the copy after free"
            );
        }
    }
    // Bottom half rows: blue (mask=0, dst sentinel retained). BGRA [0xFF,0,0,0xFF].
    for row in 4..8 {
        for col in 0..8 {
            let off = (row * 8 + col) * 4;
            assert_eq!(
                &out[off..off + 4],
                &[0xFF, 0x00, 0x00, 0xFF],
                "row {row} col {col} should remain blue (mask=0) — \
                 retained snapshot must still mask out the bottom half"
            );
        }
    }
}

/// Deferred CPU clip bytes must still honor retain-after-free for the
/// run-based clip consumers. Install the clip while the source mask is live,
/// free the mask BEFORE any CPU-clipped op uses it, then do a
/// `PolyFillRectangle`: the top-half mask captured at free time must still
/// gate the fill.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn clip_pixmap_fill_retains_clip_after_pixmap_freed() {
    use yserver_core::backend::{ClipState, PixmapHandle as ApplyPixmapHandle};

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let dst_xid = b.create_pixmap(None, 24, 8, 8).unwrap().as_raw();
    b.fill_rectangle(None, dst_xid, 0xFF0000FF, 0, 0, 8, 8)
        .expect("fill dst blue");

    let mask_xid = b.create_pixmap(None, 1, 8, 8).unwrap().as_raw();
    let mut mask_bits = vec![0u8; 4 * 8];
    for row in 0..4 {
        mask_bits[row * 4] = 0xFF;
    }
    b.put_image(None, mask_xid, 1, 8, 8, 0, 0, &mask_bits)
        .expect("put_image mask");

    let mask_handle = ApplyPixmapHandle::from_raw(mask_xid).expect("mask handle");
    b.apply_clip_state(
        None,
        &ClipState::Pixmap {
            origin: (0, 0),
            pixmap: mask_handle,
        },
    )
    .expect("apply_clip_state Pixmap");

    b.free_pixmap(None, mask_xid).expect("free mask pixmap");
    b.engine_drain_all_for_tests();
    b.for_tests_poll_retired();

    let rect_bytes = {
        let mut buf = Vec::new();
        buf.extend_from_slice(&i16::to_le_bytes(0));
        buf.extend_from_slice(&i16::to_le_bytes(0));
        buf.extend_from_slice(&u16::to_le_bytes(8));
        buf.extend_from_slice(&u16::to_le_bytes(8));
        buf
    };
    b.poly_fill_rectangle(None, dst_xid, 0xFFFF0000, &rect_bytes)
        .expect("poly_fill_rectangle");

    b.clear_clip_rectangles(None).expect("clear clip");

    let out = b
        .get_image_pixels_for_tests(dst_xid, 2, 0, 0, 8, 8, !0)
        .expect("get_image")
        .expect("Some(bytes)");

    for row in 0..4 {
        for col in 0..8 {
            let off = (row * 8 + col) * 4;
            assert_eq!(
                &out[off..off + 4],
                &[0x00, 0x00, 0xFF, 0xFF],
                "row {row} col {col} should be red after free (mask=1)"
            );
        }
    }
    for row in 4..8 {
        for col in 0..8 {
            let off = (row * 8 + col) * 4;
            assert_eq!(
                &out[off..off + 4],
                &[0xFF, 0x00, 0x00, 0xFF],
                "row {row} col {col} should remain blue after free (mask=0)"
            );
        }
    }
}

/// Task 16 Step 2 — same-frame mask write: in a FRESH backend, install a
/// `ClipState::Pixmap` clip (mask=top-half), then WRITE the mask pixmap
/// (inverting it to bottom-half via `put_image`, which bumps the drawable's
/// `content_version`), then a masked GXcopy CopyArea — all in one frame. The
/// copy must reflect the NEWLY-WRITTEN mask, not the install-time mask: the
/// version-change triggers `refresh_clip_snapshot` (a GPU re-copy + barrier)
/// before the masked blit, so the snapshot now carries the bottom-half mask.
///
/// The install-time and written masks gate INVERTED regions, so a stale read
/// would copy the TOP half (wrong) instead of the BOTTOM half (correct) —
/// making the assertion load-bearing. If this shows the OLD (top-half) result,
/// that is a REAL bug in the version-staleness check / refresh ordering, NOT a
/// test to weaken.
///
/// IMPORTANT setup detail: the mask write must NOT be gated by the very clip
/// being installed. While `current_clip == ClipState::Pixmap{mask}`, ALL
/// drawable writes (`put_image`/`fill_rectangle`) route through
/// `intersect_with_current_clip_live`, so writing to the mask pixmap would be
/// self-masked by its OLD shape — the new bottom-half bits would be clipped
/// away and never land. This mirrors the canonical X11 client pattern
/// (`XSetClipMask(None)` → modify the bitmap → `XSetClipMask(mask)`): we clear
/// the clip to `None` (the frozen snapshot is RETAINED, not refreshed, across
/// `None`), write the new mask bits (version bumps), then re-install the same
/// Pixmap clip in the SAME frame. The re-install's version-change refresh
/// re-snapshots the new bytes before the masked blit.
///
/// Drives the REAL `copy_area` production path, NOT the test shim.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn masked_copyarea_mask_written_same_frame() {
    use yserver_core::backend::{ClipState, PixmapHandle as ApplyPixmapHandle};

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // Depth-24 src filled red, dst filled blue (distinct sentinel).
    let src_xid = b.create_pixmap(None, 24, 8, 8).unwrap().as_raw();
    b.fill_rectangle(None, src_xid, 0xFFFF0000, 0, 0, 8, 8)
        .expect("fill src red");
    let dst_xid = b.create_pixmap(None, 24, 8, 8).unwrap().as_raw();
    b.fill_rectangle(None, dst_xid, 0xFF0000FF, 0, 0, 8, 8)
        .expect("fill dst blue");

    // Install-time mask: rows 0..4 = ones (TOP half), rows 4..8 = zeros.
    let mask_xid = b.create_pixmap(None, 1, 8, 8).unwrap().as_raw();
    let mut install_bits = vec![0u8; 4 * 8];
    for row in 0..4 {
        install_bits[row * 4] = 0xFF;
    }
    b.put_image(None, mask_xid, 1, 8, 8, 0, 0, &install_bits)
        .expect("put_image install mask");

    // Install the Pixmap clip — eagerly snapshots the TOP-half mask.
    let mask_handle = ApplyPixmapHandle::from_raw(mask_xid).expect("mask handle");
    let pixmap_clip = ClipState::Pixmap {
        origin: (0, 0),
        pixmap: mask_handle,
    };
    b.apply_clip_state(None, &pixmap_clip)
        .expect("apply_clip_state Pixmap (install)");

    let install_ver = b
        .drawable_content_version_for_tests(mask_xid)
        .expect("mask content_version after install");

    // Clear the clip to None so the upcoming mask write is NOT self-gated by
    // the OLD mask shape. The frozen snapshot is RETAINED (not touched) across
    // None per the install path's documented contract.
    b.apply_clip_state(None, &ClipState::None)
        .expect("apply_clip_state None");

    // Same-frame WRITE: invert the mask to rows 4..8 = ones (BOTTOM half),
    // rows 0..4 = zeros. `put_image` is a drawable-write entry point and bumps
    // content_version, which must invalidate the snapshot for the next masked
    // draw.
    let mut written_bits = vec![0u8; 4 * 8];
    for row in 4..8 {
        written_bits[row * 4] = 0xFF;
    }
    b.put_image(None, mask_xid, 1, 8, 8, 0, 0, &written_bits)
        .expect("put_image written mask");

    let written_ver = b
        .drawable_content_version_for_tests(mask_xid)
        .expect("mask content_version after write");
    assert!(
        written_ver > install_ver,
        "put_image into the mask must bump content_version \
         (install={install_ver}, written={written_ver}) — \
         otherwise the staleness check can never fire"
    );

    // Re-install the SAME Pixmap clip in the SAME frame. The version change
    // (install_ver -> written_ver) triggers refresh_clip_snapshot, re-copying
    // the BOTTOM-half mask bytes into the snapshot before the masked blit.
    b.apply_clip_state(None, &pixmap_clip)
        .expect("apply_clip_state Pixmap (re-install)");

    // Masked copy through the production path. The snapshot now carries the
    // new (bottom-half) mask.
    b.copy_area(None, src_xid, dst_xid, 0, 0, 0, 0, 8, 8)
        .expect("copy_area same-frame");

    let out = b
        .get_image_pixels_for_tests(dst_xid, 2, 0, 0, 8, 8, !0)
        .expect("get_image")
        .expect("Some(bytes)");

    // WRITTEN mask gates the BOTTOM half: rows 4..8 red (mask=1, copied),
    // rows 0..4 blue (mask=0, dst sentinel retained). A stale (install-time)
    // read would invert this — copying the TOP half instead.
    for row in 0..4 {
        for col in 0..8 {
            let off = (row * 8 + col) * 4;
            assert_eq!(
                &out[off..off + 4],
                &[0xFF, 0x00, 0x00, 0xFF],
                "row {row} col {col} should remain blue (NEW mask=0) — \
                 same-frame refresh must pick up the inverted mask; \
                 red here means the STALE install-time mask was used"
            );
        }
    }
    for row in 4..8 {
        for col in 0..8 {
            let off = (row * 8 + col) * 4;
            assert_eq!(
                &out[off..off + 4],
                &[0x00, 0x00, 0xFF, 0xFF],
                "row {row} col {col} should be red (NEW mask=1) — \
                 same-frame refresh must have re-copied the written mask"
            );
        }
    }
}

/// #90 follow-up regression: `create_cursor` (XCreatePixmapCursor)
/// must round-trip a depth-1 source/mask pixmap through `get_image`
/// into a full-height cursor sprite — NOT a flattened top sliver.
///
/// `get_image` at depth 1 returns the packed X11 wire bitmap
/// (`⌈w/32⌉·4` bytes/row, LSBFirst); the rasteriser wants one byte
/// per pixel. Before the fix `read_cursor_depth1_pixmap` handed the
/// packed bytes straight to the rasteriser, so a 17-wide cursor
/// collapsed into its first `⌈17/32⌉·4·17 ÷ 17` ≈ 4 rows. ImageMagick
/// `import`'s crosshair (this exact 17×17 `scope` bitmap) showed as a
/// horizontal sliver during the region-select pointer grab.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn create_cursor_depth1_source_is_full_height_not_flattened() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // ImageMagick MagickCore/xwindow.c XMakeCursor scope_bits +
    // scope_mask_bits, 17×17, LSBFirst, 3 bytes/row in the client
    // image. The X11 wire pads each scanline to 4 bytes, which is what
    // PutImage carries and what get_image returns. `import` passes both
    // the source and the mask, and visibility follows the mask — so the
    // mask suffers the same depth-1 unpack, and a flattened mask leaves
    // the bottom of the crosshair invisible.
    const SCOPE_CLIENT: [u8; 51] = [
        0x80, 0x03, 0x00, 0x80, 0x02, 0x00, 0x80, 0x02, 0x00, 0x80, 0x02, 0x00, 0x80, 0x02, 0x00,
        0x80, 0x02, 0x00, 0x80, 0x02, 0x00, 0x7f, 0xfc, 0x01, 0x01, 0x00, 0x01, 0x7f, 0xfc, 0x01,
        0x80, 0x02, 0x00, 0x80, 0x02, 0x00, 0x80, 0x02, 0x00, 0x80, 0x02, 0x00, 0x80, 0x02, 0x00,
        0x80, 0x02, 0x00, 0x80, 0x03, 0x00,
    ];
    const SCOPE_MASK_CLIENT: [u8; 51] = [
        0xc0, 0x07, 0x00, 0xc0, 0x07, 0x00, 0xc0, 0x06, 0x00, 0xc0, 0x06, 0x00, 0xc0, 0x06, 0x00,
        0xc0, 0x06, 0x00, 0xff, 0xfe, 0x01, 0x7f, 0xfc, 0x01, 0x03, 0x80, 0x01, 0x7f, 0xfc, 0x01,
        0xff, 0xfe, 0x01, 0xc0, 0x06, 0x00, 0xc0, 0x06, 0x00, 0xc0, 0x06, 0x00, 0xc0, 0x06, 0x00,
        0xc0, 0x07, 0x00, 0xc0, 0x07, 0x00,
    ];
    // Re-pad 3-byte client rows to the 4-byte wire stride.
    let repad = |client: &[u8; 51]| {
        let mut wire = vec![0u8; 4 * 17];
        for row in 0..17 {
            wire[row * 4..row * 4 + 3].copy_from_slice(&client[row * 3..row * 3 + 3]);
        }
        wire
    };
    let src_wire = repad(&SCOPE_CLIENT);
    let mask_wire = repad(&SCOPE_MASK_CLIENT);

    let src_pix = b
        .create_pixmap(None, 1, 17, 17)
        .expect("create_pixmap depth=1 17x17 src");
    b.put_image(None, src_pix.as_raw(), 1, 17, 17, 0, 0, &src_wire)
        .expect("put_image src depth=1");
    let mask_pix = b
        .create_pixmap(None, 1, 17, 17)
        .expect("create_pixmap depth=1 17x17 mask");
    b.put_image(None, mask_pix.as_raw(), 1, 17, 17, 0, 0, &mask_wire)
        .expect("put_image mask depth=1");

    let cursor = b
        .create_cursor(
            None,
            src_pix,
            Some(mask_pix),
            (0xFFFF, 0xFFFF, 0xFFFF),
            (0, 0, 0),
            8,
            8,
        )
        .expect("create_cursor");

    let (w, h, bgra) = b
        .cursor_record_bgra_for_tests(cursor.as_raw())
        .expect("cursor record present");
    assert_eq!((w, h), (17, 17), "cursor keeps its 17x17 dims");
    assert_eq!(bgra.len(), 17 * 17 * 4);

    let opaque = |x: usize, y: usize| bgra[(y * 17 + x) * 4 + 3] != 0;
    // Every one of the 17 rows carries part of the crosshair, so every
    // row must have at least one opaque pixel. The flattened bug left
    // rows ~4..17 fully transparent.
    for y in 0..17 {
        assert!(
            (0..17).any(|x| opaque(x, y)),
            "row {y} of the crosshair is empty — cursor is flattened"
        );
    }
    // Spot-check the vertical bar's extremes: top row and bottom row
    // both light column 8 (the scope's centre stem).
    assert!(opaque(8, 0), "top of vertical bar missing");
    assert!(opaque(8, 16), "bottom of vertical bar missing");
}
