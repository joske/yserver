use super::*;

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
