//! Text-run drawing op (sub-phase 4.1.4.5).
//!
//! Per-glyph quad draws into the target mirror, sampling the
//! shared [`GlyphAtlas`](super::super::glyph::GlyphAtlas) via the
//! [`TextPipeline`](super::super::text_pipeline::TextPipeline) and
//! premultiplied src-over blending. The glyphs the caller passes
//! in have already been packed into the atlas (the caller invokes
//! `GlyphAtlas::intern` for each glyph in the run before
//! recording; misses fall back to the pixman path).

use ash::vk;

use crate::kms::vk::{
    device::VkContext,
    target::DrawableImage,
    text_pipeline::{TextPipeline, TextPushConsts},
};

/// The minimal "paint-target" surface that
/// [`record_text_run`] needs. v1's `DrawableImage` and v2's
/// `Drawable` storage both implement this; the recorder doesn't
/// care which one is behind it (Stage 3 plan §"Cross-cutting" §1).
pub trait TextRunTarget {
    fn vk_image(&self) -> vk::Image;
    fn vk_image_view(&self) -> vk::ImageView;
    fn extent(&self) -> vk::Extent2D;
    fn current_layout(&self) -> vk::ImageLayout;
    fn set_current_layout(&mut self, layout: vk::ImageLayout);
}

impl TextRunTarget for DrawableImage {
    fn vk_image(&self) -> vk::Image {
        self.vk_image
    }
    fn vk_image_view(&self) -> vk::ImageView {
        self.vk_image_view
    }
    fn extent(&self) -> vk::Extent2D {
        self.extent
    }
    fn current_layout(&self) -> vk::ImageLayout {
        self.current_layout()
    }
    fn set_current_layout(&mut self, layout: vk::ImageLayout) {
        self.set_current_layout(layout);
    }
}

/// Record a text run into `target`. `foreground` is the GC's RGB
/// foreground in `[0..1]`; the shader multiplies it by the
/// sampled atlas alpha. Mirror is left in
/// `SHADER_READ_ONLY_OPTIMAL` on return.
///
/// The atlas image referenced by `pipeline`'s descriptor set must
/// be in `SHADER_READ_ONLY_OPTIMAL` at submission time. v1's
/// `GlyphAtlas::intern` enforces this via its inline upload path;
/// v2's `RenderEngine::image_text` enforces it via same-queue
/// submission ordering between the upload CB and this draw CB.
///
/// Single-scissor convenience wrapper that scopes the draws to the
/// full target extent. Callers that need per-rect picture-clip
/// scissoring (Stage 3d `render_composite_glyphs`) should reach for
/// [`record_text_run_scissored`] instead.
pub fn record_text_run<T: TextRunTarget + ?Sized>(
    vk: &VkContext,
    cb: vk::CommandBuffer,
    target: &mut T,
    atlas_extent: vk::Extent2D,
    pipeline: &TextPipeline,
    instance_buf: vk::Buffer,
    instance_offset: vk::DeviceSize,
    instance_count: u32,
    foreground: [f32; 4],
) -> Result<(), vk::Result> {
    let extent = target.extent();
    let full = [vk::Rect2D {
        offset: vk::Offset2D::default(),
        extent,
    }];
    record_text_run_scissored(
        vk,
        cb,
        target,
        atlas_extent,
        pipeline,
        instance_buf,
        instance_offset,
        // Core text is never split into runs: one buffer, one draw,
        // from instance 0.
        0,
        instance_count,
        foreground,
        &full,
    )
}

