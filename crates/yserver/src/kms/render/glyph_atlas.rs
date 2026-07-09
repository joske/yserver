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
//! - Cache / shelf state is monotonic — freed glyphs (when
//!   `FreeGlyphs`/`FreeGlyphSet` from Stage 3d eventually lands)
//!   don't reclaim their atlas slot. Stage 5's grow + LRU pass
//!   owns slot reclamation. The fixed 4096² R8 atlas comfortably
//!   holds typical desktop sessions (~14k glyphs).
//! - When the atlas is full, `pack` returns `None`; the engine
//!   drops the glyph (pen advances by `character_width`, no draw),
//!   increments `glyphs_dropped_atlas_full`, and logs `atlas_full`
//!   once per session. No pixman fallback (v1's path is gone per
//!   the v2 spec).

#![allow(
    dead_code,
    reason = "Stage 3a consumers (text + RENDER glyphs) wire up incrementally"
)]

use std::{collections::HashMap, sync::Arc};

use ash::vk;

use crate::kms::vk::device::VkContext;
pub(crate) use crate::kms::vk::glyph::{AtlasEntry, GlyphKey};

/// Side length (px) of the fixed atlas allocation. 4096² R8 = 16 MiB.
pub(crate) const ATLAS_SIDE: u32 = 4096;

/// Pure-logic shelf packer + cache. Factored out of
/// [`GlyphAtlas`] so unit tests can exercise pack / cache
/// semantics without a live VkContext.
pub(crate) struct ShelfPacker {
    extent: vk::Extent2D,
    cache: HashMap<GlyphKey, AtlasEntry>,
    shelves: Vec<Shelf>,
    atlas_full_logged: bool,
}

impl ShelfPacker {
    pub(crate) fn new(extent: vk::Extent2D) -> Self {
        Self {
            extent,
            cache: HashMap::new(),
            shelves: Vec::new(),
            atlas_full_logged: false,
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
        if w == 0 || h == 0 {
            return Some((0, 0));
        }
        if w > self.extent.width || h > self.extent.height {
            return None;
        }
        for shelf in &mut self.shelves {
            if shelf.height >= h && shelf.x_used + w <= self.extent.width {
                let x = shelf.x_used;
                let y = shelf.y_top;
                shelf.x_used += w;
                return Some((x, y));
            }
        }
        let next_y = self.shelves.last().map(|s| s.y_top + s.height).unwrap_or(0);
        if next_y + h > self.extent.height {
            return None;
        }
        self.shelves.push(Shelf {
            y_top: next_y,
            height: h,
            x_used: w,
        });
        Some((0, next_y))
    }

    pub(crate) fn insert_entry(&mut self, key: GlyphKey, entry: AtlasEntry) {
        self.cache.insert(key, entry);
    }

    /// Latch + log the first time pack refuses a glyph. Returns
    /// `true` exactly once per packer.
    pub(crate) fn note_full_once(&mut self) -> bool {
        if self.atlas_full_logged {
            return false;
        }
        self.atlas_full_logged = true;
        log::warn!(
            "render glyph atlas full ({}×{} R8 exhausted); affected glyphs drop until \
             Stage 5 grow/LRU lands",
            self.extent.width,
            self.extent.height,
        );
        true
    }

    #[cfg(test)]
    pub(crate) fn cache_len(&self) -> usize {
        self.cache.len()
    }
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
        let memory = match unsafe { vk.device.allocate_memory(&alloc_info, None) } {
            Ok(m) => m,
            Err(e) => {
                unsafe { vk.device.destroy_image(image, None) };
                return Err(e.into());
            }
        };
        if let Err(e) = unsafe { vk.device.bind_image_memory(image, memory, 0) } {
            unsafe {
                vk.device.free_memory(memory, None);
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
                    vk.device.free_memory(memory, None);
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

    pub(crate) fn note_full_once(&mut self) -> bool {
        self.packer.note_full_once()
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
    /// (offset 0, tightly packed) into the atlas image at
    /// `(atlas_x, atlas_y)`. Updates the tracked layout to
    /// `SHADER_READ_ONLY_OPTIMAL` on return.
    ///
    /// Caller is responsible for sequencing this on a CB whose
    /// staging buffer outlives the submission via the engine's
    /// `SubmittedOp` discipline.
    pub(crate) fn record_upload(
        &mut self,
        cb: vk::CommandBuffer,
        staging_buffer: vk::Buffer,
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
            .buffer_offset(0)
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

    /// Test-helper: count of cached glyphs.
    #[cfg(test)]
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
            self.vk.device.free_memory(self.memory, None);
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
        // note_full_once latches.
        assert!(packer.note_full_once());
        assert!(!packer.note_full_once());
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
                w: 8,
                h: 16,
                pen_left: 0,
                pen_top: 12,
            },
        );
        let got = packer.lookup(key).expect("cache hit");
        assert_eq!(got.atlas_x, ax);
        assert_eq!(got.atlas_y, ay);
        assert_eq!(got.w, 8);
        assert_eq!(got.h, 16);
    }
}
