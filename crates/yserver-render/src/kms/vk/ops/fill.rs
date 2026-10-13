//! Solid-fill drawing op (sub-phase 4.1.4.1).
//!
//! Backs `PolyFillRectangle` and `ClearArea`. Both are rect lists
//! filled with a single color — Vulkan models them naturally as
//! `vkCmdClearAttachments` inside an active render pass:
//! `cmd_clear_attachments` honours the bound scissor (so GC
//! `clip-rectangles` are a `cmd_set_scissor` away) and writes only
//! to the color attachment — no pipeline / shader / descriptor set
//! needed.
//!
//! Caller pattern:
//!
//! ```ignore
//! backend.with_ops_cb(|cb| {
//!     fill::record_fill_rectangles(
//!         vk, cb, &mut window.vk_mirror.unwrap(),
//!         color, &rects, clip_scissor,
//!     )
//! })?;
//! ```
//!
//! `with_ops_cb` allocates the CB, submits, and waits idle — see
//! `kms::backend::KmsBackend::with_ops_cb`. The mirror is left in
//! `SHADER_READ_ONLY_OPTIMAL` after this call so the next composite
//! pass can sample it.

use ash::vk;

use crate::kms::vk::{
    device::VkContext, logic_fill_pipeline::LogicFillPushConsts, target::DrawableImage,
};

/// Record a solid-fill of `rects` (in mirror-local pixel coords)
/// into `target`, in the GC's foreground color. Each rect is
/// clipped to `clip_scissor` (which the caller has already
/// intersected with the mirror extent).
///
/// `target.current_layout` is updated to
/// `SHADER_READ_ONLY_OPTIMAL` on return; if recording fails
/// partway, the layout is left at whatever transition we'd
/// committed to so far — caller treats partial failure as a
/// scuffed frame and moves on.
pub fn record_fill_rectangles(
    vk: &VkContext,
    cb: vk::CommandBuffer,
    target: &mut DrawableImage,
    color: [f32; 4],
    rects: &[vk::Rect2D],
    clip_scissor: vk::Rect2D,
) -> Result<(), vk::Result> {
    if rects.is_empty() || target.extent.width == 0 || target.extent.height == 0 {
        return Ok(());
    }

    let device = &vk.device;
    let old_layout = target.current_layout();

    // 1. Transition mirror into COLOR_ATTACHMENT_OPTIMAL.
    let to_color = vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
        .src_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
        .dst_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
        .dst_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
        .old_layout(old_layout)
        .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .image(target.vk_image)
        .subresource_range(
            vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .level_count(1)
                .layer_count(1),
        );
    let to_color_arr = [to_color];
    let to_color_dep = vk::DependencyInfo::default().image_memory_barriers(&to_color_arr);
    crate::vk_count!(cmd_pipeline_barrier2);
    unsafe { device.cmd_pipeline_barrier2(cb, &to_color_dep) };

    // 2. Begin rendering (loadOp=LOAD: preserve existing pixels;
    //    we only want to fill the listed rects).
    let render_area = vk::Rect2D {
        offset: vk::Offset2D::default(),
        extent: target.extent,
    };
    let color_attachment = [vk::RenderingAttachmentInfo::default()
        .image_view(target.vk_image_view)
        .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .load_op(vk::AttachmentLoadOp::LOAD)
        .store_op(vk::AttachmentStoreOp::STORE)];
    let rendering_info = vk::RenderingInfo::default()
        .render_area(render_area)
        .layer_count(1)
        .color_attachments(&color_attachment);
    unsafe {
        crate::vk_count!(cmd_begin_rendering);
        device.cmd_begin_rendering(cb, &rendering_info);

        // 3. Viewport = full mirror; scissor = GC clip.
        let viewport = [vk::Viewport {
            x: 0.0,
            y: 0.0,
            width: target.extent.width as f32,
            height: target.extent.height as f32,
            min_depth: 0.0,
            max_depth: 1.0,
        }];
        crate::vk_count!(cmd_set_viewport);
        device.cmd_set_viewport(cb, 0, &viewport);
        let scissor = [clip_scissor];
        crate::vk_count!(cmd_set_scissor);
        device.cmd_set_scissor(cb, 0, &scissor);

        // 4. Solid-fill: cmd_clear_attachments with one VkClearRect
        //    per request rect.
        let attachments = [vk::ClearAttachment::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .color_attachment(0)
            .clear_value(vk::ClearValue {
                color: vk::ClearColorValue { float32: color },
            })];
        let clear_rects: Vec<vk::ClearRect> = rects
            .iter()
            .map(|r| {
                vk::ClearRect::default()
                    .rect(*r)
                    .base_array_layer(0)
                    .layer_count(1)
            })
            .collect();
        device.cmd_clear_attachments(cb, &attachments, &clear_rects);

        crate::vk_count!(cmd_end_rendering);
        device.cmd_end_rendering(cb);
    }

    // 5. Transition back into SHADER_READ_ONLY_OPTIMAL for the
    //    next composite read.
    let to_read = vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
        .src_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
        .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
        .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
        .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
        .image(target.vk_image)
        .subresource_range(
            vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .level_count(1)
                .layer_count(1),
        );
    let to_read_arr = [to_read];
    let to_read_dep = vk::DependencyInfo::default().image_memory_barriers(&to_read_arr);
    crate::vk_count!(cmd_pipeline_barrier2);
    unsafe { device.cmd_pipeline_barrier2(cb, &to_read_dep) };

    target.set_current_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
    Ok(())
}

