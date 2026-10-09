use super::*;

/// X11 ZPixmap source row stride for a given depth + width. Per
/// the wire format: scanline padded to 32 bits.
pub(super) fn x11_src_row_stride(depth: u8, width: u32) -> usize {
    let bits_per_row = match depth {
        1 => width,
        4 => u32::from(4u8) * width,
        8 => u32::from(8u8) * width,
        24 | 32 => 32 * width,
        _ => 32 * width,
    };
    // Pad up to 32 bits (4 bytes).
    let bits_padded = bits_per_row.div_ceil(32) * 32;
    (bits_padded / 8) as usize
}

/// Copy a sub-rect of `src` (ZPixmap wire, padded rows) into
/// `dst_ptr` (tightly packed bytes matching the storage format).
///
/// # Safety
///
/// `dst_ptr` must be valid for `dst_w * dst_h * dst_bpp` bytes.
///
/// # Errors
///
/// `TruncatedSource` if `src` is shorter than the row stride ×
/// (sy + dst_h) the depth implies.
pub(super) fn unpack_to_staging(
    src: &[u8],
    src_extent: vk::Extent2D,
    sx: u32,
    sy: u32,
    dst_w: u32,
    dst_h: u32,
    src_depth: u8,
    dst_ptr: *mut u8,
) -> Result<(), RenderError> {
    let src_row_bytes = x11_src_row_stride(src_depth, src_extent.width);
    let expected_len =
        src_row_bytes
            .checked_mul((sy + dst_h) as usize)
            .ok_or(RenderError::TruncatedSource {
                expected: usize::MAX,
            })?;
    if src.len() < expected_len {
        return Err(RenderError::TruncatedSource {
            expected: expected_len,
        });
    }
    match src_depth {
        32 | 24 => {
            // BGRA8 wire → BGRA8 staging. For depth-24, force
            // alpha to 0xFF so subsequent sample-as-source has a
            // defined alpha channel.
            let row_dst_bytes = (dst_w * 4) as usize;
            for row in 0..dst_h {
                let src_row_off = (sy + row) as usize * src_row_bytes;
                let src_col_off = sx as usize * 4;
                let src_slice =
                    &src[src_row_off + src_col_off..src_row_off + src_col_off + row_dst_bytes];
                // SAFETY: caller guarantees dst_ptr is valid for
                // dst_w*dst_h*4 bytes; row * row_dst_bytes within.
                unsafe {
                    let dst = dst_ptr.add(row as usize * row_dst_bytes);
                    std::ptr::copy_nonoverlapping(src_slice.as_ptr(), dst, row_dst_bytes);
                    if src_depth == 24 {
                        // Stomp alpha to 0xFF every 4th byte.
                        for col in 0..dst_w as usize {
                            *dst.add(col * 4 + 3) = 0xFF;
                        }
                    }
                }
            }
        }
        8 => {
            let row_dst_bytes = dst_w as usize;
            for row in 0..dst_h {
                let src_row_off = (sy + row) as usize * src_row_bytes;
                let src_col_off = sx as usize;
                let src_slice =
                    &src[src_row_off + src_col_off..src_row_off + src_col_off + row_dst_bytes];
                unsafe {
                    let dst = dst_ptr.add(row as usize * row_dst_bytes);
                    std::ptr::copy_nonoverlapping(src_slice.as_ptr(), dst, row_dst_bytes);
                }
            }
        }
        4 => {
            let row_dst_bytes = dst_w as usize;
            for row in 0..dst_h {
                let src_row_off = (sy + row) as usize * src_row_bytes;
                let row_src = &src[src_row_off..src_row_off + src_row_bytes];
                unsafe {
                    let dst = dst_ptr.add(row as usize * row_dst_bytes);
                    for col in 0..dst_w as usize {
                        let byte = row_src[col / 2];
                        let nibble = if col % 2 == 0 {
                            byte & 0x0f
                        } else {
                            (byte >> 4) & 0x0f
                        };
                        *dst.add(col) = nibble;
                    }
                }
            }
        }
        1 => {
            // 1 bit per pixel → 1 byte per pixel (0xFF if set,
            // 0x00 if clear). Unpack each requested column from
            // the source bit position. Bit order matches the
            // server's advertised `bitmap-bit-order` — we forward
            // the client's `byte_order` from setup (typically
            // `LSBFirst` on x86), so bit 0 of a byte is pixel 0
            // in that 8-pixel group. Mirrors v1's depth-1 PutImage
            // unpacker at `kms::backend.rs:3995`.
            let row_dst_bytes = dst_w as usize;
            for row in 0..dst_h {
                let src_row_off = (sy + row) as usize * src_row_bytes;
                let row_src = &src[src_row_off..src_row_off + src_row_bytes];
                unsafe {
                    let dst = dst_ptr.add(row as usize * row_dst_bytes);
                    for col in 0..dst_w as usize {
                        let bit_index = sx as usize + col;
                        let byte = row_src[bit_index / 8];
                        let bit = (byte >> (bit_index % 8)) & 0x1;
                        *dst.add(col) = if bit != 0 { 0xFF } else { 0x00 };
                    }
                }
            }
        }
        _ => return Err(RenderError::UnsupportedDepth(src_depth)),
    }
    Ok(())
}

