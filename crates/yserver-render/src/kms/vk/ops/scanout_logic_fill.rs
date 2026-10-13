//! Root-overlay XOR pass recorded INTO an already-active compose
//! rendering instance (root-`IncludeInferiors` feature, Task 6).
//!
//! Unlike [`super::fill::record_logic_fill`], this recorder does NOT
//! begin/end its own dynamic-rendering instance and does NOT emit any
//! image-layout barriers: the scanout BO is already in
//! `COLOR_ATTACHMENT_OPTIMAL` and `vkCmdBeginRendering` is already
//! active (the scene compositor calls us between its scene draws and
//! its `cmd_end_rendering`). We only mirror the DRAW half of
//! `record_logic_fill`: bind the XOR-logic-op pipeline, then per op set
//! the scissor to the output-local rect, push the geometry + decoded
//! color, and draw a 4-vertex triangle-strip quad.
//!
//! The pipeline is expected to be the `(GcFunction::Xor, opaque_alpha =
//! true)` variant from [`crate::kms::vk::logic_fill_pipeline`], whose
//! color write-mask drops alpha (RGB-only) — the server-owned-α
//! behaviour we want on the depth-24 scanout. One such pipeline serves
//! both `Invert` (value = plane_mask) and `Xor` (value = fg) since the
//! per-pixel operand is already folded into the `xor_value`.

use ash::vk;

use crate::kms::vk::{device::VkContext, logic_fill_pipeline::LogicFillPushConsts};

/// Record the retained root-overlay XOR ops into the currently-active
/// compose rendering instance on `cb`.
///
/// `viewport` is the full scanout-BO extent in pixels
/// (`[width, height]`) — the vertex shader needs it to map the
/// output-local rect into NDC (identical to `record_logic_fill`'s
/// `dst_vp`). Each `ops` entry is `(xor_value, output-local scissor
/// rect)`; the rect is used verbatim as both the per-draw scissor and
/// the quad geometry (`apply_list_for_output` has already clipped it to
/// the output bounds).
///
/// No return value: this records into a caller-owned CB that the caller
/// submits; there is nothing to fail here beyond the raw command
/// recording (which cannot return an error).
pub fn record_scanout_logic_fill(
    vk: &VkContext,
    cb: vk::CommandBuffer,
    pipeline: vk::Pipeline,
    pipeline_layout: vk::PipelineLayout,
    viewport: [f32; 2],
    ops: &[(u32, vk::Rect2D)],
) {
    if ops.is_empty() {
        return;
    }
    let device = &vk.device;
    unsafe {
        crate::vk_count!(cmd_bind_pipeline);
        device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, pipeline);

        for (value, rect) in ops {
            if rect.extent.width == 0 || rect.extent.height == 0 {
                continue;
            }
            // The scanout BO is B8G8R8A8_UNORM, server-owned-α (depth-24):
            // decode the xor_value exactly the way `record_logic_fill`'s
            // caller decodes a fill pixel. The pipeline's write mask drops
            // the alpha channel, so only RGB reaches the XOR logic op.
            let fg_color = crate::kms::render::engine::decode_x11_pixel_for_storage(
                *value,
                24,
                vk::Format::B8G8R8A8_UNORM,
            );

            let scissor = [*rect];
            crate::vk_count!(cmd_set_scissor);
            device.cmd_set_scissor(cb, 0, &scissor);

            let pc = LogicFillPushConsts {
                dst_origin: [rect.offset.x as f32, rect.offset.y as f32],
                dst_size: [rect.extent.width as f32, rect.extent.height as f32],
                viewport,
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
    }
}