/// Solid-fill `rects` in `target` with `fg_color` through a
/// `VkLogicOp`-bearing pipeline. Used when the GC `function` isn't
/// `Copy` — the X11 bitwise GC functions (Xor / And / Or / Invert /
/// etc.) all map 1:1 to a `VkLogicOp` and the per-function pipeline
/// the cache built. `clip_scissor` is the same coarse scissor the
/// `Copy` path uses.
///
/// Each rect is drawn as a 4-vertex triangle strip; the per-rect
/// scissor is set to `clip_scissor ∩ rect` so we don't paint past
/// the rect bounds, even when the pipeline's blend/logic is
/// permissive.
pub fn record_logic_fill(
    vk: &VkContext,
    cb: vk::CommandBuffer,
    target: &mut DrawableImage,
    pipeline: vk::Pipeline,
    pipeline_layout: vk::PipelineLayout,
    fg_color: [f32; 4],
    rects: &[vk::Rect2D],
    clip_scissor: vk::Rect2D,
) -> Result<(), vk::Result> {
    if rects.is_empty() || target.extent.width == 0 || target.extent.height == 0 {
        return Ok(());
    }

    let device = &vk.device;
    let old_layout = target.current_layout();

    let to_color = vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
        .src_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
        .dst_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
        .dst_access_mask(
            vk::AccessFlags2::COLOR_ATTACHMENT_WRITE | vk::AccessFlags2::COLOR_ATTACHMENT_READ,
        )
        .old_layout(old_layout)
        .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .image(target.vk_image)
        .subresource_range(
            vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .level_count(1)
                .layer_count(1),
        );
    let to_color_arr = [to_color];
    let to_color_dep = vk::DependencyInfo::default().image_memory_barriers(&to_color_arr);
    crate::vk_count!(cmd_pipeline_barrier2);
    unsafe { device.cmd_pipeline_barrier2(cb, &to_color_dep) };

    let render_area = vk::Rect2D {
        offset: vk::Offset2D::default(),
        extent: target.extent,
    };
    let color_attachment = [vk::RenderingAttachmentInfo::default()
        .image_view(target.vk_image_view)
        .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .load_op(vk::AttachmentLoadOp::LOAD)
        .store_op(vk::AttachmentStoreOp::STORE)];
    let rendering_info = vk::RenderingInfo::default()
        .render_area(render_area)
        .layer_count(1)
        .color_attachments(&color_attachment);

    let viewport = [vk::Viewport {
        x: 0.0,
        y: 0.0,
        width: target.extent.width as f32,
        height: target.extent.height as f32,
        min_depth: 0.0,
        max_depth: 1.0,
    }];

    unsafe {
        crate::vk_count!(cmd_begin_rendering);
        device.cmd_begin_rendering(cb, &rendering_info);
        crate::vk_count!(cmd_set_viewport);
        device.cmd_set_viewport(cb, 0, &viewport);
        crate::vk_count!(cmd_bind_pipeline);
        device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, pipeline);

        let dst_vp = [target.extent.width as f32, target.extent.height as f32];
        for r in rects {
            // Per-rect scissor = clip ∩ rect, in mirror coords.
            let rect_x0 = r.offset.x;
            let rect_y0 = r.offset.y;
            let rect_x1 = rect_x0 + r.extent.width as i32;
            let rect_y1 = rect_y0 + r.extent.height as i32;
            let clip_x0 = clip_scissor.offset.x;
            let clip_y0 = clip_scissor.offset.y;
            let clip_x1 = clip_scissor
                .offset
                .x
                .saturating_add(clip_scissor.extent.width as i32);
            let clip_y1 = clip_scissor
                .offset
                .y
                .saturating_add(clip_scissor.extent.height as i32);
            let sx0 = rect_x0.max(clip_x0).max(0);
            let sy0 = rect_y0.max(clip_y0).max(0);
            let sx1 = rect_x1.min(clip_x1).min(target.extent.width as i32);
            let sy1 = rect_y1.min(clip_y1).min(target.extent.height as i32);
            if sx1 <= sx0 || sy1 <= sy0 {
                continue;
            }
            let scissor = [vk::Rect2D {
                offset: vk::Offset2D { x: sx0, y: sy0 },
                extent: vk::Extent2D {
                    width: (sx1 - sx0) as u32,
                    height: (sy1 - sy0) as u32,
                },
            }];
            crate::vk_count!(cmd_set_scissor);
            device.cmd_set_scissor(cb, 0, &scissor);

            let pc = LogicFillPushConsts {
                dst_origin: [rect_x0 as f32, rect_y0 as f32],
                dst_size: [r.extent.width as f32, r.extent.height as f32],
                viewport: dst_vp,
                _pad: [0.0, 0.0],
                fg_color,
            };
            crate::vk_count!(cmd_push_constants);
            device.cmd_push_constants(
                cb,
                pipeline_layout,
                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                0,
                pc.as_bytes(),
            );
            crate::vk_count!(cmd_draw);
            device.cmd_draw(cb, 4, 1, 0, 0);
        }

        crate::vk_count!(cmd_end_rendering);
        device.cmd_end_rendering(cb);
    }

    let to_read = vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
        .src_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
        .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
        .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
        .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
        .image(target.vk_image)
        .subresource_range(
            vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .level_count(1)
                .layer_count(1),
        );
    let to_read_arr = [to_read];
    let to_read_dep = vk::DependencyInfo::default().image_memory_barriers(&to_read_arr);
    crate::vk_count!(cmd_pipeline_barrier2);
    unsafe { device.cmd_pipeline_barrier2(cb, &to_read_dep) };

    target.set_current_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
    Ok(())
}
