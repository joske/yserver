//! Shared glyph atlas (sub-phase 4.1.4.5).
//!
//! Spec: docs/superpowers/specs/2026-05-07-phase4-1-vulkan-compositor-design.md
//! "Glyph atlas".
//!
//! FreeType still rasterises glyphs CPU-side; this module owns the
//! resulting `R8_UNORM` atlas image plus a glyph-key→entry cache, a
//! shelf bin packer, and an upload helper. `record_text_run` reads
//! the atlas via the text pipeline's combined-image-sampler
//! descriptor set (the atlas image view is bound once at pipeline
//! construction; the descriptor never needs re-binding).
//!
//! ## Sizing
//!
//! Atlas is a fixed `4096×4096` allocation (16 MiB) created
//! eagerly at backend startup. That's plenty for typical sessions
//! (fvwm + xterm + wmaker fits in well under 1024×1024 of glyphs).
//! When the atlas fills, [`Self::intern`] returns `None` and the
//! caller falls back to pixman for the offending glyph.
//! Grow-on-demand and LRU eviction are deferred — atlas-full has
//! never been observed in practice and a fixed allocation avoids
//! the descriptor-rebinding refactor that grow would require.
//!
//! ## Bin packing
//!
//! Simple shelf packer. Each shelf is a horizontal row pinned at
//! a fixed `y_top`, growing left-to-right as glyphs land. A new
//! glyph fits in the current shelf if its height ≤ shelf height
//! and there's still room along the row; otherwise a new shelf
//! opens below. When a new shelf would exceed the atlas height,
//! the atlas is full.
//!
//! ## Upload cadence
//!
//! Eager submit per glyph: each `intern` call that adds a new
//! entry runs a one-shot `cmd_copy_buffer_to_image` via
//! [`run_one_shot_op`]. Per the 4.1.4 family-port decisions
//! batched uploads can wait for 4.1.4.6 RENDER's higher op rate.

use std::{collections::HashMap, ptr::NonNull, sync::Arc};

use ash::vk;

use super::{device::VkContext, ops::run_one_shot_op};

/// Side length (px) of the fixed atlas allocation. 4096² R8 = 16 MiB.
pub const ATLAS_SIDE: u32 = 4096;

/// Cache key. Identifies a glyph uniquely across the server's font
/// table. `font_xid` is the X resource id of the loaded font;
/// re-loading the same TTF at a different size produces a different
/// `font_xid`, so the same `(file, codepoint)` at two sizes ends up
/// at two atlas entries (correct).
#[derive(Hash, Eq, PartialEq, Copy, Clone, Debug)]
pub struct GlyphKey {
    pub font_xid: u32,
    pub codepoint: u32,
}

/// Where the glyph lives inside the atlas, plus the FreeType pen
/// offsets the caller needs to position the dst quad. Caller does
/// `dst_x = pen_x + entry.pen_left`,
/// `dst_y = pen_y - entry.pen_top` (FreeType's `bitmap_top` is the
/// y-up offset from the baseline to the glyph's top row).
///
/// `packed_w` and `logical_w` coincide for a single-plane `A8`
/// entry, and DIVERGE for a `ComponentAlpha` one — four
/// horizontally-adjacent coverage planes of `logical_w` texels each,
/// so `packed_w == 4 * logical_w`. The atlas copy region and the
/// packer need the **footprint** (`packed_w`), while the dst quad,
/// the damage extent and the instance geometry need the **glyph's
/// own size** (`logical_w`).
/// Keep them as two fields rather than deriving one from the other
/// (e.g. from `layout`) — a derived value invites a call site to
/// use the wrong one, and the failure mode is a subtly misaligned
/// or stretched glyph rather than a compile error.
#[derive(Copy, Clone, Debug)]
pub struct AtlasEntry {
    pub atlas_x: u32,
    pub atlas_y: u32,
    /// Width of the atlas footprint in texels — what the packer
    /// reserved and what the upload copy region covers. Equal to
    /// `logical_w` for an `A8` entry; `4 * logical_w` for a
    /// `ComponentAlpha` one.
    pub packed_w: u32,
    /// The glyph's own width in pixels — what the dst quad, the
    /// damage extent and the instance geometry use. Never a
    /// multiple of the plane count, unlike `packed_w`.
    pub logical_w: u32,
    pub h: u32,
    pub pen_left: i32,
    pub pen_top: i32,
    /// Which pipeline samples this entry, and whether its packed
    /// width encodes multiple planes at a fixed stride. `A8` is a
    /// single plane (`packed_w == logical_w`) sampled through the
    /// interpolated UV; `ComponentAlpha` is four planes fetched by
    /// texel index at the explicit per-instance plane stride.
    ///
    /// Decided in exactly one place —
    /// `RenderEngine::effective_glyph_layout` — from the glyphset's
    /// picture format and the device's `dualSrcBlend`. An entry whose
    /// layout disagreed with the bytes the uploader staged would be
    /// sampled as garbage.
    pub layout: GlyphLayout,
}

