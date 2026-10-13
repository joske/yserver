//! Backend-wide R8 mask scratch image (sub-phase 4.1.4.7).
//!
//! Some RENDER ops (`Trapezoids`, `Triangles`, glyph-shape composites
//! that aren't through the atlas) need an A8 coverage mask that
//! doesn't correspond to any X drawable. The mask is rasterized
//! GPU-side by the [`trap_pipeline`](super::trap_pipeline) draw,
//! which targets this image as a `COLOR_ATTACHMENT`; the surrounding
//! composite then samples it as a `SHADER_READ_ONLY_OPTIMAL` mask.
//!
//! The image view is lifetime-stable as long as the caller doesn't
//! request a bigger mask; resize via
//! [`MaskScratch::ensure_image_size_returning_old`] allocates a new
//! image, returns the old one wrapped as a `BatchResource` for
//! defer-release, and installs the new fields on the scratch.
//!
//! 5-T5: defer-release replaces the pre-Phase-5 pre-flush gate —
//! the old image survives any in-flight CB because the scheduler
//! holds it until the open `PaintBatch` retires (or releases it
//! synchronously if no in-flight batch exists).
//!
//! Single shared scratch — the renderer schedules render-traps
//! into PaintBatches that submit before the next protocol cycle,
//! so the scratch only has pending uses inside the current
//! recording batch.
//!
//! Pre-gpu-trap (T5) note: a `record_upload_r8` method used to
//! upload CPU-rasterized coverage into this image via a staged copy
//! from a `BatchUploadArena` chunk. The CPU rasterizer was retired
//! in gpu-trap T5 in favour of the GPU draw, and the upload helper
//! went with it.

use std::sync::Arc;

use ash::vk;

use super::device::VkContext;
use crate::kms::render::batch_resource::BatchResource;

#[derive(Debug, thiserror::Error)]
pub enum MaskScratchError {
    #[error("vulkan: {0:?}")]
    Vk(vk::Result),
    #[error("no memory type matches mask scratch requirements")]
    NoMemoryType,
}

impl From<vk::Result> for MaskScratchError {
    fn from(r: vk::Result) -> Self {
        MaskScratchError::Vk(r)
    }
}

/// 5-T5: wraps the just-replaced scratch image (image + view +
/// memory) so the scheduler can release it after the in-flight batch
/// retires. The new image has already been installed on `MaskScratch`
/// by the time this is constructed.
#[derive(Debug)]
struct RetiredMaskScratchImage {
    image: vk::Image,
    view: vk::ImageView,
    attachment_view: vk::ImageView,
    image_memory: vk::DeviceMemory,
}

impl BatchResource for RetiredMaskScratchImage {
    fn release(self: Box<Self>, vk: &VkContext) {
        unsafe {
            vk.device.destroy_image_view(self.attachment_view, None);
            vk.device.destroy_image_view(self.view, None);
            vk.device.destroy_image(self.image, None);
            crate::kms::vk::mem_accounting::free_memory(&vk.device, self.image_memory);
        }
    }
}

pub struct MaskScratch {
    vk: Arc<VkContext>,
    image: vk::Image,
    /// Sampled view, with `a=R` swizzle so the composite shader's
    /// `mask_sample.a` returns the R8 coverage. Bound as
    /// `COMBINED_IMAGE_SAMPLER` in the descriptor set.
    view: vk::ImageView,
    /// Attachment view, with **IDENTITY swizzle** so the trap
    /// rasterize's fragment write to the R component lands
    /// correctly. Vulkan requires identity swizzle on framebuffer
    /// attachment views (`VUID-VkFramebufferCreateInfo-pAttachments-00891`).
    /// lavapipe is lenient about this — it accepts the swizzled view
    /// either way — but Intel and RADV reject the write or produce
    /// undefined results, surfacing as "the rasterize wrote nothing"
    /// in the subsequent sampler read. Bound as `COLOR_ATTACHMENT`
    /// in `cmd_begin_rendering`.
    attachment_view: vk::ImageView,
    image_memory: vk::DeviceMemory,
    extent: vk::Extent2D,
    current_layout: vk::ImageLayout,
}

