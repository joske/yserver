//! Shared helpers used by `kms::render` (the rendering backend).
//!
//! Historical note: this module was originally the home of `KmsBackend`
//! (the v1 rendering path) plus its supporting types. The v1 path was
//! retired 2026-05-26 after v2 closed Phase B.3 on bee / yoga / silence
//! / air / nvidia hardware. What remains is the small set of free
//! functions + plain-data types v2 still uses:
//!
//! - `ActiveOutput` / `Rect` / `PlatformInit` / `platform_init` —
//!   per-output bring-up that v2's `PlatformBackend::open_with_commit`
//!   delegates into.
//! - Wire-byte helpers (`read_i16_pair`, `read_rect`) consumed by v2's
//!   poly_* dispatch.
//! - Rasterisation helpers (`bresenham_segment`,
//!   `scanline_fill_polygon`, `clip_rects_to_image`) consumed by v2's
//!   poly_line / poly_segment / poly_arc / fill_poly lowering.
//! - SHAPE / clip-mask helpers (`ClipMaskCache`,
//!   `rasterize_pixmap_mask_to_rects`) consumed by v2's GC clip path.
//! - RENDER affine helpers (`compose_affines`,
//!   `pixman_transform_to_affine`, `repeat_to_shader_const`) consumed
//!   by v2's render_composite / render_traps_or_tris.
//! - `parse_add_glyphs` — RENDER AddGlyphs wire decode, consumed by
//!   v2's render_add_glyphs.
//!
//! Module name kept as `backend` for now to avoid touching every v2
//! `crate::kms::backend::FOO` import in the same change that removed
//! v1; a future rename to something like `kms::raster` is fine but
//! not load-bearing.

use std::{
    io,
    path::{Path, PathBuf},
    rc::Rc,
};

use crate::{
    drm,
    kms::{
        core::{GlyphSetFormat, GlyphSetState, StoredGlyph},
        cpu_types::{PictTransform, Rectangle16, Repeat},
        scanout_route::ScanoutRoute,
    },
};

/// `depth` and `row_stride` together describe the byte layout:
///   - `depth=1`: bytes are wire-format ZPixmap (packed bits LSB-first
///     within each byte — bit 0 = leftmost pixel — scanline-padded to
///     32 bits — `row_stride = ((width + 31) / 32) * 4`). Matches the
///     server's advertised `bitmap-bit-order=LSBFirst`.
///   - `depth=8`: bytes are one byte per pixel (any non-zero byte = set);
///     `row_stride = ((width + 3) / 4) * 4` for X11 wire format, or
///     `row_stride = width` for storage R8 readback (v1's path).
pub(crate) struct ClipMaskCache {
    /// Host xid of the mask pixmap. Used by `apply_clip_state` to
    /// skip re-readback when the GC is re-applied with the same
    /// pixmap + origin between paints.
    pub(crate) pixmap_xid: u32,
    /// Live drawable identity captured at read time. `pixmap_xid` is the
    /// installed-GC handle (survives free); `drawable_id` distinguishes a
    /// re-allocated pixmap at a recycled xid.
    pub(crate) drawable_id: crate::kms::render::store::DrawableId,
    /// `Drawable.content_version` at the moment the bytes were read. While a
    /// live drawable still exists, reuse requires this to still match.
    pub(crate) content_version: u64,
    pub(crate) origin: (i16, i16),
    pub(crate) width: u16,
    pub(crate) height: u16,
    pub(crate) depth: u8,
    pub(crate) row_stride: u32,
    /// True while the cache only carries identity/origin metadata. CPU bytes
    /// are materialized lazily on the first run-based clip consumer, or
    /// captured at `free_pixmap` before the source drawable disappears.
    pub(crate) cpu_bytes_pending: bool,
    pub(crate) bytes: Vec<u8>,
}

/// CPU cache of depth-1 SHAPE::Mask readbacks (`read_depth1_pixmap`),
/// added for #32/#96: on discrete NVIDIA each mask read is a VRAM->CPU
/// round-trip that stalls the single-threaded loop, and ~60% of them are
/// re-reads of an UNCHANGED mask (Peter's `rdepth1_diag`, 2026-07-24).
/// On host-visible (integrated/APU) GPUs the readback is a cheap memcpy,
/// so this is simply a no-op win there.
///
/// Keyed by `DrawableId` — minted monotonically and NEVER recycled, so a
/// freed+reallocated pixmap always gets a fresh id and cannot alias a
/// stale entry — and validated by `content_version`, which is bumped on
/// every pixel write (the same invariant `ClipMaskCache` relies on), so
/// any draw into the mask forces a re-read. Bounded LRU so masks of
/// long-gone pixmaps can't accumulate (no free-path hook needed).
pub(crate) struct Depth1MaskCache {
    entries: std::collections::HashMap<crate::kms::render::store::DrawableId, Depth1MaskEntry>,
    /// LRU order, back = most-recently-used. `touch` removes any prior
    /// occurrence before pushing, so this stays duplicate-free and
    /// `entries.len() == order.len()` holds.
    order: std::collections::VecDeque<crate::kms::render::store::DrawableId>,
    cap: usize,
}

struct Depth1MaskEntry {
    content_version: u64,
    width: u32,
    height: u32,
    bytes: Vec<u8>,
}

