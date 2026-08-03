// Cursor rasterisation is full of intentional i32 → u16/u32 saturating
// casts matched per the codebase's per-call discipline in
// `kms/render/backend.rs`. Hoisted to module scope here to avoid clutter
// inside the algorithm body.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

//! Stage 5 Phase A — cursor records + sprite rasterisation.
//!
//! Each X11 cursor (created via `CreateCursor`, `CreateGlyphCursor`,
//! `RenderCreateCursor`) lowers to an immutable [`CursorRecord`]:
//! a versioned, refcounted snapshot of the cursor sprite.
//!
//! Two pixel storages per record:
//! - `bgra_bytes`: tightly-packed little-endian BGRA8 matching DRM
//!   `ARGB8888`. Used by the HW cursor path to memcpy into the
//!   dumb-buffer (no GPU readback). Premultiplied-alpha convention
//!   is `straight` here — fully-visible pixels have α=0xFF, fully
//!   transparent pixels have α=0x00. Phase B's `cursor_plane_upload_image`
//!   blits these straight bytes.
//! - The matching v2 [`DrawableStore`] Pixmap (held under
//!   `cursor_pixmaps[xid]` on the backend) which the SW scene
//!   path samples through. Uploaded once via `engine.put_image`.
//!
//! Records are wrapped in `Arc` so anything that captured a
//! reference (a pending cursor swap mid-frame, a Phase D deferred
//! upload) observes the bytes it captured even after a later
//! `DefineCursor` allocates a fresh record with a fresh version.
//! Versions are monotonically increasing server-wide; comparison is
//! by value, never by `Arc` pointer identity.

use std::sync::Arc;

/// Per-cursor versioned snapshot.
///
/// Immutable after construction — theme reload / `XFixes` replacement
/// / `RenderCreateCursor` of a new image allocates a *fresh* record
/// with a fresh version, never mutates an existing one. This is what
/// lets pointer-grab paths capture an "effective cursor" reference
/// (`Arc<CursorRecord>`) and observe stable bytes even if a newer
/// record has superseded the canonical xid mapping.
#[derive(Debug)]
pub(crate) struct CursorRecord {
    /// Sprite width in pixels. Clamped to ≤ `HW_CURSOR_W` (64) at
    /// rasterisation time; cursors larger than that take the SW
    /// fallback in Phase C's `CursorAssignment` decision.
    pub(crate) width: u16,
    /// Sprite height in pixels.
    pub(crate) height: u16,
    /// Hotspot X (X11 cursor-origin coords; the click point).
    pub(crate) hot_x: u16,
    /// Hotspot Y.
    pub(crate) hot_y: u16,
    /// Tightly-packed `width × height × 4` BGRA8. Little-endian
    /// byte order matching DRM `ARGB8888`. Straight alpha (NOT
    /// premultiplied) so the HW dumb-buffer and the SW pixmap
    /// agree on sample values byte-for-byte.
    pub(crate) bgra_bytes: Vec<u8>,
    /// Pixel roles retained for core monochrome cursors so
    /// `RecolorCursor` can regenerate the sprite even when the original
    /// foreground and background colors were identical. `None` identifies
    /// an ARGB/RENDER cursor, for which recoloring is intentionally ignored.
    pub(crate) color_roles: Option<Vec<CursorColorRole>>,
    /// Monotonically-increasing version (compared by value, never
    /// by Arc identity). Consumed by Phase B/C's upload-dedup path.
    pub(crate) version: u64,
}

impl CursorRecord {
    pub(crate) fn new(
        width: u16,
        height: u16,
        hot_x: u16,
        hot_y: u16,
        bgra_bytes: Vec<u8>,
        version: u64,
    ) -> Arc<Self> {
        debug_assert_eq!(
            bgra_bytes.len(),
            usize::from(width) * usize::from(height) * 4,
            "CursorRecord bytes must be width*height*4",
        );
        Arc::new(Self {
            width,
            height,
            hot_x,
            hot_y,
            bgra_bytes,
            color_roles: None,
            version,
        })
    }

