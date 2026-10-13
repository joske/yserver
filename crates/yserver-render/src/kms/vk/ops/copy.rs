//! `CopyArea` (sub-phase 4.1.4.2).
//!
//! GPU-to-GPU image copy via `vkCmdCopyImage`. Records barriers
//! `SHADER_READ_ONLY_OPTIMAL → TRANSFER_{SRC,DST}_OPTIMAL`, the
//! copy itself with one `VkImageCopy` region per GC-clipped
//! sub-rect, and reverse barriers back to
//! `SHADER_READ_ONLY_OPTIMAL`.
//!
//! Same-image copies (xterm-style scrollback or any `src == dst`
//! call) collapse the two transient layouts to `GENERAL` so a
//! single barrier covers both src and dst roles. The Vulkan spec
//! permits this when the regions don't overlap; the caller must
//! check overlap before calling.
//!
//! `vkCmdCopyImage` requires matching formats — caller filters
//! mismatched-format pairs to the pixman fallback.

use ash::vk;

use crate::kms::vk::{copy_scratch::CopyScratch, device::VkContext, target::DrawableImage};

/// Record a `vkCmdCopyImage` from `src` to `dst` (distinct
/// `DrawableImage`s — caller has verified `src.vk_image !=
/// dst.vk_image`). On return both `src.current_layout` and
/// `dst.current_layout` are `SHADER_READ_ONLY_OPTIMAL`.
///
/// Same-image copies use [`record_copy_area_same`]; the
/// borrow-checker forbids passing the same `&mut DrawableImage`
/// twice, and the same-image path collapses both barriers into
/// one.
pub fn record_copy_area_distinct(
    vk: &VkContext,
    cb: vk::CommandBuffer,
    src: &mut DrawableImage,
    dst: &mut DrawableImage,
    regions: &[vk::ImageCopy],
) -> Result<(), vk::Result> {
    if regions.is_empty() {
        return Ok(());
    }
    record_distinct_image_copy(&vk.device, cb, src, dst, regions);
    Ok(())
}

/// Record a `vkCmdCopyImage` with the same image as both src and
/// dst — caller guarantees the regions don't overlap (Vulkan UB
/// otherwise). Uses `GENERAL` as the transient layout.
pub fn record_copy_area_same(
    vk: &VkContext,
    cb: vk::CommandBuffer,
    img: &mut DrawableImage,
    regions: &[vk::ImageCopy],
) -> Result<(), vk::Result> {
    if regions.is_empty() {
        return Ok(());
    }
    record_same_image_copy(&vk.device, cb, img, regions);
    Ok(())
}

/// Same-image overlapping copy via a scratch hop. Records:
///   1. img → scratch (one `vkCmdCopyImage`, src offsets, scratch
///      bbox-relative dst offsets at `(0,0)`).
///   2. scratch → img (dst offsets).
///
/// Each step's regions are disjoint (no overlap between the
/// individual mirror image and the scratch image), so this works
/// for arbitrary src/dst overlap on the same drawable — the use
/// case is xterm scrollback.
///
/// `regions` is the list of mirror→mirror copies the caller would
/// have submitted. `bbox_origin` is the top-left of the rect that
/// scratch will be sized to fit (caller picks something that
/// covers all the source rects).
pub fn record_copy_area_same_overlap(
    vk: &VkContext,
    cb: vk::CommandBuffer,
    img: &mut DrawableImage,
    scratch: &mut CopyScratch,
    regions: &[vk::ImageCopy],
    bbox_origin: (i32, i32),
) -> Result<(), vk::Result> {
    if regions.is_empty() {
        return Ok(());
    }

    let device = &vk.device;
    let img_old = img.current_layout();

    // Pre: img → TRANSFER_SRC; scratch → TRANSFER_DST.
    let pre = [vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
        .src_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
        .dst_stage_mask(vk::PipelineStageFlags2::COPY)
        .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
        .old_layout(img_old)
        .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
        .image(img.vk_image)
        .subresource_range(color_subresource_range())];
    let pre_dep = vk::DependencyInfo::default().image_memory_barriers(&pre);
    crate::vk_count!(cmd_pipeline_barrier2);
    unsafe { device.cmd_pipeline_barrier2(cb, &pre_dep) };
    scratch.record_to_transfer_dst(cb);

    // Step 1: img → scratch. Each region's scratch dst is the src
    // offset minus `bbox_origin`, so the scratch holds a rectangle
    // packed at (0, 0).
    let to_scratch: Vec<vk::ImageCopy> = regions
        .iter()
        .map(|r| {
            vk::ImageCopy::default()
                .src_subresource(r.src_subresource)
                .src_offset(r.src_offset)
                .dst_subresource(r.dst_subresource)
                .dst_offset(vk::Offset3D {
                    x: r.src_offset.x - bbox_origin.0,
                    y: r.src_offset.y - bbox_origin.1,
                    z: 0,
                })
                .extent(r.extent)
        })
        .collect();
    unsafe {
        crate::vk_count!(cmd_copy_image);
        device.cmd_copy_image(
            cb,
            img.vk_image,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            scratch.image(),
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &to_scratch,
        );
    }

    // Mid: img → TRANSFER_DST; scratch → TRANSFER_SRC.
    let mid = [vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::COPY)
        .src_access_mask(vk::AccessFlags2::TRANSFER_READ)
        .dst_stage_mask(vk::PipelineStageFlags2::COPY)
        .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
        .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
        .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
        .image(img.vk_image)
        .subresource_range(color_subresource_range())];
    let mid_dep = vk::DependencyInfo::default().image_memory_barriers(&mid);
    crate::vk_count!(cmd_pipeline_barrier2);
    unsafe { device.cmd_pipeline_barrier2(cb, &mid_dep) };
    scratch.record_to_transfer_src(cb);

    // Step 2: scratch → img. Each region's scratch src is the same
    // bbox-relative rect we wrote in step 1.
    let from_scratch: Vec<vk::ImageCopy> = regions
        .iter()
        .map(|r| {
            vk::ImageCopy::default()
                .src_subresource(r.src_subresource)
                .src_offset(vk::Offset3D {
                    x: r.src_offset.x - bbox_origin.0,
                    y: r.src_offset.y - bbox_origin.1,
                    z: 0,
                })
                .dst_subresource(r.dst_subresource)
                .dst_offset(r.dst_offset)
                .extent(r.extent)
        })
        .collect();
    unsafe {
        crate::vk_count!(cmd_copy_image);
        device.cmd_copy_image(
            cb,
            scratch.image(),
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            img.vk_image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &from_scratch,
        );
    }

    // Post: img → SHADER_READ_ONLY_OPTIMAL.
    let post = [vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::COPY)
        .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
        .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
        .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
        .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
        .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
        .image(img.vk_image)
        .subresource_range(color_subresource_range())];
    let post_dep = vk::DependencyInfo::default().image_memory_barriers(&post);
    crate::vk_count!(cmd_pipeline_barrier2);
    unsafe { device.cmd_pipeline_barrier2(cb, &post_dep) };

    img.set_current_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
    Ok(())
}