impl Depth1MaskCache {
    /// `cap` = max distinct masks retained (clamped to >= 1). Peter's
    /// session touched ~176 distinct masks; 256 covers it comfortably.
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            entries: std::collections::HashMap::new(),
            order: std::collections::VecDeque::new(),
            cap: cap.max(1),
        }
    }

    /// Cache hit iff we hold `id` at the SAME `content_version` AND the
    /// SAME extent. Refreshes LRU recency and returns an owned copy of the
    /// mask — a few-KB memcpy, vastly cheaper than the GPU readback it
    /// replaces. The `width`/`height` guard is belt-and-suspenders: X11
    /// pixmaps (the only SHAPE::Mask sources) can't resize, and any pixel
    /// write already bumps `content_version`, but validating the extent
    /// means a hit can never hand back bytes sized for a different
    /// allocation even if some future caller reuses this with a
    /// resizable drawable.
    pub(crate) fn get(
        &mut self,
        id: crate::kms::render::store::DrawableId,
        content_version: u64,
        width: u32,
        height: u32,
    ) -> Option<(u32, u32, Vec<u8>)> {
        let hit = matches!(
            self.entries.get(&id),
            Some(e) if e.content_version == content_version
                && e.width == width
                && e.height == height
        );
        if !hit {
            return None;
        }
        self.touch(id);
        let e = &self.entries[&id];
        Some((e.width, e.height, e.bytes.clone()))
    }

    /// Insert (or replace) `id`'s entry and evict LRU victims past `cap`.
    pub(crate) fn insert(
        &mut self,
        id: crate::kms::render::store::DrawableId,
        content_version: u64,
        width: u32,
        height: u32,
        bytes: Vec<u8>,
    ) {
        self.entries.insert(
            id,
            Depth1MaskEntry {
                content_version,
                width,
                height,
                bytes,
            },
        );
        self.touch(id);
        while self.entries.len() > self.cap {
            match self.order.pop_front() {
                Some(victim) => {
                    self.entries.remove(&victim);
                }
                None => break,
            }
        }
    }

    /// Move `id` to the MRU end, removing any stale position first so
    /// `order` never holds duplicates.
    fn touch(&mut self, id: crate::kms::render::store::DrawableId) {
        if let Some(pos) = self.order.iter().position(|&x| x == id) {
            self.order.remove(pos);
        }
        self.order.push_back(id);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        debug_assert_eq!(self.entries.len(), self.order.len());
        self.entries.len()
    }
}

/// Rasterise an X11 pixmap clip-mask against a paint-rect list.
///
/// X11 GC clip-mask: a pixel paints iff the mask bit at
/// `(dst_x - clip_origin.x, dst_y - clip_origin.y)` is 1. Mask
/// coordinates outside `[0, mask_width) × [0, mask_height)` are
/// treated as 0 (no paint).
///
/// `mask_depth` is 1 (canonical) or 8 (any non-zero byte = paint).
/// `mask_row_stride` is the number of bytes per row in `mask_bytes`
/// (X11 scanline-padded; for depth-1 with the server's 32-bit
/// scanline pad this is `((width + 31) / 32) * 4`).
///
/// Bit order for depth-1 is **LSB-first** within each byte (bit 0 =
/// leftmost pixel in that byte's 8-pixel group). Matches the
/// server's advertised `bitmap-bit-order` (`LSBFirst` for the
/// x86-default client byte order), v1's depth-1 PutImage unpacker,
/// and v2's `pack_from_storage` / `unpack_to_staging` depth-1
/// branches — all forwards/backwards round-trip through LSB-first
/// packed bytes.
///
/// Emits horizontal runs as rectangles (consecutive set bits in a
/// row become one wide rect). Empty input or fully-masked paints
/// return an empty Vec.
pub(crate) fn rasterize_pixmap_mask_to_rects(
    paint_rects: &[Rectangle16],
    mask_bytes: &[u8],
    mask_width: u16,
    mask_height: u16,
    mask_depth: u32,
    mask_row_stride: u32,
    clip_origin: (i16, i16),
) -> Vec<Rectangle16> {
    let mw = i32::from(mask_width);
    let mh = i32::from(mask_height);
    let ox = i32::from(clip_origin.0);
    let oy = i32::from(clip_origin.1);
    let stride = mask_row_stride as usize;
    let mut out: Vec<Rectangle16> = Vec::new();
    let pixel_set = |mx: i32, my: i32| -> bool {
        if mx < 0 || my < 0 || mx >= mw || my >= mh {
            return false;
        }
        let row = my as usize * stride;
        match mask_depth {
            1 => {
                let byte = row + (mx as usize / 8);
                let bit = (mx as usize) % 8;
                mask_bytes.get(byte).is_some_and(|b| (b >> bit) & 1 != 0)
            }
            8 => mask_bytes.get(row + mx as usize).is_some_and(|b| *b != 0),
            _ => false,
        }
    };
    for r in paint_rects {
        let rx0 = i32::from(r.x);
        let ry0 = i32::from(r.y);
        let rx1 = rx0 + i32::from(r.width);
        let ry1 = ry0 + i32::from(r.height);
        for dy in ry0..ry1 {
            let my = dy - oy;
            let mut run_start: Option<i32> = None;
            for dx in rx0..rx1 {
                let mx = dx - ox;
                if pixel_set(mx, my) {
                    if run_start.is_none() {
                        run_start = Some(dx);
                    }
                } else if let Some(s) = run_start.take() {
                    out.push(Rectangle16 {
                        x: s as i16,
                        y: dy as i16,
                        width: (dx - s) as u16,
                        height: 1,
                    });
                }
            }
            if let Some(s) = run_start {
                out.push(Rectangle16 {
                    x: s as i16,
                    y: dy as i16,
                    width: (rx1 - s) as u16,
                    height: 1,
                });
            }
        }
    }
    out
}

