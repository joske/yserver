use super::*;

#[test]
fn decode_pixel_bgra_round_trip() {
    // 0xAARRGGBB → r,g,b,a in 0..1
    let rgba = decode_x11_pixel_bgra(0xFF_80_40_20);
    assert!((rgba[0] - 128.0 / 255.0).abs() < 1e-3); // R = 0x80
    assert!((rgba[1] - 64.0 / 255.0).abs() < 1e-3); // G = 0x40
    assert!((rgba[2] - 32.0 / 255.0).abs() < 1e-3); // B = 0x20
    assert!((rgba[3] - 255.0 / 255.0).abs() < 1e-3); // A = 0xFF
}

#[test]
fn x11_row_stride_pad_to_32_bits() {
    // depth-1, width 9 → 9 bits → ceil(9/32)*4 = 4 bytes.
    assert_eq!(x11_src_row_stride(1, 9), 4);
    // depth-1, width 33 → ceil(33/32)*4 = 8.
    assert_eq!(x11_src_row_stride(1, 33), 8);
    // depth-4 is nibble-packed and padded to 32 bits.
    assert_eq!(x11_src_row_stride(4, 3), 4);
    assert_eq!(x11_src_row_stride(4, 9), 8);
    // depth-8, width 3 → 24 bits padded to 32 → 4 bytes.
    assert_eq!(x11_src_row_stride(8, 3), 4);
    // depth-8, width 5 → 40 bits padded to 64 → 8 bytes.
    assert_eq!(x11_src_row_stride(8, 5), 8);
    // depth-32, width 10 → 320 bits = 40 bytes (already aligned).
    assert_eq!(x11_src_row_stride(32, 10), 40);
}

// ───── #133 step 3 (P4) — the bounds-aware clamp/scissor layer ─────
//
// Every op's clip site moved from "the storage extent" to "the
// destination handle's content bounds". These four tests pin the two
// properties the whole step rests on: at a full-extent bounds the
// new helpers are the OLD helpers (so `bw == 0` is a pure refactor),
// and at a narrower bounds they confine the op to the content.

fn brd_extent(w: u32, h: u32) -> vk::Extent2D {
    vk::Extent2D {
        width: w,
        height: h,
    }
}

fn brd_rect(x: i32, y: i32, w: u32, h: u32) -> vk::Rect2D {
    vk::Rect2D {
        offset: vk::Offset2D { x, y },
        extent: brd_extent(w, h),
    }
}

#[test]
fn clamp_rect_to_full_extent_equals_clamp_rect() {
    let ext = brd_extent(24, 16);
    for r in [
        brd_rect(-8, -8, 64, 64),
        brd_rect(0, 0, 24, 16),
        brd_rect(20, 12, 8, 8),
        brd_rect(30, 30, 4, 4),
    ] {
        assert_eq!(
            clamp_rect_to(r, brd_rect(0, 0, ext.width, ext.height)),
            clamp_rect(r, ext),
            "full-extent bounds must reproduce clamp_rect for {r:?}",
        );
    }
    // Narrower bounds (content at (4, 4) inside 24x16 storage).
    assert_eq!(
        clamp_rect_to(brd_rect(-8, -8, 64, 64), brd_rect(4, 4, 16, 8)),
        brd_rect(4, 4, 16, 8),
    );
}

#[test]
fn clamp_put_rect_to_crops_the_source_at_the_content_origin() {
    let src = brd_extent(24, 16);
    // Full-extent bounds == the legacy helper.
    assert_eq!(
        clamp_put_rect_to(vk::Offset2D { x: -4, y: -4 }, src, brd_rect(0, 0, 24, 16)),
        clamp_put_rect(vk::Offset2D { x: -4, y: -4 }, src, brd_extent(24, 16)),
    );
    // Content bounds (4, 4, 16, 8): a PutImage at content-local
    // (-4, -4) — storage (0, 0) — is cropped by 4 rows/columns and
    // lands at the content origin, not in the ring.
    let (rect, (sx, sy)) =
        clamp_put_rect_to(vk::Offset2D { x: 0, y: 0 }, src, brd_rect(4, 4, 16, 8))
            .expect("visible");
    assert_eq!(rect, brd_rect(4, 4, 16, 8));
    assert_eq!((sx, sy), (4, 4), "leading source rows/cols cropped");
}

#[test]
fn clamp_copy_rects_to_clips_both_sides_and_keeps_them_aligned() {
    let ext = brd_extent(24, 16);
    // Full-extent bounds on both sides == the legacy helper.
    assert_eq!(
        clamp_copy_rects_to(
            brd_rect(-2, 0, 8, 8),
            vk::Offset2D { x: 0, y: 0 },
            brd_rect(0, 0, 24, 16),
            brd_rect(0, 0, 24, 16),
        ),
        clamp_copy_rects(brd_rect(-2, 0, 8, 8), vk::Offset2D { x: 0, y: 0 }, ext, ext),
    );
    // SOURCE bounds = content (4, 4, 16, 8): a read starting in the
    // ring advances BOTH origins, so no ring pixel is copied and the
    // destination stays aligned with the source.
    let (s, d) = clamp_copy_rects_to(
        brd_rect(0, 0, 24, 16),
        vk::Offset2D { x: 0, y: 0 },
        brd_rect(4, 4, 16, 8),
        brd_rect(0, 0, 24, 16),
    )
    .expect("visible");
    assert_eq!(s, brd_rect(4, 4, 16, 8));
    assert_eq!(d, brd_rect(4, 4, 16, 8));
}