/// Selects how an atlas entry's texels are laid out, and therefore
/// which text pipeline samples it.
///
/// `A8` is one coverage plane per glyph pixel — every core-text
/// glyph, every `PICT_a8` / `PICT_a1` glyphset, and every `ARGB32`
/// glyphset on a device without `dualSrcBlend` (where the upload
/// reduces the colour channels to their mean).
///
/// `ComponentAlpha` is four adjacent planes — **logical R, G, B, A
/// coverage, in that order** — packed at
/// `packed_w == 4 * logical_w`, sampled by a pipeline that gathers
/// all four and emits the per-channel alpha factor to output index 1
/// for dual-source blending. That is X RENDER component alpha, i.e.
/// subpixel/LCD text antialiasing.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum GlyphLayout {
    A8,
    ComponentAlpha,
}

/// Per-backend glyph atlas. Owns the atlas image + view + memory,
/// the cache, the bin-packer state, and a host-mapped staging
/// buffer reused across upload calls.
pub struct GlyphAtlas {
    vk: Arc<VkContext>,
    image: vk::Image,
    view: vk::ImageView,
    memory: vk::DeviceMemory,
    extent: vk::Extent2D,
    cache: HashMap<GlyphKey, AtlasEntry>,
    shelves: Vec<Shelf>,
    /// Reused staging buffer. Grows on demand when a single glyph
    /// upload exceeds the current size.
    staging_buffer: vk::Buffer,
    staging_memory: vk::DeviceMemory,
    staging_mapped: NonNull<u8>,
    staging_size: u64,
    /// Tracks the atlas image's current Vulkan layout. Starts
    /// `UNDEFINED`; flips to `SHADER_READ_ONLY_OPTIMAL` after the
    /// first upload. Subsequent uploads transition through
    /// `TRANSFER_DST_OPTIMAL` and back.
    current_layout: vk::ImageLayout,
}

unsafe impl Send for GlyphAtlas {}
unsafe impl Sync for GlyphAtlas {}

#[derive(Debug, Clone, Copy)]
struct Shelf {
    y_top: u32,
    height: u32,
    x_used: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum GlyphAtlasError {
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
    pub fn new(vk: Arc<VkContext>) -> Result<Self, GlyphAtlasError> {
        let extent = vk::Extent2D {
            width: ATLAS_SIDE,
            height: ATLAS_SIDE,
        };

        // R8_UNORM atlas — alpha-only glyph bitmaps. SAMPLED for
        // text shader, TRANSFER_DST for upload.
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
        let memory_type_index = (0..mem_props.memory_type_count)
            .find(|&i| {
                mem_reqs.memory_type_bits & (1 << i) != 0
                    && mem_props.memory_types[i as usize]
                        .property_flags
                        .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
            })
            .ok_or(GlyphAtlasError::NoMemoryType);
        let memory_type_index = match memory_type_index {
            Ok(i) => i,
            Err(e) => {
                unsafe { vk.device.destroy_image(image, None) };
                return Err(e);
            }
        };
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_reqs.size)
            .memory_type_index(memory_type_index)
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

        // Starter staging buffer. Most glyphs are small (< 1 KiB);
        // 64 KiB covers a comfortable batch without per-call grow.
        let starter_staging: u64 = 64 * 1024;
        let (staging_buffer, staging_memory, staging_mapped) =
            match allocate_staging(&vk, starter_staging) {
                Ok(t) => t,
                Err(e) => {
                    unsafe {
                        vk.device.destroy_image_view(view, None);
                        crate::kms::vk::mem_accounting::free_memory(&vk.device, memory);
                        vk.device.destroy_image(image, None);
                    }
                    return Err(e);
                }
            };

        Ok(Self {
            vk,
            image,
            view,
            memory,
            extent,
            cache: HashMap::new(),
            shelves: Vec::new(),
            staging_buffer,
            staging_memory,
            staging_mapped,
            staging_size: starter_staging,
            current_layout: vk::ImageLayout::UNDEFINED,
        })
    }

    pub fn image(&self) -> vk::Image {
        self.image
    }

    pub fn image_view(&self) -> vk::ImageView {
        self.view
    }

    pub fn extent(&self) -> vk::Extent2D {
        self.extent
    }