/// Convert tightly-packed storage bytes (from a GetImage
/// readback) into the wire format clients expect. Inverse of
/// [`unpack_to_staging`].
///
/// # Errors
///
/// `UnsupportedDepth` for depths other than 1/8/24/32.
pub(super) fn pack_from_storage(
    raw: &[u8],
    w: u32,
    h: u32,
    depth: u8,
) -> Result<Vec<u8>, RenderError> {
    match depth {
        32 | 24 => {
            // Storage is BGRA8 tightly packed; wire ZPixmap is
            // also BGRA8 tightly packed for our advertised
            // visual (no scanline pad at depth-32 because
            // 32 bits already aligns). Round-trip is a memcpy.
            // depth-24 carries the alpha byte through (clients
            // ignore the X-byte position).
            Ok(raw.to_vec())
        }
        8 => {
            // Scanline padded to 32 bits.
            let row_dst_bytes = (w as usize + 3) & !3;
            let mut out = vec![0u8; row_dst_bytes * h as usize];
            for row in 0..h as usize {
                let src_off = row * w as usize;
                let dst_off = row * row_dst_bytes;
                out[dst_off..dst_off + w as usize]
                    .copy_from_slice(&raw[src_off..src_off + w as usize]);
            }
            Ok(out)
        }
        4 => {
            // Two pixels per byte, low nibble first, rows padded to 32 bits.
            let row_dst_bytes = w.div_ceil(8) as usize * 4;
            let mut out = vec![0u8; row_dst_bytes * h as usize];
            for row in 0..h as usize {
                let src_off = row * w as usize;
                let dst_off = row * row_dst_bytes;
                for col in 0..w as usize {
                    let nibble = raw[src_off + col] & 0x0f;
                    let dst = &mut out[dst_off + col / 2];
                    if col % 2 == 0 {
                        *dst = (*dst & 0xf0) | nibble;
                    } else {
                        *dst = (*dst & 0x0f) | (nibble << 4);
                    }
                }
            }
            Ok(out)
        }
        1 => {
            // Pack 0xFF/0x00 bytes back to 1bpp; scanline
            // padded to 32 bits. Bit order matches the server's
            // advertised `bitmap-bit-order` (LSBFirst when the
            // client requested it, which is the x86 default); bit
            // 0 of a byte is pixel 0 in that 8-pixel group.
            // Mirrors `unpack_to_staging`'s depth-1 branch above.
            let row_bytes = w.div_ceil(32) as usize * 4;
            let mut out = vec![0u8; row_bytes * h as usize];
            for row in 0..h as usize {
                let src_off = row * w as usize;
                let dst_off = row * row_bytes;
                for col in 0..w as usize {
                    if raw[src_off + col] != 0 {
                        out[dst_off + col / 8] |= 1 << (col % 8);
                    }
                }
            }
            Ok(out)
        }
        _ => Err(RenderError::UnsupportedDepth(depth)),
    }
}

/// Decode an X11 32-bit pixel (B in low byte, then G, R, A) into
/// an RGBA float-4 suitable for `vkCmdClearAttachments` against a
/// `B8G8R8A8_UNORM` target.
#[must_use]
pub(crate) fn decode_x11_pixel_bgra(pixel: u32) -> [f32; 4] {
    let b = (pixel & 0xff) as f32 / 255.0;
    let g = ((pixel >> 8) & 0xff) as f32 / 255.0;
    let r = ((pixel >> 16) & 0xff) as f32 / 255.0;
    let a = ((pixel >> 24) & 0xff) as f32 / 255.0;
    // `vkCmdClearAttachments` clearColor.float32 against a
    // BGRA8_UNORM attachment writes [R, G, B, A] components per
    // spec — the format swizzle handles the BGRA→RGBA mapping at
    // store time. So we pass logical RGBA here.
    [r, g, b, a]
}