fn record_distinct_image_copy(
    device: &ash::Device,
    cb: vk::CommandBuffer,
    src: &mut DrawableImage,
    dst: &mut DrawableImage,
    regions: &[vk::ImageCopy],
) {
    let src_old = src.current_layout();
    let dst_old = dst.current_layout();

    // Pre-copy: src → TRANSFER_SRC_OPTIMAL, dst → TRANSFER_DST_OPTIMAL.
    let pre = [
        vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
            .src_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
            .dst_stage_mask(vk::PipelineStageFlags2::COPY)
            .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
            .old_layout(src_old)
            .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .image(src.vk_image)
            .subresource_range(color_subresource_range()),
        vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
            .src_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
            .dst_stage_mask(vk::PipelineStageFlags2::COPY)
            .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
            .old_layout(dst_old)
            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .image(dst.vk_image)
            .subresource_range(color_subresource_range()),
    ];
    let pre_dep = vk::DependencyInfo::default().image_memory_barriers(&pre);
    crate::vk_count!(cmd_pipeline_barrier2);
    unsafe { device.cmd_pipeline_barrier2(cb, &pre_dep) };

    unsafe {
        crate::vk_count!(cmd_copy_image);
        device.cmd_copy_image(
            cb,
            src.vk_image,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            dst.vk_image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            regions,
        );
    }

    // Post-copy: both → SHADER_READ_ONLY_OPTIMAL.
    let post = [
        vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::COPY)
            .src_access_mask(vk::AccessFlags2::TRANSFER_READ)
            .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
            .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
            .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .image(src.vk_image)
            .subresource_range(color_subresource_range()),
        vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::COPY)
            .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
            .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .image(dst.vk_image)
            .subresource_range(color_subresource_range()),
    ];
    let post_dep = vk::DependencyInfo::default().image_memory_barriers(&post);
    crate::vk_count!(cmd_pipeline_barrier2);
    unsafe { device.cmd_pipeline_barrier2(cb, &post_dep) };

    src.set_current_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
    dst.set_current_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
}

fn record_same_image_copy(
    device: &ash::Device,
    cb: vk::CommandBuffer,
    img: &mut DrawableImage,
    regions: &[vk::ImageCopy],
) {
    // Single image acts as both src and dst. GENERAL is the
    // permissive layout that allows transfer-read AND
    // transfer-write on the same subresource. Vulkan spec
    // permits this when the regions don't overlap.
    let old_layout = img.current_layout();
    let pre = [vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
        .src_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
        .dst_stage_mask(vk::PipelineStageFlags2::COPY)
        .dst_access_mask(vk::AccessFlags2::TRANSFER_READ | vk::AccessFlags2::TRANSFER_WRITE)
        .old_layout(old_layout)
        .new_layout(vk::ImageLayout::GENERAL)
        .image(img.vk_image)
        .subresource_range(color_subresource_range())];
    let pre_dep = vk::DependencyInfo::default().image_memory_barriers(&pre);
    crate::vk_count!(cmd_pipeline_barrier2);
    unsafe { device.cmd_pipeline_barrier2(cb, &pre_dep) };

    unsafe {
        crate::vk_count!(cmd_copy_image);
        device.cmd_copy_image(
            cb,
            img.vk_image,
            vk::ImageLayout::GENERAL,
            img.vk_image,
            vk::ImageLayout::GENERAL,
            regions,
        );
    }

    let post = [vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::COPY)
        .src_access_mask(vk::AccessFlags2::TRANSFER_READ | vk::AccessFlags2::TRANSFER_WRITE)
        .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
        .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
        .old_layout(vk::ImageLayout::GENERAL)
        .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
        .image(img.vk_image)
        .subresource_range(color_subresource_range())];
    let post_dep = vk::DependencyInfo::default().image_memory_barriers(&post);
    crate::vk_count!(cmd_pipeline_barrier2);
    unsafe { device.cmd_pipeline_barrier2(cb, &post_dep) };

    img.set_current_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
}

fn color_subresource_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1)
}