/// Append rects covering a thin line from (x0,y0) to (x1,y1). Axis-aligned
/// (horizontal / vertical) segments emit a single span rect; diagonals emit
/// one 1×1 rect per Bresenham pixel. Pixel coverage matches a per-pixel walk.
pub(crate) fn bresenham_segment(x0: i32, y0: i32, x1: i32, y1: i32, out: &mut Vec<Rectangle16>) {
    // Axis-aligned fast path: a horizontal or vertical thin line is ONE span,
    // not N per-pixel 1×1 rects. This is the overwhelmingly common case
    // (rectangle edges, borders, grids) and matches Xorg's fb layer, which
    // fills H/V lines as a single span. Pixel coverage is identical to the
    // per-pixel loop below; only the rect count shrinks. Emitting per-pixel
    // here previously exploded rectangle outlines into thousands of rects
    // (e.g. blew the root-overlay rect cap, making wide XOR rubber-bands
    // vanish) and cost one GPU draw per pixel.
    let clamp16 = |v: i32| v.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
    if y0 == y1 {
        let (xa, xb) = (x0.min(x1), x0.max(x1));
        let w = u16::try_from(xb - xa + 1).unwrap_or(u16::MAX);
        out.push(Rectangle16 {
            x: clamp16(xa),
            y: clamp16(y0),
            width: w,
            height: 1,
        });
        return;
    }
    if x0 == x1 {
        let (ya, yb) = (y0.min(y1), y0.max(y1));
        let h = u16::try_from(yb - ya + 1).unwrap_or(u16::MAX);
        out.push(Rectangle16 {
            x: clamp16(x0),
            y: clamp16(ya),
            width: 1,
            height: h,
        });
        return;
    }
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    let (mut x, mut y) = (x0, y0);
    loop {
        out.push(Rectangle16 {
            x: x.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            y: y.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            width: 1,
            height: 1,
        });
        if x == x1 && y == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
    }
}

/// Scanline fill a polygon (even-odd rule).  Edges are pairs of i32
/// vertices.  Output is a Vec of 1-pixel-tall horizontal Rectangle16 spans.
pub(crate) fn scanline_fill_polygon(verts: &[(i32, i32)], out: &mut Vec<Rectangle16>) {
    if verts.len() < 3 {
        return;
    }
    let y_min = verts.iter().map(|&(_, y)| y).min().unwrap();
    let y_max = verts.iter().map(|&(_, y)| y).max().unwrap();
    let mut crossings: Vec<i32> = Vec::with_capacity(verts.len());
    for y in y_min..=y_max {
        crossings.clear();
        for i in 0..verts.len() {
            let (x0, y0) = verts[i];
            let (x1, y1) = verts[(i + 1) % verts.len()];
            // Skip horizontal edges; use half-open [min_y, max_y) so
            // shared vertices contribute exactly once across two edges.
            let (ya, yb) = if y0 < y1 { (y0, y1) } else { (y1, y0) };
            if ya == yb || y < ya || y >= yb {
                continue;
            }
            // Linear interpolation: x at scanline y.
            let x = x0 as i64 + ((y - y0) as i64 * (x1 - x0) as i64) / (y1 - y0) as i64;
            crossings.push(x as i32);
        }
        crossings.sort_unstable();
        let mut i = 0;
        while i + 1 < crossings.len() {
            let x_start = crossings[i];
            let x_end = crossings[i + 1];
            if x_end > x_start {
                let w = (x_end - x_start) as i64;
                out.push(Rectangle16 {
                    x: x_start.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                    y: y.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                    width: w.min(u16::MAX as i64) as u16,
                    height: 1,
                });
            }
            i += 2;
        }
    }
}

/// Clip a list of `Rectangle16` to the bounds `[0, iw) × [0, ih)` and drop
/// rects that fall entirely outside.  Pixman's `fill_rectangles` is supposed
/// to clip on its own but in our build a partially-out-of-bounds rect
/// (especially with negative x/y) can segfault; pre-clipping is the cheap
/// defensive workaround.
pub(crate) fn clip_rects_to_image(rects: &[Rectangle16], iw: i32, ih: i32) -> Vec<Rectangle16> {
    let mut out = Vec::with_capacity(rects.len());
    for r in rects {
        let x1 = (r.x as i32).max(0);
        let y1 = (r.y as i32).max(0);
        let x2 = ((r.x as i32) + r.width as i32).min(iw);
        let y2 = ((r.y as i32) + r.height as i32).min(ih);
        if x2 <= x1 || y2 <= y1 {
            continue;
        }
        out.push(Rectangle16 {
            x: x1 as i16,
            y: y1 as i16,
            width: (x2 - x1) as u16,
            height: (y2 - y1) as u16,
        });
    }
    out
}

/// Translate a [`Repeat`] enum value to the integer constant the
/// `render.frag.glsl` shader expects (matches the protocol numbering;
/// see `render_pipeline::REPEAT_*`).
pub(crate) fn repeat_to_shader_const(repeat: Repeat) -> i32 {
    use crate::kms::vk::render_pipeline::{REPEAT_NONE, REPEAT_NORMAL, REPEAT_PAD, REPEAT_REFLECT};
    match repeat {
        Repeat::None => REPEAT_NONE,
        Repeat::Normal => REPEAT_NORMAL,
        Repeat::Pad => REPEAT_PAD,
        Repeat::Reflect => REPEAT_REFLECT,
    }
}