/// L1 server-α invariant: depth-24 / depth-8 / depth-1 destinations
/// are server-owned-α, so the stored alpha byte must read back as
/// `0xFF` regardless of what the X11 pixel's upper byte happens to
/// contain (typically `0x00` for `0x00RRGGBB` colour literals). The
/// scene compositor binds `storage.image_view` (IDENTITY swizzle —
/// required because the same view doubles as a colour attachment per
/// VUID-VkFramebufferCreateInfo-pAttachments-00891) and runs window
/// draws in `alpha_passthrough=true` mode, so a paint that leaves
/// α=0 in storage renders as a fully-transparent window — the layer
/// underneath leaks through. v1 forces this at every fill site
/// (`kms/backend.rs:try_vk_solid_fill`); this helper is v2's
/// equivalent.
#[must_use]
pub(crate) fn decode_x11_pixel_server_alpha(pixel: u32, depth: u8) -> [f32; 4] {
    let mut c = decode_x11_pixel_bgra(pixel);
    if depth != 32 {
        c[3] = 1.0;
    }
    c
}

/// #137 tier 1 — one wire pixel from [`RenderEngine::get_image`] as the
/// premultiplied `[R, G, B, A]` that `ResolvedSource::Solid` carries.
///
/// The whole point of this conversion is that the result must equal what
/// the engine's own sampler would have produced for that pixel, or the
/// collapse is not exact. So it reproduces, on the CPU, exactly the two
/// decisions the sampling path makes for a drawable source:
///
/// - the **swizzle** (`swizzle_class_for_pict_format`): an `R8_UNORM`
///   storage — depth 8 or depth 1 — is an alpha mask, sampled
///   `(0, 0, 0, R)`; a BGRA storage is sampled as it lies.
/// - **force-opaque** (`resolve_force_opaque_pict_format` and the
///   `BgraNoAlpha` swizzle, which agree): a depth-24 storage, or a
///   picture declaring `xRGB24` / `xRGB32`, has no client-meaningful
///   alpha byte and reads as `a = 1.0`.
///
/// Both of the conversions here fail *silently* when wrong, which is
/// why they are pinned by a test rather than reasoned about:
///
/// - `get_image` returns wire order — `[B, G, R, A]`, blue in the low
///   byte — while `ResolvedSource::Solid` is logical `[R, G, B, A]`. A
///   swap turns blue text red and nothing errors.
/// - taking the stored byte as alpha on a depth-24 source would give
///   `a = 0.0` and paint nothing at all: the exact symptom of the bug
///   being fixed, which would be maximally confusing to debug.
///
/// No premultiplication is applied. X RENDER storage is premultiplied
/// already (as is the `CreateSolidFill` wire colour, which v2 stores
/// as-is), so the channels pass through untouched.
///
/// `None` for a short buffer or a depth v2 has no RENDER format for.
#[must_use]
pub(crate) fn premul_from_wire_pixel(wire: &[u8], depth: u8, pict_format: u32) -> Option<[f32; 4]> {
    use yserver_protocol::x11::{RENDER_FMT_RGB24, RENDER_FMT_XRGB32};
    // Divide rather than multiply by a reciprocal: `decode_x11_pixel_bgra`
    // and every other channel decode in this file divide, and the two do
    // not agree to the last bit.
    match depth {
        24 | 32 => {
            let px = wire.get(..4)?;
            let (b, g, r, a) = (px[0], px[1], px[2], px[3]);
            let opaque =
                depth == 24 || pict_format == RENDER_FMT_RGB24 || pict_format == RENDER_FMT_XRGB32;
            Some([
                f32::from(r) / 255.0,
                f32::from(g) / 255.0,
                f32::from(b) / 255.0,
                if opaque { 1.0 } else { f32::from(a) / 255.0 },
            ])
        }
        // A8: alpha only. Premultiplied, the colour channels of a
        // pure-alpha picture are zero — which is what the
        // `AlphaOnlyR8` swizzle's `(0, 0, 0, R)` samples.
        8 => Some([0.0, 0.0, 0.0, f32::from(*wire.first()?) / 255.0]),
        // A1: `pack_from_storage` returns one bit per pixel, LSB
        // first, so pixel 0 is bit 0 of byte 0.
        1 => Some([0.0, 0.0, 0.0, f32::from(*wire.first()? & 1)]),
        _ => None,
    }
}

/// Decode an X11 pixel for direct storage writes.
///
/// `R8_UNORM` targets are alpha-mask style storages, so the byte must
/// land in the attachment's first component, not the BGRA low byte
/// interpretation used by `decode_x11_pixel_server_alpha`.
#[must_use]
pub(crate) fn decode_x11_pixel_for_storage(pixel: u32, depth: u8, format: vk::Format) -> [f32; 4] {
    if format == vk::Format::R8_UNORM {
        [
            (pixel & 0xff) as f32 / 255.0,
            0.0,
            0.0,
            if depth == 32 {
                ((pixel >> 24) & 0xff) as f32 / 255.0
            } else {
                1.0
            },
        ]
    } else {
        decode_x11_pixel_server_alpha(pixel, depth)
    }
}
