//! V2 glyph atlas (Stage 3a).
//!
//! Forks v1's [`crate::kms::vk::glyph::GlyphAtlas`] to drop the
//! persistent host-mapped staging buffer that the v1 atlas relied
//! on. v1 got away with reusing a single staging buffer because
//! every `intern` call submitted a one-shot upload CB and waited on
//! it via `queue_wait_idle` before the next `intern` could overwrite
//! the buffer. v2 doesn't wait on the hot path — each glyph upload
//! must own its own staging bytes for the lifetime of the CB it's
//! referenced by, otherwise back-to-back interns would have B's
//! memcpy land on A's still-pending GPU read and corrupt A's atlas
//! slot.
//!
//! Concretely, `GlyphAtlas`:
//!
//! - Owns the atlas image + view + memory + cache + shelf packer,
//!   identical to v1.
//! - Has NO persistent staging buffer. Callers (RenderEngine) build
//!   a one-shot `StagingBuffer` per glyph upload, hand it to
//!   `record_upload`, and park the buffer on the upload's
//!   `SubmittedOp` so it lives until the CB's `FenceTicket` retires.
//! - The shelf packer only moves forward. Space is reclaimed by
//!   [`ShelfPacker::reset`]: when a request's misses no longer fit,
//!   the engine closes the open frame and empties the atlas, and every
//!   glyph still in use re-uploads on its next draw. Dropping cache
//!   entries ([`ShelfPacker::forget`], [`ShelfPacker::forget_font`] on
//!   FreeGlyphs / FreeGlyphSet / CloseFont / a redefined glyph id) does
//!   not reuse their space by itself; it keeps stale images from being
//!   served and leaves the next reset less to repopulate.
//! - A glyph that cannot fit even an empty atlas is dropped (pen
//!   advances, no draw) and logged with [`ShelfPacker::note_dropped`],
//!   rate limited.

#![allow(
    dead_code,
    reason = "Stage 3a consumers (text + RENDER glyphs) wire up incrementally"
)]

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use ash::vk;

use crate::kms::vk::device::VkContext;
pub(crate) use crate::kms::vk::glyph::{AtlasEntry, GlyphKey};

/// Side length (px) of the fixed atlas allocation. 4096² R8 = 16 MiB.
pub(crate) const ATLAS_SIDE: u32 = 4096;

/// Minimum spacing of the dropped-glyph warning.
const DROP_WARN_INTERVAL: Duration = Duration::from_secs(10);

/// Pure-logic shelf packer + cache. Factored out of
/// [`GlyphAtlas`] so unit tests can exercise pack / cache
/// semantics without a live VkContext.
pub(crate) struct ShelfPacker {
    extent: vk::Extent2D,
    cache: HashMap<GlyphKey, AtlasEntry>,
    shelves: Vec<Shelf>,
    resets: u64,
    drops: u64,
    drops_since_warn: u64,
    last_drop_warn: Option<Instant>,
}

impl ShelfPacker {
    pub(crate) fn new(extent: vk::Extent2D) -> Self {
        Self {
            extent,
            cache: HashMap::new(),
            shelves: Vec::new(),
            resets: 0,
            drops: 0,
            drops_since_warn: 0,
            last_drop_warn: None,
        }
    }

    pub(crate) fn lookup(&self, key: GlyphKey) -> Option<AtlasEntry> {
        self.cache.get(&key).copied()
    }

    /// Pack a `w × h` glyph into the next available shelf
    /// position. Returns `Some((x, y))` on success or `None` when
    /// the atlas can't fit. Zero-area glyphs map to a degenerate
    /// `(0, 0)` slot (the caller doesn't upload them but may still
    /// want to cache an entry so subsequent pen-advance calls
    /// don't re-pack).
    pub(crate) fn pack(&mut self, w: u32, h: u32) -> Option<(u32, u32)> {
        pack_shelves(&mut self.shelves, self.extent, w, h)
    }

    /// Whether every `(w, h)` in `sizes` would pack, in order, into
    /// the atlas as it stands. Does not change the packer.
    pub(crate) fn fits(&self, sizes: &[(u32, u32)]) -> bool {
        let mut shelves = self.shelves.clone();
        sizes
            .iter()
            .all(|&(w, h)| pack_shelves(&mut shelves, self.extent, w, h).is_some())
    }