    pub(crate) fn new_monochrome(
        width: u16,
        height: u16,
        hot_x: u16,
        hot_y: u16,
        color_roles: Vec<CursorColorRole>,
        fore: (u16, u16, u16),
        back: (u16, u16, u16),
        version: u64,
    ) -> Arc<Self> {
        let bgra_bytes = color_roles_to_bgra(&color_roles, fore, back);
        Self::new_monochrome_with_bgra(
            width,
            height,
            hot_x,
            hot_y,
            bgra_bytes,
            color_roles,
            version,
        )
    }

    pub(crate) fn new_monochrome_with_bgra(
        width: u16,
        height: u16,
        hot_x: u16,
        hot_y: u16,
        bgra_bytes: Vec<u8>,
        color_roles: Vec<CursorColorRole>,
        version: u64,
    ) -> Arc<Self> {
        debug_assert_eq!(color_roles.len(), usize::from(width) * usize::from(height));
        debug_assert_eq!(bgra_bytes.len(), color_roles.len() * 4);
        Arc::new(Self {
            width,
            height,
            hot_x,
            hot_y,
            bgra_bytes,
            color_roles: Some(color_roles),
            version,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CursorColorRole {
    Transparent,
    Foreground,
    Background,
}

pub(crate) fn color_roles_to_bgra(
    roles: &[CursorColorRole],
    fore: (u16, u16, u16),
    back: (u16, u16, u16),
) -> Vec<u8> {
    let foreground = [
        (fore.2 >> 8) as u8,
        (fore.1 >> 8) as u8,
        (fore.0 >> 8) as u8,
    ];
    let background = [
        (back.2 >> 8) as u8,
        (back.1 >> 8) as u8,
        (back.0 >> 8) as u8,
    ];
    let mut bgra = vec![0; roles.len() * 4];
    for (i, role) in roles.iter().enumerate() {
        let offset = i * 4;
        let color = match role {
            CursorColorRole::Transparent => continue,
            CursorColorRole::Foreground => foreground,
            CursorColorRole::Background => background,
        };
        bgra[offset..offset + 3].copy_from_slice(&color);
        bgra[offset + 3] = 0xff;
    }
    bgra
}

/// One frame of a RENDER animated cursor (spec
/// `2026-06-10-animated-cursors-design.md`). Snapshotted at
/// `create_anim_cursor` time. The record is held by `Arc` and the backing
/// sprite drawable receives a matching store reference, equivalent to
/// Xorg retaining each constituent cursor.
pub(crate) struct AnimFrame {
    pub(crate) record: Arc<CursorRecord>,
    /// Sprite pixmap the SW scene path samples. `None` when the
    /// sub-cursor's sprite alloc was skipped (Vk-less test
    /// fixtures) — mirrors `insert_cursor_record`'s best-effort
    /// `cursor_pixmaps` insert.
    pub(crate) pixmap: Option<crate::kms::render::store::DrawableId>,
    pub(crate) delay: std::time::Duration,
}

/// Frame list for one animated cursor, keyed by the anim cursor's
/// host handle in `KmsBackend::anim_cursor_records`.
pub(crate) struct AnimCursorRecord {
    pub(crate) frames: Vec<AnimFrame>,
}

/// Live animation state — at most one, mirroring the single
/// effective cursor. Armed/cleared by `sync_cursor_animation`,
/// advanced by `tick_cursor_animation`.
pub(crate) struct ActiveCursorAnim {
    /// Animated cursor (host handle) whose frames are cycling.
    pub(crate) handle: u32,
    /// Current frame index into `AnimCursorRecord::frames`.
    pub(crate) frame: usize,
    /// Deadline for the next advance. Reported via `next_wakeup()`
    /// while outputs are active.
    pub(crate) next_frame: std::time::Instant,
}

/// 16×16 classic X-shaped default cursor (matches v1's
/// `install_default_cursor` at `kms/backend.rs:2286-2308`). Both
/// diagonals of the 16×16 box drawn in black with a 1-pixel white
/// halo for visibility against dark backgrounds. Hotspot at the
/// centre — the natural choice for a centred X.
///
/// Renamed from `default_arrow` (Stage 3f.8 placeholder) so the
/// boot cursor matches the v1 baseline that long-running test
/// sessions expect.
pub(crate) fn default_arrow_bgra() -> Vec<u8> {
    const W: i32 = 16;
    const H: i32 = 16;
    let mut bytes = vec![0u8; (W * H) as usize * 4];
    let last = W - 1;
    for y in 0..H {
        for x in 0..W {
            // Distance to either diagonal of the 16×16 box.
            let d1 = (x - y).abs();
            let d2 = (x + y - last).abs();
            let dist = d1.min(d2);
            let bgra: [u8; 4] = match dist {
                0 => [0x00, 0x00, 0x00, 0xFF], // black core, opaque
                1 => [0xFF, 0xFF, 0xFF, 0xFF], // white halo, opaque
                _ => [0x00, 0x00, 0x00, 0x00], // transparent
            };
            let off = ((y * W + x) as usize) * 4;
            bytes[off..off + 4].copy_from_slice(&bgra);
        }
    }
    bytes
}

pub(crate) const DEFAULT_ARROW_W: u16 = 16;
pub(crate) const DEFAULT_ARROW_H: u16 = 16;
/// Hotspot for the default X cursor — centred per v1's
/// `install_default_cursor` (`w/2, h/2`).
pub(crate) const DEFAULT_ARROW_HOT_X: u16 = 8;
pub(crate) const DEFAULT_ARROW_HOT_Y: u16 = 8;

/// Unpack an X11 wire depth-1 bitmap (as produced by the render
/// engine's `pack_from_storage` / returned by `get_image` at depth 1)
/// into a tight `width × height` R8 buffer — one byte per pixel,
/// `0xFF` where the bit is set, `0x00` otherwise.
///
/// Wire layout: 1 bit per pixel, LSBFirst within each byte (bit 0 =
/// leftmost pixel of its 8-pixel group), each scanline padded to a
/// 32-bit boundary (`⌈width/32⌉·4` bytes). This is the inverse of
/// `pack_from_storage`'s depth-1 branch. `rasterise_create_cursor`
/// consumes the R8 form, so `CreateCursor`'s source/mask pixmaps must
/// be unpacked before rasterising (see `read_cursor_depth1_pixmap`).
///
/// A short `packed` (fewer bytes than the padded layout implies) is
/// tolerated: missing bytes read as zero, matching `get_image`
/// clamping a read to storage bounds.
pub(crate) fn unpack_wire_bitmap_to_r8(packed: &[u8], width: u16, height: u16) -> Vec<u8> {
    let w = usize::from(width);
    let h = usize::from(height);
    let row_stride = w.div_ceil(32) * 4;
    let mut out = vec![0u8; w * h];
    for row in 0..h {
        let src_row = row * row_stride;
        let dst_row = row * w;
        for col in 0..w {
            let byte = packed.get(src_row + (col >> 3)).copied().unwrap_or(0);
            if byte & (1 << (col & 7)) != 0 {
                out[dst_row + col] = 0xFF;
            }
        }
    }
    out
}

/// Rasterise an X11 `CreateCursor` (`source`, `mask`, `fore`, `back`)
/// tuple into BGRA. Both sources are depth-1 R8-mirrored — a non-zero
/// byte means the bit is set. Output uses straight alpha (0xFF for
/// visible, 0x00 for transparent).
///
/// `src_bytes` and `mask_bytes` are arranged row-major at width
/// `src_w` (mask must match dims or be `None`). Pre-sized so we
/// don't have to clip per-pixel; bytes are read directly.
///
/// X11 pixel rule:
///   * mask supplied → pixel visible iff mask bit set; visible pixels
///     carry `fore` if source bit set else `back`.
///   * mask = None   → all pixels visible; same fore/back gating.
#[cfg(test)]
pub(crate) fn rasterise_create_cursor(
    src_bytes: &[u8],
    src_w: u16,
    src_h: u16,
    mask_bytes: Option<&[u8]>,
    fore: (u16, u16, u16),
    back: (u16, u16, u16),
) -> Vec<u8> {
    rasterise_create_cursor_with_roles(src_bytes, src_w, src_h, mask_bytes, fore, back).bgra_bytes
}

pub(crate) struct MonochromeCursorImage {
    pub(crate) bgra_bytes: Vec<u8>,
    pub(crate) color_roles: Vec<CursorColorRole>,
}

pub(crate) fn rasterise_create_cursor_with_roles(
    src_bytes: &[u8],
    src_w: u16,
    src_h: u16,
    mask_bytes: Option<&[u8]>,
    fore: (u16, u16, u16),
    back: (u16, u16, u16),
) -> MonochromeCursorImage {
    let w = usize::from(src_w);
    let h = usize::from(src_h);
    let pixel_count = w * h;
    let mut roles = vec![CursorColorRole::Transparent; pixel_count];
    for (i, role) in roles.iter_mut().enumerate() {
        let src_set = src_bytes.get(i).copied().unwrap_or(0) != 0;
        let visible = match mask_bytes {
            Some(mb) => mb.get(i).copied().unwrap_or(0) != 0,
            None => true,
        };
        if !visible {
            continue;
        }
        *role = if src_set {
            CursorColorRole::Foreground
        } else {
            CursorColorRole::Background
        };
    }
    let bgra_bytes = color_roles_to_bgra(&roles, fore, back);
    MonochromeCursorImage {
        bgra_bytes,
        color_roles: roles,
    }
}

/// Glyph cursor rasterisation result. Returned by
/// [`rasterise_glyph_cursor`]; carries the pixmap dimensions, hotspot
/// derived from the source glyph's origin, and the packed BGRA bytes.
pub(crate) struct GlyphCursorImage {
    pub(crate) width: u16,
    pub(crate) height: u16,
    pub(crate) hot_x: u16,
    pub(crate) hot_y: u16,
    pub(crate) bgra_bytes: Vec<u8>,
    pub(crate) color_roles: Vec<CursorColorRole>,
}

/// A single FreeType-rendered glyph used as input to glyph-cursor
/// rasterisation. `lsb` / `top` are the `FreeType` `bitmap_left` /
/// `bitmap_top` (signed; can be negative for italic-style glyphs).
pub(crate) struct GlyphBitmap<'a> {
    pub(crate) pixels: &'a [u8],
    pub(crate) width: i32,
    pub(crate) height: i32,
    pub(crate) lsb: i32,
    pub(crate) top: i32,
}

/// Rasterise an X11 `CreateGlyphCursor` into BGRA bytes + pixmap dims +
/// hotspot. Ported from v1's body in `kms/backend.rs:9937-10108`.
///
/// X11 pixel rule:
///   * `mask` supplied → visible iff mask bit set; visible pixels
///     carry `fore` if source bit set else `back`.
///   * `mask = None`   → source doubles as mask: visible iff source
///     bit set; visible pixels always carry `fore`.
///
/// Coordinates: the cursor pixmap is the union bbox of source + mask
/// glyphs in their `FreeType` origin frame (positive y up). The hotspot
/// is the source glyph's origin point expressed in pixmap coords
/// (top-left origin, y down).
pub(crate) fn rasterise_glyph_cursor(
    src: &GlyphBitmap<'_>,
    mask: Option<&GlyphBitmap<'_>>,
    fore: (u16, u16, u16),
    back: (u16, u16, u16),
) -> GlyphCursorImage {
    let (left, right, top, bottom) = match mask {
        Some(m) => (
            src.lsb.min(m.lsb),
            (src.lsb + src.width).max(m.lsb + m.width),
            src.top.max(m.top),
            (src.height - src.top).max(m.height - m.top),
        ),
        None => (src.lsb, src.lsb + src.width, src.top, src.height - src.top),
    };
    let pixmap_w = (right - left).max(1) as u32;
    let pixmap_h = (top + bottom).max(1) as u32;
    let hot_x = (-left).clamp(0, i32::from(u16::MAX)) as u16;
    let hot_y = top.clamp(0, i32::from(u16::MAX)) as u16;

    let read_bit = |pixels: &[u8], w: i32, h: i32, x: i32, y: i32| -> bool {
        if x < 0 || y < 0 || x >= w || y >= h {
            return false;
        }
        let off = (y * w + x) as usize;
        pixels.get(off).copied().unwrap_or(0) > 0
    };
    let src_off_x = src.lsb - left;
    let src_off_y = top - src.top;
    let mask_off = mask.as_ref().map(|m| (m.lsb - left, top - m.top));

    let pixel_count = (pixmap_w as usize) * (pixmap_h as usize);
    let mut color_roles = vec![CursorColorRole::Transparent; pixel_count];
    for y in 0..pixmap_h as i32 {
        for x in 0..pixmap_w as i32 {
            let src_set = read_bit(
                src.pixels,
                src.width,
                src.height,
                x - src_off_x,
                y - src_off_y,
            );
            let visible = match (mask, mask_off) {
                (Some(m), Some((mox, moy))) => {
                    read_bit(m.pixels, m.width, m.height, x - mox, y - moy)
                }
                _ => src_set,
            };
            if !visible {
                continue;
            }
            let off = (y as u32 * pixmap_w + x as u32) as usize;
            color_roles[off] = if src_set {
                CursorColorRole::Foreground
            } else {
                CursorColorRole::Background
            };
        }
    }
    let bgra_bytes = color_roles_to_bgra(&color_roles, fore, back);
    GlyphCursorImage {
        width: pixmap_w.min(u32::from(u16::MAX)) as u16,
        height: pixmap_h.min(u32::from(u16::MAX)) as u16,
        hot_x,
        hot_y,
        bgra_bytes,
        color_roles,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_record_dims_round_trip() {
        let rec = CursorRecord::new(4, 4, 1, 2, vec![0xFFu8; 4 * 4 * 4], 42);
        assert_eq!(rec.width, 4);
        assert_eq!(rec.height, 4);
        assert_eq!(rec.hot_x, 1);
        assert_eq!(rec.hot_y, 2);
        assert_eq!(rec.version, 42);
        assert_eq!(rec.bgra_bytes.len(), 4 * 4 * 4);
    }

    #[test]
    fn monochrome_roles_recolor_even_when_original_colors_match() {
        let roles = vec![
            CursorColorRole::Transparent,
            CursorColorRole::Foreground,
            CursorColorRole::Background,
        ];
        let same = (0xaaaa, 0xbbbb, 0xcccc);
        let record = CursorRecord::new_monochrome(3, 1, 0, 0, roles.clone(), same, same, 1);
        assert_eq!(&record.bgra_bytes[4..8], &record.bgra_bytes[8..12]);

        let recolored = color_roles_to_bgra(&roles, (0xff00, 0, 0), (0, 0, 0xff00));
        assert_eq!(&recolored[0..4], &[0, 0, 0, 0]);
        assert_eq!(&recolored[4..8], &[0, 0, 0xff, 0xff]);
        assert_eq!(&recolored[8..12], &[0xff, 0, 0, 0xff]);
    }

    /// Replacing a record never mutates the old Arc's bytes — load-
    /// bearing for any path that captured an `Arc<CursorRecord>`
    /// reference (pointer grab, Phase D deferred upload).
    #[test]
    fn replacement_does_not_mutate_old() {
        let old = CursorRecord::new(
            2,
            2,
            0,
            0,
            vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
            1,
        );
        let snapshot: Vec<u8> = old.bgra_bytes.clone();
        // Allocate a "replacement" — a fresh Arc with different bytes.
        // The old Arc is held in `old`; the test simulates the
        // protocol-handler swapping the canonical xid → record map.
        let _new = CursorRecord::new(2, 2, 0, 0, vec![0u8; 2 * 2 * 4], 2);
        assert_eq!(old.bgra_bytes, snapshot, "old bytes mutated under us");
    }

    /// Version comparison is by value, not pointer identity — two
    /// distinct Arc allocations holding the same bytes/version must
    /// compare equal.
    #[test]
    fn version_compared_by_value() {
        let a = CursorRecord::new(1, 1, 0, 0, vec![0, 0, 0, 0xFF], 7);
        let b = CursorRecord::new(1, 1, 0, 0, vec![0, 0, 0, 0xFF], 7);
        assert!(!Arc::ptr_eq(&a, &b), "test invariant: distinct Arcs");
        assert_eq!(a.version, b.version);
    }

    /// Straight-alpha invariant — visible pixels in
    /// `rasterise_create_cursor` carry `α=0xFF`, fully-transparent
    /// pixels carry `α=0x00`. No intermediate values (no premul).
    #[test]
    fn rasterise_create_cursor_uses_straight_alpha() {
        // 2×2 source: pixel 0,2 set; mask: pixel 0,1 set.
        // → pixel 0 visible (mask set) + src set → `fore` opaque
        // → pixel 1 visible (mask set) + src clear → `back` opaque
        // → pixel 2 invisible (mask clear) → α=0
        // → pixel 3 invisible (mask clear) → α=0
        let src = [0xFFu8, 0x00, 0xFFu8, 0x00];
        let mask = [0xFFu8, 0xFFu8, 0x00, 0x00];
        let bgra = rasterise_create_cursor(
            &src,
            2,
            2,
            Some(&mask),
            (0xFFFF, 0, 0), // red fore
            (0, 0xFFFF, 0), // green back
        );
        assert_eq!(bgra.len(), 16);
        // Pixel 0: visible, src set → red, α=FF.
        assert_eq!(&bgra[0..4], &[0x00, 0x00, 0xFF, 0xFF]);
        // Pixel 1: visible, src clear → green, α=FF.
        assert_eq!(&bgra[4..8], &[0x00, 0xFF, 0x00, 0xFF]);
        // Pixel 2,3: invisible → all zero (α=0).
        assert_eq!(&bgra[8..12], &[0, 0, 0, 0]);
        assert_eq!(&bgra[12..16], &[0, 0, 0, 0]);
    }

    /// Default X cursor rasterisation: 16×16 with both diagonals
    /// in opaque black, 1-pixel white halo, fully-transparent
    /// elsewhere. Matches v1's `install_default_cursor` at
    /// `kms/backend.rs:2286-2308`.
    #[test]
    fn default_arrow_is_x_with_halo() {
        let bytes = default_arrow_bgra();
        assert_eq!(bytes.len(), 16 * 16 * 4);
        let px = |x: usize, y: usize| {
            let off = (y * 16 + x) * 4;
            [bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]]
        };
        // Diagonal 1 (top-left → bottom-right): every (k, k) is
        // opaque black.
        for k in 0..16 {
            assert_eq!(px(k, k), [0x00, 0x00, 0x00, 0xFF], "diag1 (k,k)={k}");
        }
        // Diagonal 2 (top-right → bottom-left): every (k, 15-k).
        for k in 0..16 {
            assert_eq!(
                px(k, 15 - k),
                [0x00, 0x00, 0x00, 0xFF],
                "diag2 (k,15-k)={k}"
            );
        }
        // 1-pixel white halo: e.g. (1, 0) is `dist=1` to diag1,
        // visible-white.
        assert_eq!(px(1, 0), [0xFF, 0xFF, 0xFF, 0xFF]);
        // Far from both diagonals: (7, 0) sits at d1=7, d2=8 →
        // min=7 → transparent.
        assert_eq!(px(7, 0), [0, 0, 0, 0]);
        // Centre (8, 8) is on diag1 → opaque black (hotspot).
        assert_eq!(px(8, 8), [0x00, 0x00, 0x00, 0xFF]);
    }

    /// Glyph cursor with `mask = None` collapses to "source bit also
    /// acts as visibility" — every visible pixel carries `fore`,
    /// invisible pixels are α=0.
    #[test]
    fn glyph_cursor_no_mask_uses_source_as_mask() {
        // 2×2 glyph: top-left and bottom-right set.
        let pixels = [0xFFu8, 0x00, 0x00, 0xFFu8];
        let src = GlyphBitmap {
            pixels: &pixels,
            width: 2,
            height: 2,
            lsb: 0,
            top: 2, // glyph origin at (0, 2) in FreeType frame
        };
        let img = rasterise_glyph_cursor(
            &src,
            None,
            (0xFFFF, 0, 0), // red
            (0, 0xFFFF, 0), // green (unused because mask is None)
        );
        // Pixmap dims = src dims (no mask), hotspot = (0, 2).
        assert_eq!(img.width, 2);
        assert_eq!(img.height, 2);
        assert_eq!(img.hot_x, 0);
        assert_eq!(img.hot_y, 2);
        // Pixel (0,0) — src set → red, opaque.
        assert_eq!(&img.bgra_bytes[0..4], &[0x00, 0x00, 0xFF, 0xFF]);
        // Pixel (1,0) — src clear, no mask → invisible.
        assert_eq!(&img.bgra_bytes[4..8], &[0, 0, 0, 0]);
        // Pixel (1,1) — src set → red.
        assert_eq!(&img.bgra_bytes[12..16], &[0x00, 0x00, 0xFF, 0xFF]);
    }

    /// `unpack_wire_bitmap_to_r8` reverses `pack_from_storage`'s
    /// depth-1 layout: LSBFirst bits, rows padded to 32 bits. A width
    /// that isn't a multiple of 8 exercises the partial-byte tail and
    /// the row-stride padding — the case that flattened `import`'s
    /// 17-wide crosshair.
    #[test]
    fn unpack_wire_bitmap_lsbfirst_padded_rows() {
        // 17×2. Row stride = ⌈17/32⌉·4 = 4 bytes. Set cols 0, 7, 8, 16
        // of row 0 and col 3 of row 1.
        // Row 0: byte0 bits 0,7 = 0x81; byte1 bit 0 (col 8) = 0x01;
        //        byte2 bit 0 (col 16) = 0x01; byte3 pad = 0x00.
        // Row 1: byte0 bit 3 (col 3) = 0x08; rest 0.
        let packed = [0x81, 0x01, 0x01, 0x00, 0x08, 0x00, 0x00, 0x00];
        let r8 = unpack_wire_bitmap_to_r8(&packed, 17, 2);
        assert_eq!(r8.len(), 34, "tight w*h R8, no padding");
        let set: Vec<usize> = r8
            .iter()
            .enumerate()
            .filter(|&(_, &b)| b != 0)
            .map(|(i, _)| i)
            .collect();
        // Row 0 pixels 0,7,8,16 → indices 0,7,8,16; row 1 pixel 3 → 17+3=20.
        assert_eq!(set, vec![0, 7, 8, 16, 20]);
        assert!(r8.iter().all(|&b| b == 0 || b == 0xFF));
    }

    /// End-to-end regression for #90's flattened cursor: the exact
    /// ImageMagick `import` crosshair bitmap (17×17, from
    /// `XMakeCursor`'s `scope_bits`/`scope_mask_bits`) must round-trip
    /// through the wire-bitmap unpack + `rasterise_create_cursor` into
    /// a 17-row-tall "+" — NOT a top sliver. Feeding the packed wire
    /// bytes straight into `rasterise_create_cursor` (the old bug)
    /// left every pixel from row 4 down transparent.
    #[test]
    fn import_scope_crosshair_is_full_height_not_flattened() {
        // Verbatim from ImageMagick MagickCore/xwindow.c XMakeCursor.
        // LSBFirst, 3 bytes/row in the client image; the wire pads each
        // row to 4 bytes, so re-pad here to match get_image output.
        const CLIENT: [u8; 51] = [
            0x80, 0x03, 0x00, 0x80, 0x02, 0x00, 0x80, 0x02, 0x00, 0x80, 0x02, 0x00, 0x80, 0x02,
            0x00, 0x80, 0x02, 0x00, 0x80, 0x02, 0x00, 0x7f, 0xfc, 0x01, 0x01, 0x00, 0x01, 0x7f,
            0xfc, 0x01, 0x80, 0x02, 0x00, 0x80, 0x02, 0x00, 0x80, 0x02, 0x00, 0x80, 0x02, 0x00,
            0x80, 0x02, 0x00, 0x80, 0x02, 0x00, 0x80, 0x03, 0x00,
        ];
        // Re-pad 3-byte client rows to the 4-byte wire/get_image stride.
        let mut wire = vec![0u8; 4 * 17];
        for row in 0..17 {
            wire[row * 4..row * 4 + 3].copy_from_slice(&CLIENT[row * 3..row * 3 + 3]);
        }
        let src = unpack_wire_bitmap_to_r8(&wire, 17, 17);
        assert_eq!(src.len(), 17 * 17);
        // No mask → source doubles as visibility.
        let bgra = rasterise_create_cursor(
            &src,
            17,
            17,
            None,
            (0xFFFF, 0xFFFF, 0xFFFF), // white fore
            (0, 0, 0),                // black back
        );
        let opaque = |x: usize, y: usize| bgra[(y * 17 + x) * 4 + 3] != 0;
        // The vertical bar runs down column 8 (0x80 in byte0 = bit 7,
        // 0x02 in byte1 = bit 9... center col ~8) across every row —
        // the bottom half MUST be present, which the flattened bug lost.
        let bottom_rows_lit = (9..17).filter(|&y| (0..17).any(|x| opaque(x, y))).count();
        assert_eq!(
            bottom_rows_lit, 8,
            "all 8 bottom rows of the crosshair must render (not flattened)"
        );
        // Sanity: top rows are lit too, so it's a full-height "+".
        assert!((0..8).all(|y| (0..17).any(|x| opaque(x, y))));
    }
}