/// Convert an X11 RENDER pixman 3×3 transform (16.16 fixed-point) into
/// the affine 2×3 form the `render.frag.glsl` shader uses. RENDER's
/// transform maps the *destination*-relative source coordinate to the
/// pre-sample source pixel:
///
/// ```text
///   src_pixel = M * (src_origin + dst_offset, 1)
/// ```
///
/// We assume affine — the bottom row is `[0, 0, 1]`. Real X11 clients
/// use affine transforms in practice; projective transforms (the rare
/// case) round-trip through the affine portion only and produce wrong
/// pixels at the perspective-divide corners. That trade-off is
/// documented in `feedback_phase4_1_4_decisions.md` § component-alpha
/// and matches the per-family-port strict-acceptance relaxation.
/// Compose two affine 2×3 transforms. The result satisfies
/// `compose(A, B) * v == A * (B * v)` when `v` is `(x, y, 1)`.
pub(crate) fn compose_affines(
    a: crate::kms::vk::ops::render::AffineXform,
    b: crate::kms::vk::ops::render::AffineXform,
) -> crate::kms::vk::ops::render::AffineXform {
    use crate::kms::vk::ops::render::AffineXform;
    // a.row0 = (a00, a01, a02), a.row1 = (a10, a11, a12). Bottom row
    // implicit `[0, 0, 1]`. Same for b.
    let a00 = a.row0[0];
    let a01 = a.row0[1];
    let a02 = a.row0[2];
    let a10 = a.row1[0];
    let a11 = a.row1[1];
    let a12 = a.row1[2];
    let b00 = b.row0[0];
    let b01 = b.row0[1];
    let b02 = b.row0[2];
    let b10 = b.row1[0];
    let b11 = b.row1[1];
    let b12 = b.row1[2];
    AffineXform {
        row0: [
            a00 * b00 + a01 * b10,
            a00 * b01 + a01 * b11,
            a00 * b02 + a01 * b12 + a02,
            0.0,
        ],
        row1: [
            a10 * b00 + a11 * b10,
            a10 * b01 + a11 * b11,
            a10 * b02 + a11 * b12 + a12,
            0.0,
        ],
    }
}

pub(crate) fn pixman_transform_to_affine(
    transform: Option<&PictTransform>,
    _src_extent: ash::vk::Extent2D,
) -> crate::kms::vk::ops::render::AffineXform {
    use crate::kms::vk::ops::render::AffineXform;
    let Some(t) = transform else {
        return AffineXform::IDENTITY;
    };
    // pixman_transform stores 9 fixed-point i32 values in row-major
    // order. matrix[row][col] in 16.16 fixed point.
    let m = t.matrix;
    let to_f = |v: i32| (v as f32) / 65536.0;
    let mut a = to_f(m[0][0]);
    let mut b = to_f(m[0][1]);
    let mut tx = to_f(m[0][2]);
    let mut c = to_f(m[1][0]);
    let mut d = to_f(m[1][1]);
    let mut ty = to_f(m[1][2]);
    // Constant-divisor projective transforms (matrix row 2 = `[0, 0, w]`
    // with w ≠ 1) collapse to a uniform 1/w scale on the affine portion.
    // Rendercheck's tscoords/tmcoords cases use this form to scale a 5×5
    // src 8×; pixman handles it the same way. Non-constant projective
    // transforms (m[2][0] or m[2][1] non-zero) genuinely vary per-pixel
    // and we don't model them — the affine portion is used as-is, which
    // matches the strict-acceptance relaxation in
    // `feedback_phase4_1_4_decisions.md`.
    let m20 = to_f(m[2][0]);
    let m21 = to_f(m[2][1]);
    let m22 = to_f(m[2][2]);
    if m20 == 0.0 && m21 == 0.0 && m22 != 0.0 && m22 != 1.0 {
        let inv = 1.0 / m22;
        a *= inv;
        b *= inv;
        tx *= inv;
        c *= inv;
        d *= inv;
        ty *= inv;
    }
    AffineXform {
        row0: [a, b, tx, 0.0],
        row1: [c, d, ty, 0.0],
    }
}

/// Parse a packed pair of i16 values (2 bytes each) from a byte slice.
pub(crate) fn read_i16_pair(data: &[u8], offset: usize) -> Option<(i16, i16)> {
    if offset + 4 > data.len() {
        return None;
    }
    let x = i16::from_le_bytes([data[offset], data[offset + 1]]);
    let y = i16::from_le_bytes([data[offset + 2], data[offset + 3]]);
    Some((x, y))
}