/// Per-rect-scissored sibling to [`record_text_run`]. Issues one
/// `cmd_set_scissor` + glyph-draw batch per element of `scissors`
/// inside a single render pass. Used by Stage 3d's
/// `composite_glyphs` to honour picture clip rectangles (plan §4 —
/// per-rect scissoring, not v1's union-bbox shortcut, which also
/// fixes v1's latent `_clip unused` bug).
///
/// `first_instance` / `instance_count` are the run's half-open range
/// into the instance data at `instance_offset` in `instance_buf`, which
/// may hold the instances of several runs of the same request (see
/// `RecordedCompositeGlyphs::first_instance`). `instance_offset` must be
/// 4-byte aligned: every instance attribute has 32-bit components.
///
/// If `scissors` is empty the function returns without recording
/// any draw (matches "every clip rect culled" semantics in
/// `RenderEngine::render_composite`).
pub fn record_text_run_scissored<T: TextRunTarget + ?Sized>(
    vk: &VkContext,
    cb: vk::CommandBuffer,
    target: &mut T,
    atlas_extent: vk::Extent2D,
    pipeline: &TextPipeline,
    instance_buf: vk::Buffer,
    instance_offset: vk::DeviceSize,
    first_instance: u32,
    instance_count: u32,
    foreground: [f32; 4],
    scissors: &[vk::Rect2D],
) -> Result<(), vk::Result> {
    // Zero-instance fast path (#1 codex review): don't touch the CB if
    // the run has no glyph instances or every clip rect is culled.
    if instance_count == 0
        || scissors.is_empty()
        || target.extent().width == 0
        || target.extent().height == 0
    {
        return Ok(());
    }

    let device = &vk.device;
    let old_layout = target.current_layout();
    let extent = target.extent();
    let target_image = target.vk_image();
    let target_view = target.vk_image_view();

    let to_color = [vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
        .src_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
        .dst_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
        .dst_access_mask(
            vk::AccessFlags2::COLOR_ATTACHMENT_WRITE | vk::AccessFlags2::COLOR_ATTACHMENT_READ,
        )
        .old_layout(old_layout)
        .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .image(target_image)
        .subresource_range(color_subresource_range())];
    let to_color_dep = vk::DependencyInfo::default().image_memory_barriers(&to_color);
    crate::vk_count!(cmd_pipeline_barrier2);
    unsafe { device.cmd_pipeline_barrier2(cb, &to_color_dep) };

    let render_area = vk::Rect2D {
        offset: vk::Offset2D::default(),
        extent,
    };
    let color_attachment = [vk::RenderingAttachmentInfo::default()
        .image_view(target_view)
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
        width: extent.width as f32,
        height: extent.height as f32,
        min_depth: 0.0,
        max_depth: 1.0,
    }];
    // Per-RUN push constants (#1): viewport + atlas extent (UV divisor,
    // read from the current atlas) + foreground. Per-glyph dst/atlas
    // rects ride in the instance vertex buffer.
    let push_consts = TextPushConsts::new(
        [extent.width as f32, extent.height as f32],
        [atlas_extent.width as f32, atlas_extent.height as f32],
        foreground,
    );
    unsafe {
        crate::vk_count!(cmd_begin_rendering);
        device.cmd_begin_rendering(cb, &rendering_info);
        crate::vk_count!(cmd_set_viewport);
        device.cmd_set_viewport(cb, 0, &viewport);
        crate::vk_count!(cmd_bind_pipeline);
        device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, pipeline.pipeline);
        crate::vk_count!(cmd_bind_descriptor_sets);
        device.cmd_bind_descriptor_sets(
            cb,
            vk::PipelineBindPoint::GRAPHICS,
            pipeline.pipeline_layout,
            0,
            &[pipeline.descriptor_set],
            &[],
        );
        // Per-instance glyph data: one INSTANCE-rate binding shared
        // across all scissor rects; the 4-vertex quad comes from
        // gl_VertexIndex. (No vk_count field for bind_vertex_buffers,
        // matching the trapezoid emit path.)
        device.cmd_bind_vertex_buffers(cb, 0, &[instance_buf], &[instance_offset]);
        crate::vk_count!(cmd_push_constants);
        device.cmd_push_constants(
            cb,
            pipeline.pipeline_layout,
            vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
            0,
            push_consts.as_bytes(),
        );

        // One instanced draw of this run's glyphs per clip rect.
        // Instance order == glyph order (preserves blend order for
        // non-additive ops); the same buffer is re-read per scissor.
        //
        // `first_instance` is the run's offset into the shared instance
        // buffer: `[first_instance, first_instance + instance_count)`.
        // A `CompositeGlyphs` request recorded as several runs pins one
        // buffer and hands each run its own range, so leaving this at
        // zero would draw run 0's glyphs for every run.
        for scissor_rect in scissors {
            let scissor = [*scissor_rect];
            crate::vk_count!(cmd_set_scissor);
            device.cmd_set_scissor(cb, 0, &scissor);
            crate::vk_count!(cmd_draw);
            device.cmd_draw(cb, 4, instance_count, 0, first_instance);
        }

        crate::vk_count!(cmd_end_rendering);
        device.cmd_end_rendering(cb);
    }

    let to_read = [vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
        .src_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
        .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
        .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
        .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
        .image(target_image)
        .subresource_range(color_subresource_range())];
    let to_read_dep = vk::DependencyInfo::default().image_memory_barriers(&to_read);
    crate::vk_count!(cmd_pipeline_barrier2);
    unsafe { device.cmd_pipeline_barrier2(cb, &to_read_dep) };

    target.set_current_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
    Ok(())
}

fn color_subresource_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1)
}
