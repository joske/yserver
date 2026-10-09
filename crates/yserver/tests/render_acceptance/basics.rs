use super::*;

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

/// `PutImage` must honor a rectangle clip, including when the source upload
/// covers the whole destination. Firefox uses this pattern for popup menu
/// hover updates: the untouched rows in the reused MIT-SHM buffer are stale.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn put_image_honors_rectangle_clip() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let dst = b.create_pixmap(None, 32, 4, 4).expect("create pixmap");
    let xid = dst.as_raw();
    b.fill_rectangle(None, xid, 0xFF00_00FF, 0, 0, 4, 4)
        .expect("prefill blue");
    b.apply_clip_state(
        None,
        &ClipState::Rectangles {
            origin: (0, 0),
            rects: ClipRectangles {
                ordering: 0,
                x_origin: 0,
                y_origin: 0,
                rectangles: [
                    0i16.to_le_bytes(),
                    1i16.to_le_bytes(),
                    4u16.to_le_bytes(),
                    1u16.to_le_bytes(),
                ]
                .concat(),
            },
        },
    )
    .expect("install rectangle clip");

    let red = [0x00, 0x00, 0xFF, 0xFF];
    let upload: Vec<u8> = (0..16).flat_map(|_| red).collect();
    b.put_image(None, xid, 32, 4, 4, 0, 0, &upload)
        .expect("put image");

    let pixels = b
        .get_image_pixels_for_tests(xid, 2, 0, 0, 4, 4, !0)
        .expect("get image")
        .expect("pixels");
    for row in 0..4 {
        for col in 0..4 {
            let off = (row * 4 + col) * 4;
            let expected = if row == 1 {
                red
            } else {
                [0xFF, 0x00, 0x00, 0xFF]
            };
            assert_eq!(
                &pixels[off..off + 4],
                expected,
                "row {row} col {col} must respect the rectangle clip"
            );
        }
    }
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