/// Parse a packed rectangle (x:i16, y:i16, w:u16, h:u16) from a byte slice.
pub(crate) fn read_rect(data: &[u8], offset: usize) -> Option<Rectangle16> {
    if offset + 8 > data.len() {
        return None;
    }
    let x = i16::from_le_bytes([data[offset], data[offset + 1]]);
    let y = i16::from_le_bytes([data[offset + 2], data[offset + 3]]);
    let w = u16::from_le_bytes([data[offset + 4], data[offset + 5]]);
    let h = u16::from_le_bytes([data[offset + 6], data[offset + 7]]);
    Some(Rectangle16 {
        x,
        y,
        width: w,
        height: h,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct OutputKey {
    pub(crate) device_key: crate::platform::drm::DrmDeviceKey,
    pub(crate) connector_name: String,
}

impl OutputKey {
    pub(crate) fn new(
        device_key: crate::platform::drm::DrmDeviceKey,
        connector_name: impl Into<String>,
    ) -> Self {
        Self {
            device_key,
            connector_name: connector_name.into(),
        }
    }
}

/// A single DRM output and its dedicated swapchain, positioned in the
/// virtual screen. The stable key keeps equal connector names on different
/// devices distinct. The render `PlatformBackend` owns one of these per
/// discovered output; `fb_w` / `fb_h` describe the virtual-screen
/// extent.
pub(crate) struct ActiveOutput {
    pub key: OutputKey,
    /// Renderer and KMS endpoints that own this live output's scanout route.
    /// This is qualified only after Vulkan selects its operational renderer;
    /// init-time dumb scanout uses [`PlatformInitOutput`] instead.
    pub scanout_route: ScanoutRoute,
    pub output: crate::platform::drm::Output,
    /// Kept alive for the lifetime of the output to retain initial-
    /// scanout buffer ownership; v2 has its own per-output
    /// `ScanoutBoPool` and doesn't read this field after construction.
    #[allow(dead_code)]
    pub swapchain: crate::drm::Swapchain,
    pub x: i32,
    pub y: i32,
    pub width: u16,
    pub height: u16,
}

impl ActiveOutput {
    pub(crate) fn new(
        scanout_route: ScanoutRoute,
        output: crate::platform::drm::Output,
        swapchain: crate::drm::Swapchain,
        x: i32,
        y: i32,
    ) -> Self {
        let key = OutputKey::new(scanout_route.kms_device_key, output.connector_name.clone());
        let width = output.picked.width;
        let height = output.picked.height;
        Self {
            key,
            scanout_route,
            output,
            swapchain,
            x,
            y,
            width,
            height,
        }
    }
}

/// Output committed during platform discovery, before Vulkan has selected a
/// renderer endpoint. It is converted into [`ActiveOutput`] only after a
/// truthful [`ScanoutRoute`] can be constructed.
pub(crate) struct PlatformInitOutput {
    pub key: OutputKey,
    pub output: crate::platform::drm::Output,
    pub swapchain: crate::drm::Swapchain,
    pub x: i32,
    pub y: i32,
    pub width: u16,
    pub height: u16,
}

impl PlatformInitOutput {
    fn new(
        device_key: crate::platform::drm::DrmDeviceKey,
        output: crate::platform::drm::Output,
        swapchain: crate::drm::Swapchain,
        x: i32,
        y: i32,
    ) -> Self {
        let key = OutputKey::new(device_key, output.connector_name.clone());
        let width = output.picked.width;
        let height = output.picked.height;
        Self {
            key,
            output,
            swapchain,
            x,
            y,
            width,
            height,
        }
    }

    /// Attach the post-Vulkan renderer identity without reopening, cloning, or
    /// otherwise disturbing the already-committed dumb scanout resources.
    pub(crate) fn qualify(self, scanout_route: ScanoutRoute) -> ActiveOutput {
        debug_assert_eq!(self.key.device_key, scanout_route.kms_device_key);
        ActiveOutput {
            key: self.key,
            scanout_route,
            output: self.output,
            swapchain: self.swapchain,
            x: self.x,
            y: self.y,
            width: self.width,
            height: self.height,
        }
    }
}

/// Centre of the primary output (output 0) — the startup pointer position,
/// matching Xorg which warps the pointer to the centre of the first display.
/// On a multi-head layout the framebuffer centre lands on the seam between
/// monitors, so we centre on output 0 instead. Falls back to the framebuffer
/// centre when no outputs are known.
pub(crate) fn primary_output_center(outputs: &[ActiveOutput], fb_w: u16, fb_h: u16) -> (i32, i32) {
    outputs.first().map_or_else(
        || (i32::from(fb_w) / 2, i32::from(fb_h) / 2),
        |o| (o.x + i32::from(o.width) / 2, o.y + i32::from(o.height) / 2),
    )
}

/// One DRM/KMS device opened during platform bring-up.
pub(crate) struct PlatformInitDevice {
    pub(crate) key: crate::platform::drm::DrmDeviceKey,
    pub(crate) device: Rc<drm::Device>,
}

/// Transient handoff from device discovery to the long-lived renderer.
///
/// The ordered device vector carries every successfully opened KMS card. Only
/// the first entry receives startup scanout in this incremental PRIME step;
/// secondary entries exist for provider identity and later device routing.
/// Empty `devices` and `layouts` is a valid zero-card headless platform.
pub(crate) struct PlatformInit {
    pub(crate) devices: Vec<PlatformInitDevice>,
    /// Selected render endpoint, separate from the KMS-device inventory.
    /// `None` is valid for headless startup or unavailable DRI3.
    pub(crate) render_node: Option<crate::kms::render_node::OpenedRenderNode>,
    pub(crate) layouts: Vec<PlatformInitOutput>,
    pub(crate) fb_w: u16,
    pub(crate) fb_h: u16,
    pub(crate) input_ctx: Option<crate::input::SendContext>,
}

fn render_node_for_device(
    device_path: &str,
    device: &drm::Device,
) -> Option<crate::kms::render_node::OpenedRenderNode> {
    match crate::kms::render_node::open_for_card(device) {
        Ok(render_node) => {
            log::info!(
                "DRI3 render node ready for {device_path}: fd={} path={:?} rdev={} \
                 (render node minor should be >=128)",
                render_node.raw_fd(),
                render_node.path(),
                render_node.key(),
            );
            Some(render_node)
        }
        Err(err) => {
            log::warn!(
                "DRI3 render node unavailable for selected display {device_path}: {err}; \
                 renderer selection may continue only through the explicit unverified fallback \
                 when no suitable Vulkan device exposes VK_EXT_physical_device_drm"
            );
            None
        }
    }
}

fn activate_initial_scanout_outputs(
    device_key: crate::platform::drm::DrmDeviceKey,
    device: &Rc<drm::Device>,
    commit: fn(
        &crate::drm::Device,
        &crate::platform::drm::Output,
        ::drm::control::framebuffer::Handle,
    ) -> io::Result<()>,
) -> io::Result<Vec<PlatformInitOutput>> {
    let outputs = crate::platform::drm::discover_outputs(device)?;
    if !outputs.is_empty() {
        // Refuse software-only Vulkan before allocating or committing any
        // scanout buffer. An opened card with no connected outputs remains a
        // valid headless platform; later RANDR enable repeats this guard.
        crate::kms::vk::device::ensure_hardware_vulkan_for_scanout().map_err(io::Error::other)?;
    }

    // Horizontal layout in connector order. If anything fails part way
    // through bring-up, disable everything already committed so the caller
    // starts from a clean slate.
    let mut layouts = Vec::with_capacity(outputs.len());
    let mut next_x = 0_i32;
    let mut bring_up_err = None;
    for output in outputs {
        let w = output.picked.width;
        let h = output.picked.height;
        let mut buffers = Vec::with_capacity(2);
        let mut buffer_err = None;
        for _ in 0..2 {
            match drm::Buffer::new(Rc::clone(device), w, h) {
                Ok(buffer) => buffers.push(buffer),
                Err(err) => {
                    buffer_err = Some(err);
                    break;
                }
            }
        }
        if let Some(err) = buffer_err {
            bring_up_err = Some(err);
            break;
        }
        let initial_fb = buffers[0].fb_id();
        if let Err(err) = commit(device, &output, initial_fb) {
            bring_up_err = Some(err);
            break;
        }
        let swapchain = drm::Swapchain::with_initial_scanout(buffers, 0);
        layouts.push(PlatformInitOutput::new(
            device_key, output, swapchain, next_x, 0,
        ));
        next_x = next_x.saturating_add(i32::from(w));
    }
    if let Some(err) = bring_up_err {
        for done in layouts.iter_mut().rev() {
            if let Err(disable_err) = drm::modeset::disable_output(device, &done.output) {
                log::warn!(
                    "initial scanout rollback: failed to disable {}: {disable_err}; \
                     leaving its buffers for DRM-fd close",
                    done.output.connector_name,
                );
                done.swapchain.disarm();
            }
        }
        return Err(err);
    }
    Ok(layouts)
}

/// Reject two paths that resolve to the same kernel DRM device identity.
fn validate_unique_kms_device_identity(
    opened: &[(crate::platform::drm::DrmDeviceKey, PathBuf)],
    key: crate::platform::drm::DrmDeviceKey,
    path: &Path,
) -> io::Result<()> {
    let Some((_, existing_path)) = opened.iter().find(|(existing, _)| *existing == key) else {
        return Ok(());
    };
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "DRM device {} aliases already-opened device {} (same rdev {key})",
            path.display(),
            existing_path.display(),
        ),
    ))
}