impl MaskScratch {
    pub fn new(vk: Arc<VkContext>) -> Result<Self, MaskScratchError> {
        // Start at 256×256; grow on demand. Most RENDER traps/triangle
        // workloads (cairo glyph shapes) fit; rendercheck pushes
        // ~1024×1024 in worst-case test rects.
        let extent = vk::Extent2D {
            width: 256,
            height: 256,
        };
        let (image, view, attachment_view, image_memory) = allocate_image(&vk, extent)?;
        Ok(Self {
            vk,
            image,
            view,
            attachment_view,
            image_memory,
            extent,
            current_layout: vk::ImageLayout::UNDEFINED,
        })
    }

    pub fn extent(&self) -> vk::Extent2D {
        self.extent
    }

    pub fn image_view(&self) -> vk::ImageView {
        self.view
    }

    /// IDENTITY-swizzle view for use as a color attachment. See the
    /// struct field doc for the spec rationale.
    pub fn attachment_view(&self) -> vk::ImageView {
        self.attachment_view
    }

    /// Raw `vk::Image` handle. Needed by the GPU trap-rasterize path
    /// (gpu-trap T2) to build image-memory barriers that transition
    /// the mask between `SHADER_READ_ONLY_OPTIMAL` and
    /// `COLOR_ATTACHMENT_OPTIMAL` for the new draw.
    pub fn image(&self) -> vk::Image {
        self.image
    }

    /// CPU-tracked layout (3F-2 #8 invariant). Returns the layout
    /// the image will be in once all previously recorded CBs execute
    /// in order. The trap-rasterize path (gpu-trap T2) reads this to
    /// pick the source stage/access of the first barrier.
    pub fn current_layout(&self) -> vk::ImageLayout {
        self.current_layout
    }

    /// Update the CPU-tracked layout. Called by the trap-rasterize
    /// path (gpu-trap T2) after its terminal COLOR_ATTACHMENT →
    /// SHADER_READ_ONLY_OPTIMAL barrier records so the next consumer
    /// of the scratch sees the same layout that future CB execution
    /// will leave the image in.
    pub fn set_current_layout(&mut self, layout: vk::ImageLayout) {
        self.current_layout = layout;
    }

    /// Phase B.2 Task 9 peek: would
    /// `ensure_image_size_returning_old(w, h)` grow the scratch?
    /// Returns `true` when the existing extent already satisfies the
    /// request (no grow needed), `false` when a grow would fire. The
    /// Task 9 caller uses this BEFORE `ensure_image_size_returning_old`
    /// to decide whether to force a close-reopen (so the
    /// just-submitted frame's recorded views target the same scratch
    /// instance the close-time emit will sample). Peek-only — does NOT
    /// mutate.
    ///
    /// MaskScratch is R8 only, so no format arg.
    #[allow(
        dead_code,
        reason = "B.2 Task 9 peek-before-grow predicate; first call site lands \
                  with the close-before-grow path in a later Task."
    )]
    pub fn fits(&self, width: u32, height: u32) -> bool {
        self.extent.width >= width && self.extent.height >= height
    }

    /// 5-T5: ensure the scratch image is at least `(width, height)`
    /// pixels, reallocating if smaller. Returns `Ok(None)` if no grow
    /// was needed (scratch unchanged). On grow, allocates the new
    /// resource first (so a failure leaves the scratch untouched),
    /// then installs the new fields and returns the old image
    /// wrapped as a `BatchResource` for the caller to defer-release
    /// through the scheduler.
    ///
    /// Allocation-first ordering: if `allocate_image` fails the
    /// scratch is untouched and the caller sees `Err`. Past the
    /// successful `allocate_image` the function MUST NOT fail.
    pub fn ensure_image_size_returning_old(
        &mut self,
        width: u32,
        height: u32,
    ) -> Result<Option<Box<dyn BatchResource>>, MaskScratchError> {
        if width <= self.extent.width && height <= self.extent.height {
            return Ok(None);
        }
        let new_extent = vk::Extent2D {
            width: self.extent.width.max(width).next_power_of_two().max(256),
            height: self.extent.height.max(height).next_power_of_two().max(256),
        };
        let (image, view, attachment_view, image_memory) = allocate_image(&self.vk, new_extent)?;
        // Allocation succeeded — past here the function MUST NOT fail.
        let old_image = std::mem::replace(&mut self.image, image);
        let old_view = std::mem::replace(&mut self.view, view);
        let old_attachment_view = std::mem::replace(&mut self.attachment_view, attachment_view);
        let old_memory = std::mem::replace(&mut self.image_memory, image_memory);
        self.extent = new_extent;
        self.current_layout = vk::ImageLayout::UNDEFINED;
        Ok(Some(Box::new(RetiredMaskScratchImage {
            image: old_image,
            view: old_view,
            attachment_view: old_attachment_view,
            image_memory: old_memory,
        })))
    }
}