#[test]
fn build_render_clip_scissors_to_bounds_the_no_clip_case() {
    let ext = brd_extent(24, 16);
    // No picture clip + full-extent bounds == the legacy helper.
    assert_eq!(
        build_render_clip_scissors_to(None, brd_rect(0, 0, 24, 16)),
        build_render_clip_scissors(None, ext),
    );
    // No picture clip + content bounds → the content rect itself.
    assert_eq!(
        build_render_clip_scissors_to(None, brd_rect(4, 4, 16, 8)),
        vec![brd_rect(4, 4, 16, 8)],
    );
    // A client clip that reaches into the ring is trimmed to it.
    let cr = [Rectangle16 {
        x: -8,
        y: -8,
        width: 64,
        height: 64,
    }];
    assert_eq!(
        build_render_clip_scissors_to(Some(&cr), brd_rect(4, 4, 16, 8)),
        vec![brd_rect(4, 4, 16, 8)],
    );
}

#[test]
fn clamp_put_rect_inside_returns_unchanged() {
    let r = clamp_put_rect(
        vk::Offset2D { x: 2, y: 3 },
        vk::Extent2D {
            width: 4,
            height: 5,
        },
        vk::Extent2D {
            width: 16,
            height: 16,
        },
    )
    .unwrap();
    assert_eq!(r.0.offset, vk::Offset2D { x: 2, y: 3 });
    assert_eq!(
        r.0.extent,
        vk::Extent2D {
            width: 4,
            height: 5,
        },
    );
    assert_eq!(r.1, (0, 0));
}

#[test]
fn clamp_put_rect_partial_clip_records_source_offset() {
    // dst_pos = (-1, -2), src 4×5 against a 16×16 storage →
    // dst rect (0,0,3,3) with source-input origin (1, 2).
    let r = clamp_put_rect(
        vk::Offset2D { x: -1, y: -2 },
        vk::Extent2D {
            width: 4,
            height: 5,
        },
        vk::Extent2D {
            width: 16,
            height: 16,
        },
    )
    .unwrap();
    assert_eq!(r.0.offset, vk::Offset2D { x: 0, y: 0 });
    assert_eq!(
        r.0.extent,
        vk::Extent2D {
            width: 3,
            height: 3,
        },
    );
    assert_eq!(r.1, (1, 2));
}

#[test]
fn clamp_put_rect_outside_returns_none() {
    let r = clamp_put_rect(
        vk::Offset2D { x: 100, y: 100 },
        vk::Extent2D {
            width: 4,
            height: 4,
        },
        vk::Extent2D {
            width: 16,
            height: 16,
        },
    );
    assert!(r.is_none());
}

#[test]
fn depth1_unpack_round_trip() {
    // 1×8 source padded to a 32-bit scanline (4 bytes). Bit
    // order LSB-first per the server's advertised
    // `bitmap-bit-order`: 0xAA = 1010_1010 = bits 1, 3, 5, 7
    // set → pixels 1, 3, 5, 7 set. Remaining 3 bytes are
    // scanline pad.
    let src = vec![0xAAu8, 0x00, 0x00, 0x00];
    let src_extent = vk::Extent2D {
        width: 8,
        height: 1,
    };
    let mut out = vec![0u8; 8];
    unpack_to_staging(&src, src_extent, 0, 0, 8, 1, 1, out.as_mut_ptr()).unwrap();
    assert_eq!(out, vec![0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF]);

    let packed = pack_from_storage(&out, 8, 1, 1).unwrap();
    // Row stride is 4 bytes (32 bits) per depth-1 pad rule;
    // the first byte holds the data, repacked LSB-first →
    // 0xAA round-trips (the byte is self-symmetric under
    // pack/unpack inversion).
    assert_eq!(packed.len(), 4);
    assert_eq!(packed[0], 0xAA);
}

#[test]
fn depth32_unpack_is_memcpy() {
    // 2×2 BGRA8 source.
    let src: Vec<u8> = vec![
        0x10, 0x20, 0x30, 0xFF, 0x11, 0x21, 0x31, 0xFF, // row 0
        0x12, 0x22, 0x32, 0xFF, 0x13, 0x23, 0x33, 0xFF, // row 1
    ];
    let src_extent = vk::Extent2D {
        width: 2,
        height: 2,
    };
    let mut out = vec![0u8; 16];
    unpack_to_staging(&src, src_extent, 0, 0, 2, 2, 32, out.as_mut_ptr()).unwrap();
    assert_eq!(out, src);
}

#[test]
fn depth4_unpack_and_pack_follow_nibble_layout() {
    let src = vec![0x21u8, 0x00, 0x00, 0x00];
    let src_extent = vk::Extent2D {
        width: 2,
        height: 1,
    };
    let mut out = vec![0u8; 2];
    unpack_to_staging(&src, src_extent, 0, 0, 2, 1, 4, out.as_mut_ptr()).unwrap();
    assert_eq!(out, vec![0x01, 0x02]);

    let packed = pack_from_storage(&out, 2, 1, 4).unwrap();
    assert_eq!(packed, vec![0x21, 0x00, 0x00, 0x00]);
}
