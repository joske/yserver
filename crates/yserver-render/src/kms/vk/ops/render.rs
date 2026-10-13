//! RENDER `Composite` + `FillRectangles` recorder (sub-phase
//! 4.1.4.6, commit 1).
//!
//! Records a per-op pipelined draw into the destination mirror.
//! The caller has already:
//!   * picked a [`StdPictOp`] from the request and resolved the
//!     pipeline via [`RenderPipelineCache::get`],
//!   * resolved the source — either a Drawable's mirror (sampled
//!     directly) or a `SolidFill` whose colour we cleared into a
//!     backend-shared 1×1 image,
//!   * allocated a descriptor set bound to the source view + the
//!     pipeline's linear sampler,
//!   * computed clip scissors (Region clip) — empty Vec means
//!     "no clip" so we use a full-mirror scissor.
//!
//! The recorder transitions the destination mirror through
//! `SHADER_READ_ONLY_OPTIMAL → COLOR_ATTACHMENT_OPTIMAL → back`,
//! `cmd_begin_rendering` with `LOAD/STORE`, sets the per-rect
//! viewport+scissor, binds the pipeline + descriptor set, push-
//! constants the dst+src UV rect, and `cmd_draw(4, 1)` per call.
//!
//! Mask + alpha_map + transform + component_alpha + R8 mask
//! support layer in across the follow-up commits in this slot.

use ash::vk;

use crate::kms::vk::{
    device::VkContext,
    render_pipeline::{REPEAT_FORCE_OPAQUE_BIT, RenderPushConsts},
    target::DrawableImage,
};

/// Minimal paint-target surface that [`record_render_composite`]
/// reads / mutates. Lets the recorder operate against either v1's
/// `DrawableImage` (per-window mirror) or v2's `Drawable` storage
/// (via an adapter) without depending on either type's full shape.
/// Stage 3 plan §"Cross-cutting" §1.
pub trait CompositeTarget {
    fn vk_image(&self) -> vk::Image;
    fn vk_image_view(&self) -> vk::ImageView;
    fn extent(&self) -> vk::Extent2D;
    fn current_layout(&self) -> vk::ImageLayout;
    fn set_current_layout(&mut self, layout: vk::ImageLayout);
}