impl Drop for MaskScratch {
    fn drop(&mut self) {
        unsafe {
            let _ = self.vk.device.queue_wait_idle(self.vk.graphics_queue);
            self.vk
                .device
                .destroy_image_view(self.attachment_view, None);
            self.vk.device.destroy_image_view(self.view, None);
            self.vk.device.destroy_image(self.image, None);
            crate::kms::vk::mem_accounting::free_memory(&self.vk.device, self.image_memory);
        }
    }
}

fn allocate_image(
    vk: &VkContext,
    extent: vk::Extent2D,
) -> Result<(vk::Image, vk::ImageView, vk::ImageView, vk::DeviceMemory), MaskScratchError> {
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
        .usage(
            vk::ImageUsageFlags::SAMPLED
                | vk::ImageUsageFlags::TRANSFER_DST
                | vk::ImageUsageFlags::COLOR_ATTACHMENT,
        )
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
    let mt = match mt {
        Some(i) => i,
        None => {
            unsafe { vk.device.destroy_image(image, None) };
            return Err(MaskScratchError::NoMemoryType);
        }
    };
    let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(mem_reqs.size)
        .memory_type_index(mt)
        .push_next(&mut dedicated);
    let memory = match crate::kms::vk::mem_accounting::allocate_memory(
        &vk.device,
        &alloc_info,
        crate::kms::vk::mem_accounting::MemCategory::Scratch,
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
    // Sampled view: swizzle so `.a` samples R — same convention the
    // Composite shader already uses for R8 mask drawables.
    let view_info = vk::ImageViewCreateInfo::default()
        .image(image)
        .view_type(vk::ImageViewType::TYPE_2D)
        .format(vk::Format::R8_UNORM)
        .components(vk::ComponentMapping {
            r: vk::ComponentSwizzle::ZERO,
            g: vk::ComponentSwizzle::ZERO,
            b: vk::ComponentSwizzle::ZERO,
            a: vk::ComponentSwizzle::R,
        })
        .subresource_range(color_subresource_range());
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
    // Attachment view: IDENTITY swizzle so the trap rasterize's R
    // fragment write lands at the R component. Vulkan spec requires
    // IDENTITY on color attachment views
    // (VUID-VkFramebufferCreateInfo-pAttachments-00891); a swizzled
    // view here yields undefined behaviour on real GPU drivers
    // (Intel: writes lost; RADV: similar). lavapipe accepts the
    // swizzled view as both, which is why this latent bug shipped
    // through 3e.2.
    let attachment_view_info = vk::ImageViewCreateInfo::default()
        .image(image)
        .view_type(vk::ImageViewType::TYPE_2D)
        .format(vk::Format::R8_UNORM)
        // Identity by default — no .components() call needed; setting
        // explicitly keeps the contract visible at the create site.
        .components(vk::ComponentMapping {
            r: vk::ComponentSwizzle::IDENTITY,
            g: vk::ComponentSwizzle::IDENTITY,
            b: vk::ComponentSwizzle::IDENTITY,
            a: vk::ComponentSwizzle::IDENTITY,
        })
        .subresource_range(color_subresource_range());
    let attachment_view = match unsafe { vk.device.create_image_view(&attachment_view_info, None) }
    {
        Ok(v) => v,
        Err(e) => {
            unsafe {
                vk.device.destroy_image_view(view, None);
                crate::kms::vk::mem_accounting::free_memory(&vk.device, memory);
                vk.device.destroy_image(image, None);
            }
            return Err(e.into());
        }
    };
    Ok((image, view, attachment_view, memory))
}

fn color_subresource_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1)
}