    /// Returns the atlas entry for `key`, packing + uploading the
    /// glyph if it's not already cached. `pixels` is row-major,
    /// tightly packed (no row stride beyond `w`), one alpha byte
    /// per pixel — the FreeType `BITMAP_GRAY` layout the existing
    /// `render_text_chars` already produces.
    ///
    /// `pool` is the backend's drawing-op command pool —
    /// [`OpsCommandPool`](super::ops::OpsCommandPool) — reused for
    /// the one-shot upload CB.
    ///
    /// Returns `None` when the atlas is full or a Vulkan API call
    /// fails partway through the upload. Caller then falls back to
    /// the pixman path for the offending glyph.
    pub fn intern(
        &mut self,
        key: GlyphKey,
        w: u32,
        h: u32,
        pen_left: i32,
        pen_top: i32,
        pixels: &[u8],
        pool: vk::CommandPool,
    ) -> Option<AtlasEntry> {
        if let Some(entry) = self.cache.get(&key) {
            return Some(*entry);
        }
        if w == 0 || h == 0 {
            // Zero-area glyph (e.g. ' '). Cache a degenerate entry
            // so the caller can advance the pen without retrying.
            let entry = AtlasEntry {
                atlas_x: 0,
                atlas_y: 0,
                packed_w: w,
                logical_w: w,
                h,
                pen_left,
                pen_top,
                layout: GlyphLayout::A8,
            };
            self.cache.insert(key, entry);
            return Some(entry);
        }
        if pixels.len() < (w as usize) * (h as usize) {
            return None;
        }

        let (atlas_x, atlas_y) = self.pack(w, h)?;

        // Grow staging if needed.
        let needed: u64 = u64::from(w) * u64::from(h);
        if needed > self.staging_size
            && let Err(e) = self.grow_staging(needed)
        {
            log::warn!("glyph atlas: staging grow failed: {e:?}");
            return None;
        }

        // Memcpy bitmap into staging (tightly packed).
        // SAFETY: staging_mapped covers [0, staging_size) and we just
        // grew it to ≥ needed. `pixels` length checked above.
        unsafe {
            std::ptr::copy_nonoverlapping(
                pixels.as_ptr(),
                self.staging_mapped.as_ptr(),
                (w as usize) * (h as usize),
            );
        }

        let result = self.record_upload(pool, atlas_x, atlas_y, w, h);
        if let Err(e) = result {
            log::warn!("glyph atlas: upload failed: {e:?}");
            return None;
        }

        let entry = AtlasEntry {
            atlas_x,
            atlas_y,
            packed_w: w,
            logical_w: w,
            h,
            pen_left,
            pen_top,
            layout: GlyphLayout::A8,
        };
        self.cache.insert(key, entry);
        Some(entry)
    }

    fn pack(&mut self, w: u32, h: u32) -> Option<(u32, u32)> {
        if w > self.extent.width || h > self.extent.height {
            return None;
        }
        // Try existing shelves.
        for shelf in &mut self.shelves {
            if shelf.height >= h && shelf.x_used + w <= self.extent.width {
                let x = shelf.x_used;
                let y = shelf.y_top;
                shelf.x_used += w;
                return Some((x, y));
            }
        }
        // Open a new shelf.
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

    fn record_upload(
        &mut self,
        pool: vk::CommandPool,
        atlas_x: u32,
        atlas_y: u32,
        w: u32,
        h: u32,
    ) -> Result<(), vk::Result> {
        let buffer = self.staging_buffer;
        let image = self.image;
        let old_layout = self.current_layout;

        run_one_shot_op(&self.vk, pool, |vk, cb| {
            let device = &vk.device;
            let to_dst = [vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                .src_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
                .dst_stage_mask(vk::PipelineStageFlags2::COPY)
                .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .old_layout(old_layout)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .image(image)
                .subresource_range(color_subresource_range())];
            let to_dst_dep = vk::DependencyInfo::default().image_memory_barriers(&to_dst);
            crate::vk_count!(cmd_pipeline_barrier2);
            unsafe { device.cmd_pipeline_barrier2(cb, &to_dst_dep) };

            let region = vk::BufferImageCopy::default()
                .buffer_offset(0)
                .buffer_row_length(0)
                .buffer_image_height(0)
                .image_subresource(
                    vk::ImageSubresourceLayers::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .layer_count(1),
                )
                .image_offset(vk::Offset3D {
                    x: atlas_x as i32,
                    y: atlas_y as i32,
                    z: 0,
                })
                .image_extent(vk::Extent3D {
                    width: w,
                    height: h,
                    depth: 1,
                });
            let regions = [region];
            unsafe {
                crate::vk_count!(cmd_copy_buffer_to_image);
                device.cmd_copy_buffer_to_image(
                    cb,
                    buffer,
                    image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &regions,
                );
            }

            let to_read = [vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::COPY)
                .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
                .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .image(image)
                .subresource_range(color_subresource_range())];
            let to_read_dep = vk::DependencyInfo::default().image_memory_barriers(&to_read);
            crate::vk_count!(cmd_pipeline_barrier2);
            unsafe { device.cmd_pipeline_barrier2(cb, &to_read_dep) };
            Ok(())
        })?;
        self.current_layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
        Ok(())
    }