impl CompositeTarget for DrawableImage {
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

/// One Composite request sub-rect. `src_x`/`src_y` and
/// `mask_x`/`mask_y` are the source / mask space pixel offsets
/// corresponding to `dst_x`/`dst_y`; the fragment shader applies the
/// per-picture affine transform on top of those.
#[derive(Debug, Clone, Copy)]
pub struct CompositeRect {
    pub src_x: i32,
    pub src_y: i32,
    pub mask_x: i32,
    pub mask_y: i32,
    pub dst_x: i32,
    pub dst_y: i32,
    pub width: u32,
    pub height: u32,
}

/// Affine 2×3 transform packed for the shader (rows of an X11 RENDER
/// 3×3 matrix dropped to two affine rows). For the identity transform,
/// the shader collapses to source-pixel = dst-offset + origin so the
/// "no transform" path is `IDENTITY`.
#[derive(Debug, Clone, Copy)]
pub struct AffineXform {
    pub row0: [f32; 4], // (a, b, tx, _)
    pub row1: [f32; 4], // (c, d, ty, _)
}

impl AffineXform {
    pub const IDENTITY: AffineXform = AffineXform {
        row0: [1.0, 0.0, 0.0, 0.0],
        row1: [0.0, 1.0, 0.0, 0.0],
    };
}

/// Pack a repeat-mode constant and the force-opaque flag into the
/// `i32` slot the shader reads from `RenderPushConsts::repeat_modes[i]`.
/// Bit layout is documented next to [`REPEAT_FORCE_OPAQUE_BIT`].
///
/// Exposed (and tested) so the encoding is greppable from both
/// sides — Rust pushers and the GLSL shader. Inlines to a single
/// `or` at the call site in `record_render_composite`.
#[inline]
#[must_use]
pub fn pack_repeat_mode(repeat: i32, force_opaque: bool) -> i32 {
    repeat | (i32::from(force_opaque) * REPEAT_FORCE_OPAQUE_BIT)
}

/// Per-Composite-call attribute bundle.
#[derive(Debug, Clone, Copy)]
pub struct CompositeAttrs {
    pub src_extent: ash::vk::Extent2D,
    pub mask_extent: ash::vk::Extent2D,
    /// #133 step 3 (P4) — texel offset added to every rect's source
    /// sampling origin: the picture drawable's content origin inside
    /// the sampled storage. A window's content sits `bw` inside its
    /// storage (`compAllocPixmap`, `composite/compalloc.c:610`), and
    /// Xorg's fb path folds the same shift into its sampling offset
    /// (`fb/fbpict.c:328-329` adds `pict->pDrawable->x/y`).
    /// `[0, 0]` for pixmaps, synthetic sources and `bw == 0` windows,
    /// which is every pre-#133 case.
    pub src_offset: [i32; 2],
    /// [`Self::src_offset`] for the mask picture.
    pub mask_offset: [i32; 2],
    pub src_repeat: i32,
    pub mask_repeat: i32,
    /// X11 RENDER PictFormat-driven force-opaque flag for the src
    /// picture. When the source picture's format has
    /// `alpha_mask == 0` (e.g. a depth-24 RGB visual), the spec
    /// requires samples to return `α = 1.0` regardless of the
    /// byte content of the underlying storage — the alpha byte
    /// is server-owned padding. The shader-side check honours
    /// the spec even when the image-view swizzle alone cannot
    /// (self-alias scratch paths, picture formats that disagree
    /// with the drawable depth, etc.). Packed into the upper
    /// bits of `RenderPushConsts::repeat_modes[0]` — see
    /// `REPEAT_FORCE_OPAQUE_BIT`.
    pub src_force_opaque: bool,
    /// Same shape as [`Self::src_force_opaque`] for the mask
    /// picture. Depth-24 masks are rare in practice (most masks
    /// are A8 picture formats), but the spec semantics are
    /// identical so we keep the plumbing symmetric.
    pub mask_force_opaque: bool,
    pub src_xform: AffineXform,
    pub mask_xform: AffineXform,
}

/// Record `rects` worth of Composite draws into `dst`. The
/// descriptor set has `src_tex` at binding 0 and `mask_tex` at
/// binding 1; both must already be in `SHADER_READ_ONLY_OPTIMAL`
/// (caller arranges that — Drawable mirrors are already there
/// from the last composite, SolidFill scratch is transitioned by
/// the caller's clear sequence, the white-mask scratch is
/// transitioned once at backend init and stays put).
#[allow(clippy::too_many_arguments)]
pub fn record_render_composite<T: CompositeTarget + ?Sized>(
    vk: &VkContext,
    cb: vk::CommandBuffer,
    dst: &mut T,
    pipeline: vk::Pipeline,
    pipeline_layout: vk::PipelineLayout,
    descriptor_set: vk::DescriptorSet,
    attrs: &CompositeAttrs,
    rects: &[CompositeRect],
    clip_scissors: &[vk::Rect2D],
) -> Result<(), vk::Result> {
    let extent = dst.extent();
    if rects.is_empty() || clip_scissors.is_empty() || extent.width == 0 || extent.height == 0 {
        return Ok(());
    }
    record_render_composite_open(vk, cb, dst, pipeline)?;
    record_render_composite_draws(
        vk,
        cb,
        pipeline_layout,
        descriptor_set,
        extent,
        attrs,
        rects,
        clip_scissors,
    );
    record_render_composite_close(vk, cb, dst);
    Ok(())
}

/// Stage 5 Task 3 (render-composite generalization): "open"
/// phase of a render-composite CB recording. Records the layout
/// barrier into `COLOR_ATTACHMENT_OPTIMAL`, opens
/// `cmd_begin_rendering`, sets viewport, binds the pipeline.
/// Counterpart to [`record_render_composite_close`]; the draws
/// between are recorded via [`record_render_composite_draws`]
/// which binds the descriptor set per-call (so each append in a
/// batched CB can have its own descriptor — different src /
/// mask views across same-pipeline appends).
///
/// For the batched path: this is called once at batch-open
/// time; subsequent appends only call `_draws`; `_close` runs
/// once at flush.
pub fn record_render_composite_open<T: CompositeTarget + ?Sized>(
    vk: &VkContext,
    cb: vk::CommandBuffer,
    dst: &mut T,
    pipeline: vk::Pipeline,
) -> Result<(), vk::Result> {
    let old_layout = dst.current_layout();
    record_render_composite_open_with_old_layout(vk, cb, dst, pipeline, old_layout)
}

/// Phase B.2 Task 12: same shape as [`record_render_composite_open`]
/// but takes `old_layout` explicitly instead of reading
/// `dst.current_layout()`. Used by the v2 frame builder so the
/// overlay's `current_in_frame_layout` (resolved at op-append
/// time) drives the `to_color` barrier instead of
/// `dst.current_layout()` (which reflects `storage.current_layout`
/// and is stale during deferred recording — see Pitfall 5 in
/// `docs/superpowers/plans/2026-05-24-frame-builder-phase-b2.md`).
///
/// **Non-mutating contract:** this overload does NOT call
/// `dst.set_current_layout` anywhere — the `dst: &T` shared
/// reference makes that mechanical. Storage layout commit under
/// B.2 deferred recording happens via `commit_close_success`
/// (engine.rs) reading the frame overlay back into
/// `Drawable::storage.current_layout` on a successful submit.
pub fn record_render_composite_open_with_old_layout<T: CompositeTarget + ?Sized>(
    vk: &VkContext,
    cb: vk::CommandBuffer,
    dst: &T,
    pipeline: vk::Pipeline,
    old_layout: vk::ImageLayout,
) -> Result<(), vk::Result> {
    let extent = dst.extent();
    let device = &vk.device;
    let dst_image = dst.vk_image();
    let dst_view = dst.vk_image_view();

    let to_color = [vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
        .src_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
        .dst_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
        .dst_access_mask(
            vk::AccessFlags2::COLOR_ATTACHMENT_WRITE | vk::AccessFlags2::COLOR_ATTACHMENT_READ,
        )
        .old_layout(old_layout)
        .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .image(dst_image)
        .subresource_range(color_subresource_range())];
    let to_color_dep = vk::DependencyInfo::default().image_memory_barriers(&to_color);
    crate::vk_count!(cmd_pipeline_barrier2);
    unsafe { device.cmd_pipeline_barrier2(cb, &to_color_dep) };

    let render_area = vk::Rect2D {
        offset: vk::Offset2D::default(),
        extent,
    };
    let color_attachment = [vk::RenderingAttachmentInfo::default()
        .image_view(dst_view)
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
    unsafe {
        crate::vk_count!(cmd_begin_rendering);
        device.cmd_begin_rendering(cb, &rendering_info);
        crate::vk_count!(cmd_set_viewport);
        device.cmd_set_viewport(cb, 0, &viewport);
        crate::vk_count!(cmd_bind_pipeline);
        device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, pipeline);
    }
    Ok(())
}

/// Stage 5 Task 3 (render-composite generalization): per-append
/// draw recording. Binds the call's descriptor set (so each
/// append in a batched CB can carry its own src / mask views),
/// then for every (clip_scissor × rect) pair records
/// `cmd_set_scissor` + per-rect `cmd_push_constants` +
/// `cmd_draw(4, 1)`. Caller must have run
/// [`record_render_composite_open`] first; pipeline must still
/// be bound (same op + format + alpha across the batch).
pub fn record_render_composite_draws(
    vk: &VkContext,
    cb: vk::CommandBuffer,
    pipeline_layout: vk::PipelineLayout,
    descriptor_set: vk::DescriptorSet,
    extent: vk::Extent2D,
    attrs: &CompositeAttrs,
    rects: &[CompositeRect],
    clip_scissors: &[vk::Rect2D],
) {
    if rects.is_empty() || clip_scissors.is_empty() {
        return;
    }
    let device = &vk.device;
    unsafe {
        crate::vk_count!(cmd_bind_descriptor_sets);
        device.cmd_bind_descriptor_sets(
            cb,
            vk::PipelineBindPoint::GRAPHICS,
            pipeline_layout,
            0,
            &[descriptor_set],
            &[],
        );
    }
    let dst_vp = [extent.width as f32, extent.height as f32];
    let src_extent_px = [
        attrs.src_extent.width as f32,
        attrs.src_extent.height as f32,
    ];
    let mask_extent_px = [
        attrs.mask_extent.width as f32,
        attrs.mask_extent.height as f32,
    ];

    // Plan §4: one draw call per clip-rect intersection. Set
    // scissor, issue every rect's draw, set next scissor, ...
    // Multi-rect picture clips (regions with holes) are
    // honoured exactly rather than via v1's union-bbox shortcut.
    //
    // Each clip_scissor is clamped to the render area so the
    // unclipped path can pass `Extent2D { width: u32::MAX,
    // height: u32::MAX }` without tripping Vulkan validation
    // (`offset.x + extent.width` overflow), which some drivers
    // (lavapipe) handle by silently dropping the draw.
    for cs in clip_scissors {
        let clamped = [vk::Rect2D {
            offset: cs.offset,
            extent: vk::Extent2D {
                width: cs
                    .extent
                    .width
                    .min(extent.width.saturating_sub(cs.offset.x.max(0) as u32)),
                height: cs
                    .extent
                    .height
                    .min(extent.height.saturating_sub(cs.offset.y.max(0) as u32)),
            },
        }];
        if clamped[0].extent.width == 0 || clamped[0].extent.height == 0 {
            continue;
        }
        unsafe {
            crate::vk_count!(cmd_set_scissor);
            device.cmd_set_scissor(cb, 0, &clamped);
            for r in rects {
                let pc = RenderPushConsts {
                    dst_origin: [r.dst_x as f32, r.dst_y as f32],
                    dst_size: [r.width as f32, r.height as f32],
                    viewport: dst_vp,
                    // #133 step 3 (P4): fold each picture's content
                    // origin into the sampling origin here — ONE place
                    // for every composite path (unbatched, batched and
                    // both deferred emits, plus the trapezoid/triangle
                    // composite stage, which all funnel through this
                    // recorder). The push-constant block is already at
                    // the 128-byte `maxPushConstantsSize` minimum, so
                    // this cannot become a separate uniform.
                    src_origin: [
                        (r.src_x + attrs.src_offset[0]) as f32,
                        (r.src_y + attrs.src_offset[1]) as f32,
                    ],
                    mask_origin: [
                        (r.mask_x + attrs.mask_offset[0]) as f32,
                        (r.mask_y + attrs.mask_offset[1]) as f32,
                    ],
                    src_extent: src_extent_px,
                    mask_extent: mask_extent_px,
                    repeat_modes: [
                        pack_repeat_mode(attrs.src_repeat, attrs.src_force_opaque),
                        pack_repeat_mode(attrs.mask_repeat, attrs.mask_force_opaque),
                    ],
                    src_xform_row0: attrs.src_xform.row0,
                    src_xform_row1: attrs.src_xform.row1,
                    mask_xform_row0: attrs.mask_xform.row0,
                    mask_xform_row1: attrs.mask_xform.row1,
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
        }
    }
}

/// Stage 5 Task 3 (render-composite generalization): "close"
/// phase of a render-composite CB recording. Closes
/// `cmd_end_rendering` and records the dst layout barrier back
/// to `SHADER_READ_ONLY_OPTIMAL`. Counterpart to
/// [`record_render_composite_open`].
pub fn record_render_composite_close<T: CompositeTarget + ?Sized>(
    vk: &VkContext,
    cb: vk::CommandBuffer,
    dst: &mut T,
) {
    let device = &vk.device;
    let dst_image = dst.vk_image();
    unsafe {
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
        .image(dst_image)
        .subresource_range(color_subresource_range())];
    let to_read_dep = vk::DependencyInfo::default().image_memory_barriers(&to_read);
    crate::vk_count!(cmd_pipeline_barrier2);
    unsafe { device.cmd_pipeline_barrier2(cb, &to_read_dep) };

    dst.set_current_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
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
    use crate::kms::vk::render_pipeline::{
        REPEAT_FORCE_OPAQUE_BIT, REPEAT_MODE_MASK, REPEAT_NONE, REPEAT_NORMAL, REPEAT_PAD,
        REPEAT_REFLECT,
    };

    /// X11 Render `PictFormat` fix: the force-opaque flag must pack
    /// into the upper bits of `repeat_modes[i]` (bit 8) without
    /// aliasing any of the 4 repeat-mode constants
    /// (`REPEAT_NONE` = 0 .. `REPEAT_REFLECT` = 3). Verifies the
    /// encoding contract the GLSL shader relies on — both sides
    /// must agree on the bit layout or the shader will sample
    /// garbage as the force-opaque hint.
    #[test]
    fn composite_attrs_packs_force_opaque_into_repeat_modes_upper_bits() {
        // REPEAT_PAD encodes as the bare constant when force_opaque
        // is false — the shader's `repeat & REPEAT_MODE_MASK` must
        // recover the original repeat.
        assert_eq!(pack_repeat_mode(REPEAT_PAD, false), REPEAT_PAD);
        assert_eq!(
            pack_repeat_mode(REPEAT_PAD, false) & REPEAT_MODE_MASK,
            REPEAT_PAD
        );
        assert_eq!(
            pack_repeat_mode(REPEAT_PAD, false) & REPEAT_FORCE_OPAQUE_BIT,
            0
        );

        // With force_opaque, bit 8 is set; the repeat mode in the
        // low byte is preserved.
        let pad_opaque = pack_repeat_mode(REPEAT_PAD, true);
        assert_eq!(pad_opaque, REPEAT_PAD | (1 << 8));
        assert_eq!(pad_opaque & REPEAT_MODE_MASK, REPEAT_PAD);
        assert_eq!(
            pad_opaque & REPEAT_FORCE_OPAQUE_BIT,
            REPEAT_FORCE_OPAQUE_BIT
        );

        // Each of the 4 repeat constants survives the round-trip
        // unchanged regardless of the force-opaque bit.
        for r in [REPEAT_NONE, REPEAT_NORMAL, REPEAT_PAD, REPEAT_REFLECT] {
            for force in [false, true] {
                let packed = pack_repeat_mode(r, force);
                assert_eq!(packed & REPEAT_MODE_MASK, r);
                assert_eq!(
                    (packed & REPEAT_FORCE_OPAQUE_BIT) != 0,
                    force,
                    "force_opaque bit round-trip failed for r={r}, force={force}"
                );
            }
        }
    }
}
