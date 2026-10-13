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
//! - `clip_rects_to_image`, consumed by v2's poly_line / poly_segment /
//!   poly_arc / fill_poly lowering. The other rasterisation and RENDER
//!   affine helpers live in `kms::render::geometry`; `scanline_fill_polygon`
//!   is re-exported here.
//! - SHAPE / clip-mask helpers (`ClipMaskCache`,
//!   `rasterize_pixmap_mask_to_rects`) consumed by v2's GC clip path.
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

pub(crate) use crate::kms::render::geometry::scanline_fill_polygon;
use crate::{
    drm,
    kms::{
        core::{GlyphSetFormat, GlyphSetState, StoredGlyph},
        cpu_types::Rectangle16,
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

/// CPU cache of #137 tier-1 uniform glyph-source colours: the
/// premultiplied `[f32; 4]` a 1x1 drawable source collapsed to, keyed by
/// the drawable it was read from.
///
/// **The read is not what costs.** Measured on a deliberately worst-case
/// synthetic churn workload (32 strings recoloured every 10ms), the
/// pixel copy-out itself is 1.5 ms/s — noise. What costs is that
/// [`RenderEngine::get_image`](crate::kms::render::engine::RenderEngine::get_image)
/// must close any open frame
/// ([`CloseReason::SyncWait`](crate::kms::render::frame_builder::CloseReason::SyncWait))
/// before it can wait on its readback fence, and that close destroys the
/// batching `composite_glyphs_via_frame_builder` exists to provide:
/// `frame_builder_opens=237 closes=238` per second with
/// `close_reasons[sync_wait=179]` — ~75% of ALL frame closes were this
/// one readback — and `ops/frame_avg` collapsed to 1.6.
///
/// A hit skips `get_image` entirely and therefore skips the frame close.
/// That, not the copy, is what this buys.
///
/// # Keying — all three components of the spec's invariant 6
///
/// A hit requires `(DrawableId, content_version, offset)` to match, and
/// each component is load-bearing:
///
/// - **`DrawableId`** — minted monotonically and never recycled, so a
///   freed-and-reallocated pixmap always gets a fresh id and cannot
///   alias a stale entry. `content_version` alone would collide across
///   drawables: it is local to an allocation, so two unrelated 1x1
///   sources both sitting at version 3 would share one colour.
/// - **`content_version`** (`store.rs`, bumped by every pixel write) —
///   this is the staleness guard, and it is the whole reason population
///   happens only AFTER the ordered read. Java's `XRSolidSrcPict`
///   repaints the SAME 1x1 pixmap to change text colour and then reuses
///   the picture, so a value cached at `CreatePicture` time — or keyed
///   on anything that does not move when the pixels do — renders every
///   later run of text in the PREVIOUS colour. That converts a total,
///   obvious failure into an intermittent wrong-colour one, which is
///   strictly worse than the bug being fixed (spec invariant 4).
/// - **`offset`** — one backing can legitimately be sampled at several
///   content offsets at the same version (a redirected window's 1x1
///   content domain inside a larger allocation), and those are different
///   pixels.
///
/// Following [`Depth1MaskCache`]'s shape, the map key carries identity +
/// offset while `content_version` is validated inside the entry. That is
/// the same triple, and it has one property a three-part map key does
/// not: only ONE version of a given `(id, offset)` is ever live, so an
/// insert REPLACES its predecessor instead of stranding it. Keying the
/// map on a monotonically increasing version would accumulate a dead
/// entry per repaint — a leak in all but name, and exactly the shape
/// Java's recolour-per-string pattern produces fastest.
///
/// Bounded LRU on top of that, so sources of long-gone pixmaps cannot
/// accumulate and no free-path hook is needed.
pub(crate) struct UniformGlyphSourceCache {
    entries: std::collections::HashMap<UniformGlyphSourceKey, UniformGlyphSourceEntry>,
    /// LRU order, back = most-recently-used. `touch` removes any prior
    /// occurrence before pushing, so this stays duplicate-free and
    /// `entries.len() == order.len()` holds.
    order: std::collections::VecDeque<UniformGlyphSourceKey>,
    cap: usize,
}

/// Identity + sampled origin. The version lives in the entry; see
/// [`UniformGlyphSourceCache`] for why it is not part of the map key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct UniformGlyphSourceKey {
    id: crate::kms::render::store::DrawableId,
    /// `SourceDrawable::offset()`, already resolved into backing space.
    offset: (i32, i32),
}

struct UniformGlyphSourceEntry {
    content_version: u64,
    premul: [f32; 4],
}

impl UniformGlyphSourceCache {
    /// `cap` = max distinct `(drawable, offset)` sources retained
    /// (clamped to >= 1). A client typically holds exactly one such
    /// pixmap (Java's per-`GraphicsConfiguration` `XRSolidSrcPict`), and
    /// each entry is a key plus four floats, so this is generous.
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            entries: std::collections::HashMap::new(),
            order: std::collections::VecDeque::new(),
            cap: cap.max(1),
        }
    }

    /// Hit iff we hold this `(id, offset)` at the SAME `content_version`.
    /// A hit lets the caller skip the ordered readback, and with it the
    /// frame close that readback forces.
    pub(crate) fn get(
        &mut self,
        id: crate::kms::render::store::DrawableId,
        content_version: u64,
        offset: (i32, i32),
    ) -> Option<[f32; 4]> {
        let key = UniformGlyphSourceKey { id, offset };
        let premul = match self.entries.get(&key) {
            Some(e) if e.content_version == content_version => e.premul,
            _ => return None,
        };
        self.touch(key);
        Some(premul)
    }

    /// Insert (or replace) this source's colour and evict LRU victims
    /// past `cap`. Called ONLY after the ordered read has returned the
    /// pixel — never at picture-create time.
    pub(crate) fn insert(
        &mut self,
        id: crate::kms::render::store::DrawableId,
        content_version: u64,
        offset: (i32, i32),
        premul: [f32; 4],
    ) {
        let key = UniformGlyphSourceKey { id, offset };
        self.entries.insert(
            key,
            UniformGlyphSourceEntry {
                content_version,
                premul,
            },
        );
        self.touch(key);
        while self.entries.len() > self.cap {
            match self.order.pop_front() {
                Some(victim) => {
                    self.entries.remove(&victim);
                }
                None => break,
            }
        }
    }

    /// Move `key` to the MRU end, removing any stale position first so
    /// `order` never holds duplicates.
    fn touch(&mut self, key: UniformGlyphSourceKey) {
        if let Some(pos) = self.order.iter().position(|&x| x == key) {
            self.order.remove(pos);
        }
        self.order.push_back(key);
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
        // Store each glyph in the format it arrived in. A8 is
        // densified (its wire rows are padded to 4 bytes); A1 and
        // ARGB32 keep their wire bytes verbatim, and the conversion
        // to the atlas's A8 coverage plane happens on an atlas MISS
        // in the engine (`GlyphPixels::to_a8`), so a resident glyph
        // is never converted twice.
        //
        // ARGB32 in particular must NOT be reduced here: it used to
        // be flattened to its alpha byte and recorded as A8, which is
        // what made subpixel-antialiased text render as solid blocks
        // (a subpixel-AA client leaves alpha at 255 across the whole
        // glyph box and puts the coverage in R, G and B). Keeping the
        // four channels also leaves the component-alpha path able to
        // pack all of them later.
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
            // Memory order per pixel is [B, G, R, A] — X RENDER
            // `PICT_a8r8g8b8` is a little-endian CARD32 with
            // alpha-shift 24, so blue is the LOW byte. Rows are
            // dense at `4 * w`, which is already 4-aligned.
            GlyphSetFormat::Argb32 => (wire.to_vec(), GlyphSetFormat::Argb32),
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
mod parse_add_glyphs_tests {
    use super::parse_add_glyphs;
    use crate::kms::{
        core::{GlyphSetFormat, GlyphSetState},
        render::glyph_pixels::reduce_argb32_glyph_to_a8_coverage,
    };
    use std::collections::HashMap;

    /// Build an `AddGlyphs` `body_tail` for a single glyph: the
    /// bytes after the 4-byte glyphset XID.
    fn one_glyph_body(w: u16, h: u16, pixels: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&1u32.to_le_bytes()); // n = 1
        body.extend_from_slice(&7u32.to_le_bytes()); // glyph id
        body.extend_from_slice(&w.to_le_bytes());
        body.extend_from_slice(&h.to_le_bytes());
        body.extend_from_slice(&0i16.to_le_bytes()); // x bearing
        body.extend_from_slice(&0i16.to_le_bytes()); // y bearing
        body.extend_from_slice(&i16::try_from(w).unwrap_or(0).to_le_bytes()); // x_off
        body.extend_from_slice(&0i16.to_le_bytes()); // y_off
        body.extend_from_slice(pixels);
        body
    }

    fn ingest(format: GlyphSetFormat, w: u16, h: u16, pixels: &[u8]) -> GlyphSetState {
        let mut gs = GlyphSetState {
            format,
            glyphs: HashMap::new(),
        };
        parse_add_glyphs(&mut gs, &one_glyph_body(w, h, pixels));
        gs
    }

    #[test]
    fn a8_glyphs_are_densified_and_stay_a8() {
        // Invariant 1: the common path must not move. A8 wire rows
        // are padded to 4 bytes; ingest strips the pad and nothing
        // else.
        let wire = [1u8, 2, 3, 0xEE, 4, 5, 6, 0xEE];
        let gs = ingest(GlyphSetFormat::A8, 3, 2, &wire);
        let g = gs.glyphs.get(&7).expect("glyph stored");
        assert_eq!(g.format, GlyphSetFormat::A8);
        assert_eq!(g.pixels, vec![1, 2, 3, 4, 5, 6], "row pad stripped");
    }

    #[test]
    fn a1_glyphs_keep_their_wire_bytes_and_stay_a1() {
        // Invariant 1 again: A1 is forwarded verbatim, expanded
        // later on atlas miss.
        let wire = [0b0101_0011u8, 0, 0, 0, 0b1000_0001, 0, 0, 0];
        let gs = ingest(GlyphSetFormat::A1, 8, 2, &wire);
        let g = gs.glyphs.get(&7).expect("glyph stored");
        assert_eq!(g.format, GlyphSetFormat::A1);
        assert_eq!(g.pixels, wire.to_vec(), "A1 wire bytes untouched");
    }

    #[test]
    fn argb32_glyphs_keep_all_four_channels_and_stay_argb32() {
        // The defect: ingest used to keep only the alpha byte and
        // record the glyph as A8. On a subpixel-AA glyph alpha is
        // 255 over the whole box, so the stored glyph was a solid
        // block. Wire memory order is [B, G, R, A].
        let wire = [
            0x20u8, 0x60, 0xA0, 0xE0, // (0,0) B,G,R,A
            0x10, 0x40, 0x50, 0xFF, // (1,0)
            0x00, 0x00, 0x00, 0xFF, // (0,1)
            0xFF, 0xFF, 0xFF, 0xFF, // (1,1)
        ];
        let gs = ingest(GlyphSetFormat::Argb32, 2, 2, &wire);
        let g = gs.glyphs.get(&7).expect("glyph stored");
        assert_eq!(
            g.format,
            GlyphSetFormat::Argb32,
            "the stored format must stay ARGB32; rewriting it to A8 \
             is what discarded the colour channels"
        );
        assert_eq!(g.pixels, wire.to_vec(), "all four channels retained");

        // And the deferred reduction turns those bytes into real
        // coverage rather than the alpha byte's solid 0xFF.
        assert_eq!(
            reduce_argb32_glyph_to_a8_coverage(&g.pixels, 2, 2),
            vec![96u8, 53, 0, 255],
            "coverage = mean of logical R, G, B"
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

/// #137 step 5 — the collision cases the `(DrawableId, content_version,
/// offset)` key exists to prevent. None of them is visible without an
/// explicit test: every one of them looks like a working cache until the
/// wrong colour comes out.
#[cfg(test)]
mod uniform_glyph_source_cache_tests {
    use super::UniformGlyphSourceCache;
    use crate::kms::render::store::DrawableId;

    const RED: [f32; 4] = [1.0, 0.0, 0.0, 1.0];
    const BLUE: [f32; 4] = [0.0, 0.0, 1.0, 1.0];

    /// Spec invariant 4: a repaint bumps `content_version`, and the
    /// entry stored at the old version must NOT satisfy the new read.
    /// This is the staleness trap — Java repaints this very pixmap to
    /// change text colour and reuses the picture.
    #[test]
    fn a_bumped_content_version_misses() {
        let mut c = UniformGlyphSourceCache::new(8);
        let id = DrawableId::for_tests(1);
        assert_eq!(c.get(id, 0, (0, 0)), None, "empty cache misses");

        c.insert(id, 5, (0, 0), RED);
        assert_eq!(c.get(id, 5, (0, 0)), Some(RED), "same version hits");
        assert_eq!(
            c.get(id, 6, (0, 0)),
            None,
            "a repainted source must force a fresh ordered read"
        );
    }

    /// `content_version` is local to an allocation, so two unrelated
    /// drawables routinely sit at the same version. Keying on the
    /// version alone would hand drawable 2 drawable 1's colour.
    #[test]
    fn two_drawables_at_the_same_content_version_do_not_share_an_entry() {
        let mut c = UniformGlyphSourceCache::new(8);
        let (a, b) = (DrawableId::for_tests(1), DrawableId::for_tests(2));
        c.insert(a, 3, (0, 0), RED);
        assert_eq!(
            c.get(b, 3, (0, 0)),
            None,
            "a different drawable at the same version must not hit"
        );
        c.insert(b, 3, (0, 0), BLUE);
        assert_eq!(c.get(a, 3, (0, 0)), Some(RED));
        assert_eq!(c.get(b, 3, (0, 0)), Some(BLUE));
    }

    /// One backing can legitimately be sampled at several content
    /// offsets at the same version — a redirected window's 1x1 content
    /// domain inside a larger allocation. Those are different pixels.
    #[test]
    fn two_offsets_into_one_backing_do_not_share_an_entry() {
        let mut c = UniformGlyphSourceCache::new(8);
        let id = DrawableId::for_tests(1);
        c.insert(id, 7, (0, 0), RED);
        assert_eq!(
            c.get(id, 7, (4, 2)),
            None,
            "a different sampled offset must not hit"
        );
        c.insert(id, 7, (4, 2), BLUE);
        assert_eq!(c.get(id, 7, (0, 0)), Some(RED));
        assert_eq!(c.get(id, 7, (4, 2)), Some(BLUE));
    }

    /// Only one version of a given `(id, offset)` is ever live, so a
    /// repaint REPLACES its predecessor rather than stranding it. A map
    /// keyed on the monotonically increasing version would leave a dead
    /// entry per repaint — a leak, and Java's recolour-per-string
    /// pattern produces it fastest.
    #[test]
    fn a_repaint_replaces_rather_than_accumulates() {
        let mut c = UniformGlyphSourceCache::new(8);
        let id = DrawableId::for_tests(1);
        for v in 1..=100 {
            c.insert(id, v, (0, 0), RED);
        }
        assert_eq!(c.len(), 1, "100 repaints must leave one entry");
    }

    #[test]
    fn bounded_lru_evicts_least_recently_used() {
        let mut c = UniformGlyphSourceCache::new(2);
        let (a, b, d) = (
            DrawableId::for_tests(1),
            DrawableId::for_tests(2),
            DrawableId::for_tests(3),
        );
        c.insert(a, 1, (0, 0), RED);
        c.insert(b, 1, (0, 0), BLUE);
        // Touch `a` so `b` becomes the LRU victim.
        assert_eq!(c.get(a, 1, (0, 0)), Some(RED));
        c.insert(d, 1, (0, 0), RED);
        assert_eq!(c.len(), 2, "cap enforced");
        assert_eq!(c.get(b, 1, (0, 0)), None, "LRU entry b evicted");
        assert_eq!(c.get(a, 1, (0, 0)), Some(RED), "recently-used a survives");
        assert_eq!(c.get(d, 1, (0, 0)), Some(RED), "newest d survives");
    }
}