    fn grow_staging(&mut self, at_least: u64) -> Result<(), GlyphAtlasError> {
        let mut new_size = self.staging_size.max(64 * 1024);
        while new_size < at_least {
            new_size = new_size.checked_mul(2).unwrap_or(at_least);
        }
        let (buffer, memory, mapped) = allocate_staging(&self.vk, new_size)?;
        // 5-T6: no queue_wait_idle. `grow_staging`'s only caller is
        // `GlyphAtlas::intern`, which submits per-glyph one-shot CBs
        // and waits on each (today via `queue_wait_idle` — still in
        // intern, deliberate Phase 5 deferred; tomorrow via a
        // per-glyph fence in a follow-up phase). At the time
        // `grow_staging` runs, either:
        //   (a) there has been no prior glyph upload yet (first
        //       grow: no CB ever referenced this buffer); OR
        //   (b) the last successful glyph upload has already been
        //       waited on (intern's per-glyph wait drained it).
        // In either case the OLD staging buffer has no live CB
        // reference and is safe to free synchronously. If a future
        // caller takes the atlas staging through a non-waiting
        // path, this comment block becomes the audit point.
        unsafe {
            self.vk.device.unmap_memory(self.staging_memory);
            self.vk.device.destroy_buffer(self.staging_buffer, None);
            crate::kms::vk::mem_accounting::free_memory(&self.vk.device, self.staging_memory);
        }
        self.staging_buffer = buffer;
        self.staging_memory = memory;
        self.staging_mapped = mapped;
        self.staging_size = new_size;
        Ok(())
    }
}

impl Drop for GlyphAtlas {
    fn drop(&mut self) {
        unsafe {
            let _ = self.vk.device.queue_wait_idle(self.vk.graphics_queue);
            self.vk.device.unmap_memory(self.staging_memory);
            self.vk.device.destroy_buffer(self.staging_buffer, None);
            crate::kms::vk::mem_accounting::free_memory(&self.vk.device, self.staging_memory);
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

fn allocate_staging(
    vk: &VkContext,
    size: u64,
) -> Result<(vk::Buffer, vk::DeviceMemory, NonNull<u8>), GlyphAtlasError> {
    let buf_info = vk::BufferCreateInfo::default()
        .size(size)
        .usage(vk::BufferUsageFlags::TRANSFER_SRC)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let buffer = unsafe { vk.device.create_buffer(&buf_info, None)? };
    let mem_reqs = unsafe { vk.device.get_buffer_memory_requirements(buffer) };
    let mem_props = unsafe {
        vk.instance
            .get_physical_device_memory_properties(vk.physical_device)
    };
    let want = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
    let memory_type_index = (0..mem_props.memory_type_count).find(|&i| {
        mem_reqs.memory_type_bits & (1 << i) != 0
            && mem_props.memory_types[i as usize]
                .property_flags
                .contains(want)
    });
    let memory_type_index = match memory_type_index {
        Some(i) => i,
        None => {
            unsafe { vk.device.destroy_buffer(buffer, None) };
            return Err(GlyphAtlasError::NoMemoryType);
        }
    };
    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(mem_reqs.size)
        .memory_type_index(memory_type_index);
    let memory = match crate::kms::vk::mem_accounting::allocate_memory(
        &vk.device,
        &alloc_info,
        crate::kms::vk::mem_accounting::MemCategory::Glyph,
        &mem_props,
    ) {
        Ok(m) => m,
        Err(e) => {
            unsafe { vk.device.destroy_buffer(buffer, None) };
            return Err(e.into());
        }
    };
    if let Err(e) = unsafe { vk.device.bind_buffer_memory(buffer, memory, 0) } {
        unsafe {
            crate::kms::vk::mem_accounting::free_memory(&vk.device, memory);
            vk.device.destroy_buffer(buffer, None);
        }
        return Err(e.into());
    }
    let mapped_ptr = match unsafe {
        vk.device
            .map_memory(memory, 0, size, vk::MemoryMapFlags::empty())
    } {
        Ok(p) => p,
        Err(e) => {
            unsafe {
                crate::kms::vk::mem_accounting::free_memory(&vk.device, memory);
                vk.device.destroy_buffer(buffer, None);
            }
            return Err(e.into());
        }
    };
    let mapped = NonNull::new(mapped_ptr.cast::<u8>()).expect("vkMapMemory returned non-null");
    Ok((buffer, memory, mapped))
}