/// Shared DRM / outputs / libinput bring-up for the v1 and v2
/// backends. Extracted in Stage 1b so both `KmsBackend::open_with_commit`
/// and `KmsBackend::open` use the same code path.
///
/// **Vulkan / pipelines / scanout pools / scheduler / pixmap pool**
/// stay in the v1-specific portion of `open_with_commit` for now —
/// v2 doesn't build any of that in Stage 1b (paint paths are
/// stubbed). Stage 2 promotes the appropriate subset into the
/// real `PlatformBackend` component.
///
/// # Errors
///
/// Individual DRM-open failures are logged and skipped. Once a device opens,
/// identity/discovery/commit failures remain fatal. If no device opens, the
/// returned platform is headless. On bring-up error any output already
/// committed gets disabled before returning so the next caller starts clean.
pub(crate) fn platform_init(
    device_paths: &[PathBuf],
    commit: fn(
        &crate::drm::Device,
        &crate::platform::drm::Output,
        ::drm::control::framebuffer::Handle,
    ) -> io::Result<()>,
) -> io::Result<PlatformInit> {
    let mut devices = Vec::with_capacity(device_paths.len());
    let mut opened_device_paths = Vec::with_capacity(device_paths.len());
    let mut render_node = None;
    let mut layouts = Vec::new();
    let mut open_errors = Vec::new();
    for device_path in device_paths {
        let device_path_str = device_path.to_string_lossy().into_owned();
        let device = match drm::Device::open(&device_path_str) {
            Ok(device) => Rc::new(device),
            Err(err) => {
                log::warn!(
                    "yserver: skipping DRM device {}: open failed: {err}",
                    device_path.display()
                );
                open_errors.push(format!("{}: open failed: {err}", device_path.display()));
                continue;
            }
        };
        let device_key =
            crate::platform::drm::primary_device_key_from_fd(std::os::fd::AsFd::as_fd(&*device))?;
        validate_unique_kms_device_identity(&opened_device_paths, device_key, device_path)?;
        opened_device_paths.push((device_key, device_path.clone()));

        if devices.is_empty() {
            render_node = render_node_for_device(&device_path_str, &device);
        }
        if !devices.is_empty() {
            log::info!(
                "yserver: opened secondary KMS device {} as provider/topology data; \
                 initial scanout remains on the first opened device",
                device_path.display()
            );
        }
        devices.push(PlatformInitDevice {
            key: device_key,
            device,
        });
    }

    // Finish every fallible per-device identity qualification before the
    // first modeset. Otherwise a bad secondary card could abort startup after
    // the primary was already scanning out, bypassing the renderer's
    // construction-wide rollback guard.
    if let Some(primary) = devices.first() {
        layouts = activate_initial_scanout_outputs(primary.key, &primary.device, commit)?;
    }

    if devices.is_empty() {
        if open_errors.is_empty() {
            log::info!("platform_init: no DRM devices supplied; starting headless");
        } else {
            log::warn!(
                "platform_init: no DRM devices opened; starting headless. Tried:\n  {}",
                open_errors.join("\n  ")
            );
        }
    }

    // fb_w / fb_h carry the virtual-screen extent. Saturating
    // cast: huge layouts that exceed u16 are clamped — the rest
    // of the backend assumes u16 framebuffer dims.
    let fb_w: u16 = layouts
        .iter()
        .map(|l| u16::try_from(l.x.saturating_add(i32::from(l.width))).unwrap_or(u16::MAX))
        .max()
        .unwrap_or(0);
    let fb_h: u16 = layouts
        .iter()
        .map(|l| u16::try_from(l.y.saturating_add(i32::from(l.height))).unwrap_or(u16::MAX))
        .max()
        .unwrap_or(0);

    let input_ctx = match crate::input::SendContext::new() {
        Ok(ctx) => Some(ctx),
        Err(err) => {
            // Note: not a decision point. `run()` treats a missing context as
            // fatal and refuses to start (see `input_startup_action`).
            log::warn!("libinput SendContext unavailable: {err}");
            None
        }
    };

    Ok(PlatformInit {
        devices,
        render_node,
        layouts,
        fb_w,
        fb_h,
        input_ctx,
    })
}