    /// Whether `(w, h)` fits an EMPTY atlas at all.
    pub(crate) fn fits_empty(&self, w: u32, h: u32) -> bool {
        w <= self.extent.width && h <= self.extent.height
    }

    pub(crate) fn insert_entry(&mut self, key: GlyphKey, entry: AtlasEntry) {
        self.cache.insert(key, entry);
    }

    /// Drop the cache entry for `key`. Returns whether it was cached.
    pub(crate) fn forget(&mut self, key: GlyphKey) -> bool {
        self.cache.remove(&key).is_some()
    }

    /// Drop every cache entry of glyphset / core font `font_xid`.
    /// Returns how many were cached.
    pub(crate) fn forget_font(&mut self, font_xid: u32) -> usize {
        let before = self.cache.len();
        self.cache.retain(|k, _| k.font_xid != font_xid);
        before - self.cache.len()
    }

    /// Empty the atlas: every cache entry and every shelf. The caller
    /// must have closed any open frame that recorded entries of the
    /// old layout (its pending inserts would land in the new one).
    pub(crate) fn reset(&mut self) {
        self.cache.clear();
        self.shelves.clear();
        self.resets += 1;
    }

    pub(crate) fn resets(&self) -> u64 {
        self.resets
    }

    /// Count a glyph dropped because it cannot be placed, and warn —
    /// the first time, then at most once per [`DROP_WARN_INTERVAL`]
    /// with the drops since the last warning. Returns whether it
    /// warned.
    pub(crate) fn note_dropped(&mut self, w: u32, h: u32, now: Instant) -> bool {
        self.drops += 1;
        self.drops_since_warn += 1;
        if self
            .last_drop_warn
            .is_some_and(|t| now.duration_since(t) < DROP_WARN_INTERVAL)
        {
            return false;
        }
        log::warn!(
            "render glyph atlas: dropped {} glyph(s) since the last warning ({} total); \
             latest {w}×{h} texels cannot be placed in the {}×{} atlas",
            self.drops_since_warn,
            self.drops,
            self.extent.width,
            self.extent.height,
        );
        self.drops_since_warn = 0;
        self.last_drop_warn = Some(now);
        true
    }

    pub(crate) fn cache_len(&self) -> usize {
        self.cache.len()
    }

    /// Texels below the shelves in use — the packer's high-water mark.
    pub(crate) fn rows_used(&self) -> u32 {
        self.shelves.last().map_or(0, |s| s.y_top + s.height)
    }
}

fn pack_shelves(
    shelves: &mut Vec<Shelf>,
    extent: vk::Extent2D,
    w: u32,
    h: u32,
) -> Option<(u32, u32)> {
    if w == 0 || h == 0 {
        return Some((0, 0));
    }
    if w > extent.width || h > extent.height {
        return None;
    }
    for shelf in shelves.iter_mut() {
        if shelf.height >= h && shelf.x_used + w <= extent.width {
            let x = shelf.x_used;
            let y = shelf.y_top;
            shelf.x_used += w;
            return Some((x, y));
        }
    }
    let next_y = shelves.last().map_or(0, |s| s.y_top + s.height);
    if next_y + h > extent.height {
        return None;
    }
    shelves.push(Shelf {
        y_top: next_y,
        height: h,
        x_used: w,
    });
    Some((0, next_y))
}

/// V2-side glyph atlas. Owns the atlas image; recording an upload
/// is the caller's job (they have the per-call staging buffer and
/// the engine-owned CB).
pub(crate) struct GlyphAtlas {
    vk: Arc<VkContext>,
    image: vk::Image,
    view: vk::ImageView,
    memory: vk::DeviceMemory,
    packer: ShelfPacker,
    /// Tracks the atlas image's current Vulkan layout. Starts
    /// `UNDEFINED`; flips to `SHADER_READ_ONLY_OPTIMAL` after the
    /// first upload. Subsequent uploads transition through
    /// `TRANSFER_DST_OPTIMAL` and back. Mutated by
    /// [`Self::record_upload`].
    current_layout: vk::ImageLayout,
    /// Stage 5 / Phase B.1: the `FenceTicket` of the most recent frame
    /// that touched the atlas image (uploaded a glyph or sampled it
    /// in a draw). `None` until the first frame-close-success.
    /// Destruction at backend shutdown waits on this ticket the same
    /// way `DrawableStore::poll_pending_retire` gates drawable
    /// destruction (engine drains `pending_frames` first; this field
    /// is the fallback for any path that bypasses the queue).
    last_render_ticket: Option<super::platform::FenceTicket>,
}

