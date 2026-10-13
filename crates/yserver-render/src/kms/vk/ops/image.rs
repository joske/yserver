//! `PutImage` / `GetImage` (sub-phase 4.1.4.3).
//!
//! Host-mapped staging buffer ↔ `DrawableImage` mirror via
//! `vkCmdCopyBufferToImage` (PutImage) and `vkCmdCopyImageToBuffer`
//! (GetImage). The caller (backend.rs) memcpy's pixel rows into the
//! staging buffer (with per-format byte permutation matching the
//! pixman path) before invoking [`record_put_image`], and reads them
//! back out after [`record_get_image`].
//!
//! Both regular core PutImage/GetImage and the MIT-SHM v1.2 variants
//! funnel through the backend's `put_image` / `get_image` trait
//! methods, so the wiring lives there — this module only owns the
//! Vulkan barrier sequence and the cmd buffer recording.
//!
//! Both recorders bracket the operation with two
//! `vkCmdPipelineBarrier2` calls and leave the mirror in
//! `SHADER_READ_ONLY_OPTIMAL` so the next composite-pass read sees a
//! sample-able layout.

use ash::vk;

use crate::kms::vk::{device::VkContext, target::DrawableImage};

/// Record an upload from `staging` into `dst`. `regions` indexes
/// disjoint sub-rects of the destination image; the staging buffer
/// must already contain tightly-packed rows for each region at the
/// region's `buffer_offset`.
pub fn record_put_image(
    vk: &VkContext,
    cb: vk::CommandBuffer,
    dst: &mut DrawableImage,
    staging: vk::Buffer,
    regions: &[vk::BufferImageCopy],
) -> Result<(), vk::Result> {
    if regions.is_empty() {
        return Ok(());
    }
    let device = &vk.device;
    let old_layout = dst.current_layout();

    let to_dst = [vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
        .src_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
        .dst_stage_mask(vk::PipelineStageFlags2::COPY)
        .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
        .old_layout(old_layout)
        .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
        .image(dst.vk_image)
        .subresource_range(color_subresource_range())];
    let to_dst_dep = vk::DependencyInfo::default().image_memory_barriers(&to_dst);
    crate::vk_count!(cmd_pipeline_barrier2);
    unsafe { device.cmd_pipeline_barrier2(cb, &to_dst_dep) };

    unsafe {
        crate::vk_count!(cmd_copy_buffer_to_image);
        device.cmd_copy_buffer_to_image(
            cb,
            staging,
            dst.vk_image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            regions,
        );
    }

    let to_read = [vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::COPY)
        .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
        .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
        .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
        .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
        .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
        .image(dst.vk_image)
        .subresource_range(color_subresource_range())];
    let to_read_dep = vk::DependencyInfo::default().image_memory_barriers(&to_read);
    crate::vk_count!(cmd_pipeline_barrier2);
    unsafe { device.cmd_pipeline_barrier2(cb, &to_read_dep) };

    dst.set_current_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
    Ok(())
}

/// Record a readback from `src` into `staging`. After
/// [`run_one_shot_op`](super::run_one_shot_op) returns the staging
/// buffer's mapped memory contains the requested rect rows tightly
/// packed.
pub fn record_get_image(
    vk: &VkContext,
    cb: vk::CommandBuffer,
    src: &mut DrawableImage,
    staging: vk::Buffer,
    regions: &[vk::BufferImageCopy],
) -> Result<(), vk::Result> {
    if regions.is_empty() {
        return Ok(());
    }
    let device = &vk.device;
    let old_layout = src.current_layout();

    let to_src = [vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
        .src_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
        .dst_stage_mask(vk::PipelineStageFlags2::COPY)
        .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
        .old_layout(old_layout)
        .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
        .image(src.vk_image)
        .subresource_range(color_subresource_range())];
    let to_src_dep = vk::DependencyInfo::default().image_memory_barriers(&to_src);
    crate::vk_count!(cmd_pipeline_barrier2);
    unsafe { device.cmd_pipeline_barrier2(cb, &to_src_dep) };

    unsafe {
        crate::vk_count!(cmd_copy_image_to_buffer);
        device.cmd_copy_image_to_buffer(
            cb,
            src.vk_image,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            staging,
            regions,
        );
    }

    let to_read = [vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::COPY)
        .src_access_mask(vk::AccessFlags2::TRANSFER_READ)
        .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
        .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
        .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
        .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
        .image(src.vk_image)
        .subresource_range(color_subresource_range())];
    let to_read_dep = vk::DependencyInfo::default().image_memory_barriers(&to_read);
    crate::vk_count!(cmd_pipeline_barrier2);
    unsafe { device.cmd_pipeline_barrier2(cb, &to_read_dep) };

    src.set_current_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
    Ok(())
}

fn color_subresource_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1)
}