#[cfg(test)]
mod platform_init_tests {
    use super::*;

    fn unused_commit(
        _device: &crate::drm::Device,
        _output: &crate::platform::drm::Output,
        _fb: ::drm::control::framebuffer::Handle,
    ) -> io::Result<()> {
        unreachable!("a zero-device platform must not commit an output")
    }

    #[test]
    fn duplicate_kms_identity_rejects_distinct_alias_paths() {
        let key = crate::platform::drm::DrmDeviceKey {
            major: 226,
            minor: 1,
        };
        let opened = [(key, PathBuf::from("/dev/dri/card1"))];

        validate_unique_kms_device_identity(
            &opened,
            crate::platform::drm::DrmDeviceKey {
                major: 226,
                minor: 0,
            },
            Path::new("/dev/dri/card0"),
        )
        .expect("a distinct DRM identity is allowed");

        let error = validate_unique_kms_device_identity(
            &opened,
            key,
            Path::new("/dev/dri/by-path/pci-0000:01:00.0-card"),
        )
        .expect_err("two paths naming the same DRM identity must fail");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        let message = error.to_string();
        assert!(message.contains("/dev/dri/card1"));
        assert!(message.contains("/dev/dri/by-path/pci-0000:01:00.0-card"));
        assert!(message.contains("226:1"));
    }

    #[test]
    fn accepts_empty_device_list_as_headless() {
        let init = platform_init(&[], unused_commit)
            .expect("an empty DRM device list should produce a headless platform");
        assert!(init.devices.is_empty());
        assert!(init.layouts.is_empty());
        assert_eq!((init.fb_w, init.fb_h), (0, 0));
    }

    #[test]
    fn accepts_all_open_failures_as_headless() {
        let suffix = std::process::id();
        let paths = [
            std::env::temp_dir().join(format!("yserver-missing-drm-{suffix}-a")),
            std::env::temp_dir().join(format!("yserver-missing-drm-{suffix}-b")),
        ];
        let init = platform_init(&paths, unused_commit)
            .expect("missing DRM paths should produce a headless platform");
        assert!(init.devices.is_empty());
        assert!(init.layouts.is_empty());
        assert_eq!((init.fb_w, init.fb_h), (0, 0));
    }
}