// Intentionally `!Send + !Sync`: `last_render_ticket` is backed by
// `Rc`/`Cell` state shared with other core-thread-owned render records.

#[derive(Debug, Clone, Copy)]
struct Shelf {
    y_top: u32,
    height: u32,
    x_used: u32,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum GlyphAtlasError {
    #[error("vulkan: {0:?}")]
    Vk(vk::Result),
    #[error("no memory type matches atlas requirements")]
    NoMemoryType,
}

impl From<vk::Result> for GlyphAtlasError {
    fn from(r: vk::Result) -> Self {
        GlyphAtlasError::Vk(r)
    }
}

impl GlyphAtlas {
    pub(crate) fn new(vk: Arc<VkContext>) -> Result<Self, GlyphAtlasError> {
        let extent = vk::Extent2D {
            width: ATLAS_SIDE,
            height: ATLAS_SIDE,
        };

        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::R8_UNORM)
            .extent(vk::Extent3D {
                width: extent.width,
                height: extent.height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image = unsafe { vk.device.create_image(&image_info, None)? };

        let mem_reqs = unsafe { vk.device.get_image_memory_requirements(image) };
        let mem_props = unsafe {
            vk.instance
                .get_physical_device_memory_properties(vk.physical_device)
        };
        let mt = (0..mem_props.memory_type_count).find(|&i| {
            mem_reqs.memory_type_bits & (1 << i) != 0
                && mem_props.memory_types[i as usize]
                    .property_flags
                    .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
        });
        let Some(mt) = mt else {
            unsafe { vk.device.destroy_image(image, None) };
            return Err(GlyphAtlasError::NoMemoryType);
        };
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_reqs.size)
            .memory_type_index(mt)
            .push_next(&mut dedicated);
        let memory = match crate::kms::vk::mem_accounting::allocate_memory(
            &vk.device,
            &alloc_info,
            crate::kms::vk::mem_accounting::MemCategory::Glyph,
            &mem_props,
        ) {
            Ok(m) => m,
            Err(e) => {
                unsafe { vk.device.destroy_image(image, None) };
                return Err(e.into());
            }
        };
        if let Err(e) = unsafe { vk.device.bind_image_memory(image, memory, 0) } {
            unsafe {
                crate::kms::vk::mem_accounting::free_memory(&vk.device, memory);
                vk.device.destroy_image(image, None);
            }
            return Err(e.into());
        }

        let view_info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(vk::Format::R8_UNORM)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            );
        let view = match unsafe { vk.device.create_image_view(&view_info, None) } {
            Ok(v) => v,
            Err(e) => {
                unsafe {
                    crate::kms::vk::mem_accounting::free_memory(&vk.device, memory);
                    vk.device.destroy_image(image, None);
                }
                return Err(e.into());
            }
        };

        Ok(Self {
            vk,
            image,
            view,
            memory,
            packer: ShelfPacker::new(extent),
            current_layout: vk::ImageLayout::UNDEFINED,
            last_render_ticket: None,
        })
    }

    pub(crate) fn image(&self) -> vk::Image {
        self.image
    }

    pub(crate) fn image_view(&self) -> vk::ImageView {
        self.view
    }

    pub(crate) fn extent(&self) -> vk::Extent2D {
        self.packer.extent
    }

    pub(crate) fn lookup(&self, key: GlyphKey) -> Option<AtlasEntry> {
        self.packer.lookup(key)
    }

    /// Allocate space in the atlas for a `w × h` glyph. Returns
    /// `Some((atlas_x, atlas_y))` on success and `None` when the
    /// atlas is full. Does NOT update the cache — caller commits
    /// the entry via [`Self::insert_entry`] after the upload CB has
    /// been recorded successfully.
    pub(crate) fn pack(&mut self, w: u32, h: u32) -> Option<(u32, u32)> {
        self.packer.pack(w, h)
    }

