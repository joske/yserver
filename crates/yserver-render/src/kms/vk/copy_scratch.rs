//! Backend-wide BGRA scratch image (sub-phase 4.1.5 prep).
//!
//! Same-target `CopyArea` (e.g. xterm scrollback) needs a staging
//! image so the source and destination of the same `vkCmdCopyImage`
//! don't alias. The pre-existing `MaskScratch` only goes up to
//! `R8_UNORM`; this is its colour-format twin, sized to fit the
//! biggest in-flight copy and grown power-of-two on demand.

use std::sync::Arc;

use ash::vk;

use super::{device::VkContext, ops::run_one_shot_op};
use crate::kms::render::batch_resource::BatchResource;

#[derive(Debug, thiserror::Error)]
pub enum CopyScratchError {
    #[error("vulkan: {0:?}")]
    Vk(vk::Result),
    #[error("no memory type matches copy scratch requirements")]
    NoMemoryType,
}

impl From<vk::Result> for CopyScratchError {
    fn from(r: vk::Result) -> Self {
        CopyScratchError::Vk(r)
    }
}

/// 5-T3: wraps the just-replaced scratch image + memory so the
/// scheduler can release it after the in-flight batch retires. The
/// new image has already been installed on `CopyScratch` by the time
/// this is constructed.
#[derive(Debug)]
struct RetiredCopyScratchImage {
    image: vk::Image,
    memory: vk::DeviceMemory,
}

impl BatchResource for RetiredCopyScratchImage {
    fn release(self: Box<Self>, vk: &VkContext) {
        unsafe {
            vk.device.destroy_image(self.image, None);
            crate::kms::vk::mem_accounting::free_memory(&vk.device, self.memory);
        }
    }
}

pub struct CopyScratch {
    vk: Arc<VkContext>,
    image: vk::Image,
    memory: vk::DeviceMemory,
    extent: vk::Extent2D,
    current_layout: vk::ImageLayout,
}

unsafe impl Send for CopyScratch {}
unsafe impl Sync for CopyScratch {}

impl CopyScratch {
    pub fn new(vk: Arc<VkContext>) -> Result<Self, CopyScratchError> {
        let extent = vk::Extent2D {
            width: 256,
            height: 256,
        };
        let (image, memory) = allocate_image(&vk, extent)?;
        Ok(Self {
            vk,
            image,
            memory,
            extent,
            current_layout: vk::ImageLayout::UNDEFINED,
        })
    }

    pub fn image(&self) -> vk::Image {
        self.image
    }

    pub fn extent(&self) -> vk::Extent2D {
        self.extent
    }

    /// 5-T3: like the pre-Phase-5 `ensure_size` but returns the old
    /// image wrapped as a `BatchResource` for the caller to defer-release
    /// through the scheduler. Returns `Ok(None)` if no grow was needed
    /// (old image is fine, scratch unchanged).
    ///
    /// Allocation-first ordering: if `allocate_image` fails the scratch
    /// is untouched and the caller sees `Err`. Past the successful
    /// `allocate_image` the function MUST NOT fail.
    pub fn ensure_size_returning_old(
        &mut self,
        width: u32,
        height: u32,
    ) -> Result<Option<Box<dyn BatchResource>>, CopyScratchError> {
        if width <= self.extent.width && height <= self.extent.height {
            return Ok(None);
        }
        let new_extent = vk::Extent2D {
            width: self.extent.width.max(width).next_power_of_two().max(256),
            height: self.extent.height.max(height).next_power_of_two().max(256),
        };
        let (image, memory) = allocate_image(&self.vk, new_extent)?;
        // Allocation succeeded — past here the function MUST NOT fail.
        let old_image = std::mem::replace(&mut self.image, image);
        let old_memory = std::mem::replace(&mut self.memory, memory);
        self.extent = new_extent;
        self.current_layout = vk::ImageLayout::UNDEFINED;
        Ok(Some(Box::new(RetiredCopyScratchImage {
            image: old_image,
            memory: old_memory,
        })))
    }

    /// Record a barrier transitioning the scratch into
    /// `TRANSFER_DST_OPTIMAL` so the caller can `cmd_copy_image` into
    /// it. Updates `current_layout`.
    pub fn record_to_transfer_dst(&mut self, cb: vk::CommandBuffer) {
        let from = self.current_layout;
        let barrier = [vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
            .src_access_mask(vk::AccessFlags2::empty())
            .dst_stage_mask(vk::PipelineStageFlags2::COPY)
            .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
            .old_layout(from)
            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .image(self.image)
            .subresource_range(color_subresource_range())];
        let dep = vk::DependencyInfo::default().image_memory_barriers(&barrier);
        crate::vk_count!(cmd_pipeline_barrier2);
        unsafe { self.vk.device.cmd_pipeline_barrier2(cb, &dep) };
        self.current_layout = vk::ImageLayout::TRANSFER_DST_OPTIMAL;
    }

    /// Record a barrier transitioning the scratch into
    /// `TRANSFER_SRC_OPTIMAL` so the caller can `cmd_copy_image` out
    /// of it. Updates `current_layout`.
    pub fn record_to_transfer_src(&mut self, cb: vk::CommandBuffer) {
        let from = self.current_layout;
        let barrier = [vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::COPY)
            .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags2::COPY)
            .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
            .old_layout(from)
            .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .image(self.image)
            .subresource_range(color_subresource_range())];
        let dep = vk::DependencyInfo::default().image_memory_barriers(&barrier);
        crate::vk_count!(cmd_pipeline_barrier2);
        unsafe { self.vk.device.cmd_pipeline_barrier2(cb, &dep) };
        self.current_layout = vk::ImageLayout::TRANSFER_SRC_OPTIMAL;
    }

    /// Sanity check: `mem::take`-ish quick test that allocation worked.
    /// Mostly used to keep `run_one_shot_op` linked here (the helper
    /// is shared with mask_scratch).
    #[allow(dead_code)]
    pub fn quick_smoke(&self, vk: &VkContext, pool: vk::CommandPool) -> Result<(), vk::Result> {
        run_one_shot_op(vk, pool, |_vk, _cb| Ok(()))
    }
}

impl Drop for CopyScratch {
    fn drop(&mut self) {
        unsafe {
            let _ = self.vk.device.queue_wait_idle(self.vk.graphics_queue);
            self.vk.device.destroy_image(self.image, None);
            crate::kms::vk::mem_accounting::free_memory(&self.vk.device, self.memory);
        }
    }
}

fn allocate_image(
    vk: &VkContext,
    extent: vk::Extent2D,
) -> Result<(vk::Image, vk::DeviceMemory), CopyScratchError> {
    let image_info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(vk::Format::B8G8R8A8_UNORM)
        .extent(vk::Extent3D {
            width: extent.width,
            height: extent.height,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::TRANSFER_DST)
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
            return Err(CopyScratchError::NoMemoryType);
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
    Ok((image, memory))
}

fn color_subresource_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1)
}