/// Parse an AddGlyphs `body_tail` and insert glyphs into `gs`.
/// `body_tail` is everything after the 4-byte glyphset XID.
pub(crate) fn parse_add_glyphs(gs: &mut GlyphSetState, body_tail: &[u8]) {
    if !matches!(
        gs.format,
        GlyphSetFormat::A8 | GlyphSetFormat::A1 | GlyphSetFormat::Argb32
    ) {
        log::debug!(
            "parse_add_glyphs bail: format={:?} (only A8/A1/ARGB32 supported) — {} glyphs lost",
            gs.format,
            body_tail
                .get(..4)
                .map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        );
        return;
    }
    if body_tail.len() < 4 {
        return;
    }
    let n = u32::from_le_bytes([body_tail[0], body_tail[1], body_tail[2], body_tail[3]]) as usize;
    let ids_end = 4 + n * 4;
    let infos_end = ids_end + n * 12;
    if body_tail.len() < infos_end {
        return;
    }

    let id_chunks = body_tail[4..ids_end].chunks_exact(4);
    let info_chunks = body_tail[ids_end..infos_end].chunks_exact(12);
    let mut data_off = infos_end;

    for (id_b, info_b) in id_chunks.zip(info_chunks) {
        let id = u32::from_le_bytes([id_b[0], id_b[1], id_b[2], id_b[3]]);
        let width = u16::from_le_bytes([info_b[0], info_b[1]]);
        let height = u16::from_le_bytes([info_b[2], info_b[3]]);
        let x = i16::from_le_bytes([info_b[4], info_b[5]]);
        let y = i16::from_le_bytes([info_b[6], info_b[7]]);
        let x_off = i16::from_le_bytes([info_b[8], info_b[9]]);
        let y_off = i16::from_le_bytes([info_b[10], info_b[11]]);

        let w = width as usize;
        let h = height as usize;
        let stride = match gs.format {
            GlyphSetFormat::A8 => (w + 3) & !3,
            GlyphSetFormat::A1 => w.div_ceil(32) * 4,
            // CARD32 per pixel; row size always 4-aligned, no per-row pad.
            GlyphSetFormat::Argb32 => w * 4,
            GlyphSetFormat::Other => return,
        };
        let nbytes = stride * h;
        if data_off + nbytes > body_tail.len() {
            break;
        }
        let wire = &body_tail[data_off..data_off + nbytes];
        // For ARGB32 we extract the alpha byte from each pixel into a
        // densely-packed A8 buffer and record the stored glyph as A8.
        // The downstream atlas + text pipeline path then handles it
        // identically to a real A8 upload.
        let (pixels, stored_format) = match gs.format {
            GlyphSetFormat::A8 => {
                let mut pixels = vec![0u8; w * h];
                for row in 0..h {
                    pixels[row * w..row * w + w]
                        .copy_from_slice(&wire[row * stride..row * stride + w]);
                }
                (pixels, GlyphSetFormat::A8)
            }
            GlyphSetFormat::A1 => (wire.to_vec(), GlyphSetFormat::A1),
            GlyphSetFormat::Argb32 => {
                // Pixel bytes per X RENDER ARGB32 = little-endian
                // CARD32 with alpha-shift=24 → memory order [B, G, R, A].
                let mut pixels = vec![0u8; w * h];
                for row in 0..h {
                    let row_off = row * stride;
                    for col in 0..w {
                        pixels[row * w + col] = wire[row_off + col * 4 + 3];
                    }
                }
                (pixels, GlyphSetFormat::A8)
            }
            GlyphSetFormat::Other => return,
        };
        data_off += nbytes;
        gs.glyphs.insert(
            id,
            StoredGlyph {
                width,
                height,
                x,
                y,
                x_off,
                y_off,
                pixels,
                format: stored_format,
            },
        );
    }
}

#[cfg(test)]
mod depth1_mask_cache_tests {
    use super::Depth1MaskCache;
    use crate::kms::render::store::DrawableId;

    #[test]
    fn hit_only_on_matching_content_version() {
        let mut c = Depth1MaskCache::new(8);
        let id = DrawableId::for_tests(1);
        assert!(c.get(id, 0, 4, 4).is_none(), "empty cache misses");

        c.insert(id, 5, 4, 4, vec![0xAA; 16]);
        // Same version + extent -> hit with the stored bytes.
        assert_eq!(c.get(id, 5, 4, 4), Some((4, 4, vec![0xAA; 16])));
        // A newer content_version (the mask was drawn into) -> miss,
        // forcing a fresh readback.
        assert!(c.get(id, 6, 4, 4).is_none(), "stale version must miss");
        // Same version but a different extent -> miss (belt-and-suspenders
        // guard against handing back wrongly-sized bytes).
        assert!(c.get(id, 5, 8, 8).is_none(), "extent mismatch must miss");
    }

    #[test]
    fn reinsert_updates_bytes_without_growing() {
        let mut c = Depth1MaskCache::new(8);
        let id = DrawableId::for_tests(1);
        c.insert(id, 1, 2, 2, vec![0x00; 4]);
        c.insert(id, 2, 2, 2, vec![0xFF; 4]);
        assert_eq!(c.len(), 1, "same id must not duplicate");
        assert!(c.get(id, 1, 2, 2).is_none(), "old version gone");
        assert_eq!(c.get(id, 2, 2, 2), Some((2, 2, vec![0xFF; 4])));
    }

    #[test]
    fn bounded_lru_evicts_least_recently_used() {
        let mut c = Depth1MaskCache::new(2);
        let (a, b, d) = (
            DrawableId::for_tests(1),
            DrawableId::for_tests(2),
            DrawableId::for_tests(3),
        );
        c.insert(a, 1, 1, 1, vec![1]);
        c.insert(b, 1, 1, 1, vec![2]);
        // Touch `a` so `b` becomes the LRU victim.
        assert!(c.get(a, 1, 1, 1).is_some());
        c.insert(d, 1, 1, 1, vec![3]);
        assert_eq!(c.len(), 2, "cap enforced");
        assert!(c.get(b, 1, 1, 1).is_none(), "LRU entry b evicted");
        assert!(c.get(a, 1, 1, 1).is_some(), "recently-used a survives");
        assert!(c.get(d, 1, 1, 1).is_some(), "newest d survives");
    }
}