    /// See [`ShelfPacker::fits`].
    pub(crate) fn fits(&self, sizes: &[(u32, u32)]) -> bool {
        self.packer.fits(sizes)
    }

    /// See [`ShelfPacker::fits_empty`].
    pub(crate) fn fits_empty(&self, w: u32, h: u32) -> bool {
        self.packer.fits_empty(w, h)
    }

    /// See [`ShelfPacker::note_dropped`].
    pub(crate) fn note_dropped(&mut self, w: u32, h: u32) -> bool {
        self.packer.note_dropped(w, h, Instant::now())
    }

    /// See [`ShelfPacker::forget`].
    pub(crate) fn forget(&mut self, key: GlyphKey) -> bool {
        self.packer.forget(key)
    }

    /// See [`ShelfPacker::forget_font`].
    pub(crate) fn forget_font(&mut self, font_xid: u32) -> usize {
        self.packer.forget_font(font_xid)
    }

    /// See [`ShelfPacker::reset`]. The image keeps its layout and
    /// contents; slots are simply handed out again, and each re-upload
    /// is ordered after earlier samplers of the slot by the upload's
    /// own `ALL_COMMANDS → COPY` barrier (same queue, submission order).
    pub(crate) fn reset(&mut self) {
        let live = self.packer.cache_len();
        let rows = self.packer.rows_used();
        self.packer.reset();
        log::info!(
            "render glyph atlas full: reset #{} ({live} cached glyphs, {rows} of {} rows); \
             glyphs in use re-upload on their next draw",
            self.packer.resets(),
            self.packer.extent.height,
        );
    }

    pub(crate) fn resets(&self) -> u64 {
        self.packer.resets()
    }

    /// Commit a packed slot into the lookup cache.
    pub(crate) fn insert_entry(&mut self, key: GlyphKey, entry: AtlasEntry) {
        self.packer.insert_entry(key, entry);
    }

    pub(crate) fn set_last_render_ticket(&mut self, ticket: super::platform::FenceTicket) {
        self.last_render_ticket = Some(ticket);
    }

    pub(crate) fn clear_last_render_ticket(&mut self) {
        self.last_render_ticket = None;
    }

    pub(crate) fn last_render_ticket(&self) -> Option<&super::platform::FenceTicket> {
        self.last_render_ticket.as_ref()
    }

    /// Read-only view of the tracked atlas layout. Used by the
    /// FrameBuilder's append-time first-touch snapshot (Task 15)
    /// and the close-time commit/rollback (Task 12).
    pub(crate) fn current_layout(&self) -> vk::ImageLayout {
        self.current_layout
    }

    /// Mutator used by the FrameBuilder's close-success commit
    /// (sanity write-back) and close-failure rollback (restore
    /// pre_frame_layout). Not used by `record_upload`, which mutates
    /// the field directly through `&mut self`.
    pub(crate) fn set_current_layout(&mut self, layout: vk::ImageLayout) {
        self.current_layout = layout;
    }

    /// Record barriers + `vkCmdCopyBufferToImage` into `cb` that
    /// uploads `w × h` pixels of glyph data from `staging_buffer`
    /// (starting at `buffer_offset`, tightly packed) into the atlas
    /// image at `(atlas_x, atlas_y)`. `buffer_offset` must be a multiple
    /// of 4 and of the atlas texel size (VUID-vkCmdCopyBufferToImage-
    /// bufferOffset). Updates the tracked layout to
    /// `SHADER_READ_ONLY_OPTIMAL` on return.
    ///
    /// Caller is responsible for sequencing this on a CB whose
    /// staging buffer outlives the submission via the engine's
    /// `SubmittedOp` discipline.
    pub(crate) fn record_upload(
        &mut self,
        cb: vk::CommandBuffer,
        staging_buffer: vk::Buffer,
        buffer_offset: vk::DeviceSize,
        atlas_x: u32,
        atlas_y: u32,
        w: u32,
        h: u32,
    ) {
        let device = &self.vk.device;
        let old_layout = self.current_layout;

        let to_dst = [vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
            .src_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
            .dst_stage_mask(vk::PipelineStageFlags2::COPY)
            .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
            .old_layout(old_layout)
            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .image(self.image)
            .subresource_range(color_subresource_range())];
        let dep = vk::DependencyInfo::default().image_memory_barriers(&to_dst);
        crate::vk_count!(cmd_pipeline_barrier2);
        unsafe { device.cmd_pipeline_barrier2(cb, &dep) };

        let region = [vk::BufferImageCopy::default()
            .buffer_offset(buffer_offset)
            .buffer_row_length(0)
            .buffer_image_height(0)
            .image_subresource(
                vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .layer_count(1),
            )
            .image_offset(vk::Offset3D {
                #[allow(clippy::cast_possible_wrap)]
                x: atlas_x as i32,
                #[allow(clippy::cast_possible_wrap)]
                y: atlas_y as i32,
                z: 0,
            })
            .image_extent(vk::Extent3D {
                width: w,
                height: h,
                depth: 1,
            })];
        unsafe {
            crate::vk_count!(cmd_copy_buffer_to_image);
            device.cmd_copy_buffer_to_image(
                cb,
                staging_buffer,
                self.image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &region,
            );
        }

        let to_read = [vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::COPY)
            .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
            .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .image(self.image)
            .subresource_range(color_subresource_range())];
        let dep = vk::DependencyInfo::default().image_memory_barriers(&to_read);
        crate::vk_count!(cmd_pipeline_barrier2);
        unsafe { device.cmd_pipeline_barrier2(cb, &dep) };

        self.current_layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
    }

    pub(crate) fn cache_len(&self) -> usize {
        self.packer.cache_len()
    }
}

impl Drop for GlyphAtlas {
    fn drop(&mut self) {
        unsafe {
            // Best-effort: wait on the device's outstanding work
            // before destroying the atlas image. The atlas is held
            // across the engine's whole lifetime so this only
            // fires at backend shutdown. The RenderEngine's
            // drain_all walks `submitted` ahead of this Drop and
            // already retired any in-flight upload; `device_wait_idle`
            // here is the belt-and-braces guard.
            let _ = self.vk.device.queue_wait_idle(self.vk.graphics_queue);
            self.vk.device.destroy_image_view(self.view, None);
            self.vk.device.destroy_image(self.image, None);
            crate::kms::vk::mem_accounting::free_memory(&self.vk.device, self.memory);
        }
    }
}

fn color_subresource_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_size() -> vk::Extent2D {
        vk::Extent2D {
            width: 4096,
            height: 4096,
        }
    }

    #[test]
    fn pack_zero_area_returns_degenerate_slot() {
        let mut packer = ShelfPacker::new(full_size());
        let slot = packer.pack(0, 0);
        assert_eq!(slot, Some((0, 0)));
    }

    #[test]
    fn pack_fits_in_first_shelf() {
        let mut packer = ShelfPacker::new(full_size());
        assert_eq!(packer.pack(10, 16), Some((0, 0)));
        assert_eq!(packer.pack(8, 16), Some((10, 0)));
        // Different height opens a new shelf below.
        assert_eq!(packer.pack(20, 24), Some((0, 16)));
    }

    #[test]
    fn pack_returns_none_when_exhausted() {
        let mut packer = ShelfPacker::new(vk::Extent2D {
            width: 64,
            height: 32,
        });
        // First shelf height 16 — fits two 32×16 glyphs.
        assert!(packer.pack(32, 16).is_some());
        assert!(packer.pack(32, 16).is_some());
        // Second shelf height 16 — fits two more.
        assert!(packer.pack(32, 16).is_some());
        assert!(packer.pack(32, 16).is_some());
        // Third shelf would exceed extent — None.
        assert!(packer.pack(32, 16).is_none());
    }

    #[test]
    fn cache_round_trip() {
        let mut packer = ShelfPacker::new(full_size());
        let key = GlyphKey {
            font_xid: 7,
            codepoint: u32::from(b'A'),
        };
        assert!(packer.lookup(key).is_none());
        let (ax, ay) = packer.pack(8, 16).expect("pack ok");
        packer.insert_entry(
            key,
            AtlasEntry {
                atlas_x: ax,
                atlas_y: ay,
                packed_w: 8,
                logical_w: 8,
                h: 16,
                pen_left: 0,
                pen_top: 12,
                layout: crate::kms::vk::glyph::GlyphLayout::A8,
            },
        );
        let got = packer.lookup(key).expect("cache hit");
        assert_eq!(got.atlas_x, ax);
        assert_eq!(got.atlas_y, ay);
        assert_eq!(got.packed_w, 8);
        assert_eq!(got.logical_w, 8);
        assert_eq!(got.h, 16);
    }

    fn entry(atlas_x: u32, atlas_y: u32, w: u32, h: u32) -> AtlasEntry {
        AtlasEntry {
            atlas_x,
            atlas_y,
            packed_w: w,
            logical_w: w,
            h,
            pen_left: 0,
            pen_top: 0,
            layout: crate::kms::vk::glyph::GlyphLayout::A8,
        }
    }

    fn key(font_xid: u32, codepoint: u32) -> GlyphKey {
        GlyphKey {
            font_xid,
            codepoint,
        }
    }

    #[test]
    fn reset_reuses_the_whole_atlas() {
        let mut packer = ShelfPacker::new(vk::Extent2D {
            width: 64,
            height: 32,
        });
        for _ in 0..4 {
            packer.pack(32, 16).expect("fits");
        }
        packer.insert_entry(key(1, 1), entry(0, 0, 32, 16));
        assert!(packer.pack(32, 16).is_none());
        packer.reset();
        assert_eq!(packer.resets(), 1);
        assert_eq!(packer.cache_len(), 0, "reset drops every entry");
        assert!(packer.lookup(key(1, 1)).is_none());
        assert_eq!(
            packer.pack(32, 16),
            Some((0, 0)),
            "slots are handed out again"
        );
    }

    #[test]
    fn fits_simulates_without_packing() {
        let mut packer = ShelfPacker::new(vk::Extent2D {
            width: 64,
            height: 32,
        });
        packer.pack(64, 16).expect("fits");
        assert!(packer.fits(&[(32, 16), (32, 16)]));
        assert!(!packer.fits(&[(32, 16), (32, 16), (1, 1)]));
        assert_eq!(packer.rows_used(), 16, "fits must not move the packer");
        assert_eq!(packer.pack(32, 16), Some((0, 16)));
        assert!(packer.fits(&[]));
    }

    #[test]
    fn component_alpha_footprint_is_four_planes_wide() {
        // A ComponentAlpha glyph reserves 4 × its width: 1024-wide
        // subpixel glyphs fill a 4096-wide shelf one per shelf, so two
        // 2048 tall fill the atlas, and one 1025 wide cannot be placed.
        let packer = ShelfPacker::new(full_size());
        let planes = crate::kms::render::glyph_pixels::PLANES;
        let ca = (1024 * planes, 2048);
        assert!(packer.fits(&[ca, ca]));
        assert!(!packer.fits(&[ca, ca, (1, 1)]));
        assert!(
            packer.fits(&[(1024, 2048); 8]),
            "the same glyphs as A8 take a quarter"
        );
        assert!(!packer.fits_empty(1025 * planes, 8));
        assert!(packer.fits_empty(1024 * planes, 8));
    }

    #[test]
    fn forget_drops_one_key_and_forget_font_a_whole_set() {
        let mut packer = ShelfPacker::new(full_size());
        for (font, cp) in [(1, 1), (1, 2), (1, 3), (2, 1)] {
            packer.insert_entry(key(font, cp), entry(0, 0, 8, 8));
        }
        assert!(packer.forget(key(1, 2)));
        assert!(!packer.forget(key(1, 2)), "already gone");
        assert!(packer.lookup(key(1, 2)).is_none());
        assert_eq!(packer.forget_font(1), 2);
        assert!(packer.lookup(key(1, 1)).is_none());
        assert!(packer.lookup(key(2, 1)).is_some(), "other sets keep theirs");
        assert_eq!(packer.cache_len(), 1);
    }

    #[test]
    fn note_dropped_warns_first_then_rate_limited() {
        let mut packer = ShelfPacker::new(full_size());
        let t0 = Instant::now();
        assert!(packer.note_dropped(5000, 8, t0));
        assert!(!packer.note_dropped(5000, 8, t0 + Duration::from_secs(1)));
        assert!(!packer.note_dropped(5000, 8, t0 + Duration::from_secs(9)));
        assert!(packer.note_dropped(5000, 8, t0 + DROP_WARN_INTERVAL));
        assert_eq!(packer.drops, 4);
        assert_eq!(packer.drops_since_warn, 0);
    }
}
