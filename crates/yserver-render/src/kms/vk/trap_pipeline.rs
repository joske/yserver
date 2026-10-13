//! GPU rasterization pipelines for RENDER `Trapezoids` and
//! `Triangles` (gpu-trap T1 / T3).
//!
//! Replaces the retired CPU 4×4 supersampled rasterizer with a single
//! Vulkan draw per RENDER request that writes coverage directly into
//! the `R8_UNORM` MaskScratch image. The fragment shader computes
//! analytic edge coverage per pixel; saturated additive blending
//! (ONE + ONE ADD, clamped by the R8_UNORM format to `[0, 1]`) gives
//! the same union-with-saturating-add semantics as the CPU path's
//! per-pixel `saturating_add`.
//!
//! Cadence: one draw call per request, `vkCmdDraw(4, n_primitives)`.
//! All primitives in the same RENDER request collapse into a single
//! instanced draw; per-instance vertex attributes encode the trap /
//! triangle geometry. The bbox is a per-draw constant carried in
//! push constants.
//!
//! Both pipelines share a pipeline layout (push consts identical);
//! they differ in vertex input + shaders. `trapezoid_pipeline()`
//! consumes [`TrapInstanceData`] (40 bytes per instance);
//! `triangle_pipeline()` consumes [`TriangleInstanceData`] (24 bytes
//! per instance).

use std::sync::Arc;

use ash::vk;

use super::device::VkContext;

const TRAP_VERT_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/trap.vert.spv"));
const TRAP_FRAG_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/trap.frag.spv"));
const TRIANGLE_VERT_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/triangle.vert.spv"));
const TRIANGLE_FRAG_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/triangle.frag.spv"));

/// Per-instance trap data — exactly the per-trap geometry the
/// fragment shader needs for analytic edge coverage. The bbox is in
/// `TrapDrawPushConsts`, not here, because all instances in a draw
/// share the same bbox.
///
/// Layout: 10 `f32` = 40 bytes. Vertex input attributes (binding 0,
/// `VK_VERTEX_INPUT_RATE_INSTANCE`) decompose into 6 slots:
/// `top:f32`, `bottom:f32`, `left_p1:vec2`, `left_p2:vec2`,
/// `right_p1:vec2`, `right_p2:vec2`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct TrapInstanceData {
    pub top: f32,
    pub bottom: f32,
    pub left_p1: [f32; 2],
    pub left_p2: [f32; 2],
    pub right_p1: [f32; 2],
    pub right_p2: [f32; 2],
}

const _: () = assert!(std::mem::size_of::<TrapInstanceData>() == 40);

impl TrapInstanceData {
    /// View this struct as a byte slice for direct memcpy into the
    /// host-visible upload arena.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: `repr(C)` with f32 fields, no padding (asserted
        // above); the resulting slice never outlives `self`.
        unsafe {
            std::slice::from_raw_parts(
                std::ptr::from_ref::<Self>(self).cast::<u8>(),
                std::mem::size_of::<Self>(),
            )
        }
    }
}

/// Per-instance triangle data — three corners as `vec2`s. The bbox
/// is in `TrapDrawPushConsts` (shared with the trapezoid pipeline),
/// not here.
///
/// Layout: 3 × `vec2` = 6 `f32` = 24 bytes. Vertex input attributes
/// (binding 0, `VK_VERTEX_INPUT_RATE_INSTANCE`) decompose into 3
/// slots: `p1:vec2`, `p2:vec2`, `p3:vec2`. The triangle pipeline
/// (`triangle_pipeline`) shares the same pipeline layout / push
/// consts as the trapezoid pipeline; only the vertex input layout
/// and shaders differ.
///
/// RENDER's `Triangles` request does NOT specify a winding
/// convention — points may arrive CW or CCW. Winding-order handling
/// is in the triangle vertex shader (computes signed-area sign once
/// per instance, passes as `orient` flat float) so the fragment can
/// pick a consistent inside-side regardless. Mirrors the
/// sign-agnostic CPU reference `point_in_triangle` in
/// `vk/ops/traps.rs::point_in_triangle`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct TriangleInstanceData {
    pub p1: [f32; 2],
    pub p2: [f32; 2],
    pub p3: [f32; 2],
}

const _: () = assert!(std::mem::size_of::<TriangleInstanceData>() == 24);

impl TriangleInstanceData {
    /// View this struct as a byte slice for direct memcpy into the
    /// host-visible upload arena.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: `repr(C)` with f32 fields, no padding (asserted
        // above); the resulting slice never outlives `self`.
        unsafe {
            std::slice::from_raw_parts(
                std::ptr::from_ref::<Self>(self).cast::<u8>(),
                std::mem::size_of::<Self>(),
            )
        }
    }
}

/// Per-draw push constants. Constant across every instance in a
/// single `vkCmdDraw`. The vertex shader uses
/// `bbox_origin_pixel` + `bbox_size_pixel` to position its unit quad
/// in mask pixel space; `mask_extent` scales pixel coords to NDC.
/// `_pad` pads to a 16-byte boundary for consistent `std430` layout.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct TrapDrawPushConsts {
    pub mask_extent: [f32; 2],       // offset 0
    pub bbox_origin_pixel: [f32; 2], // offset 8
    pub bbox_size_pixel: [f32; 2],   // offset 16
    pub _pad: [f32; 2],              // offset 24
}

const _: () = assert!(std::mem::size_of::<TrapDrawPushConsts>() == 32);

impl TrapDrawPushConsts {
    /// View this struct as a byte slice for `vkCmdPushConstants`.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: `repr(C)` with f32 fields, no padding (asserted
        // above); the resulting slice never outlives `self`.
        unsafe {
            std::slice::from_raw_parts(
                std::ptr::from_ref::<Self>(self).cast::<u8>(),
                std::mem::size_of::<Self>(),
            )
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TrapPipelineError {
    #[error("vulkan: {0:?}")]
    Vk(vk::Result),
    #[error("trap shader SPIR-V from build.rs is malformed (length not multiple of 4): {0} bytes")]
    SpirvUnaligned(usize),
}

impl From<vk::Result> for TrapPipelineError {
    fn from(r: vk::Result) -> Self {
        TrapPipelineError::Vk(r)
    }
}

/// GPU rasterization pipeline for RENDER Trapezoids + Triangles.
///
/// Holds the shared pipeline layout (push constants only — no
/// descriptor sets, per-instance data is via vertex attributes) and
/// two sibling pipelines: `trapezoid_pipeline` (T2) writing 4-edge
/// coverage masks and `triangle_pipeline` (T3) writing 3-edge masks.
/// Both share the same push-const layout (`TrapDrawPushConsts`)
/// but differ in vertex input layout and shader pair.
pub struct TrapPipeline {
    vk: Arc<VkContext>,
    pipeline_layout: vk::PipelineLayout,
    trapezoid_pipeline: vk::Pipeline,
    triangle_pipeline: vk::Pipeline,
}

impl std::fmt::Debug for TrapPipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrapPipeline")
            .field("pipeline_layout", &self.pipeline_layout)
            .field("trapezoid_pipeline", &self.trapezoid_pipeline)
            .field("triangle_pipeline", &self.triangle_pipeline)
            .finish_non_exhaustive()
    }
}

impl TrapPipeline {
    pub fn new(vk: Arc<VkContext>, mask_format: vk::Format) -> Result<Self, TrapPipelineError> {
        let device = &vk.device;
        // gpu-trap T2: push consts are read by both stages — the
        // vertex shader for quad positioning + viewport scaling, the
        // fragment shader for adding `bbox_origin_pixel` to
        // `gl_FragCoord` (the quad emits in MaskScratch-local coords
        // so the GPU writes to (0, 0)..(bbox_w, bbox_h), but the
        // trap edge attributes are in absolute pixel coords from the
        // protocol). Widening the visibility mask costs nothing.
        let push_const_ranges = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
            .offset(0)
            .size(std::mem::size_of::<TrapDrawPushConsts>() as u32)];
        let pl_info =
            vk::PipelineLayoutCreateInfo::default().push_constant_ranges(&push_const_ranges);
        let pipeline_layout = unsafe { device.create_pipeline_layout(&pl_info, None)? };

        let trapezoid_pipeline = match build_trap_pipeline(&vk, pipeline_layout, mask_format) {
            Ok(p) => p,
            Err(e) => {
                unsafe { device.destroy_pipeline_layout(pipeline_layout, None) };
                return Err(e);
            }
        };
        let triangle_pipeline = match build_triangle_pipeline(&vk, pipeline_layout, mask_format) {
            Ok(p) => p,
            Err(e) => {
                unsafe {
                    device.destroy_pipeline(trapezoid_pipeline, None);
                    device.destroy_pipeline_layout(pipeline_layout, None);
                }
                return Err(e);
            }
        };

        Ok(Self {
            vk,
            pipeline_layout,
            trapezoid_pipeline,
            triangle_pipeline,
        })
    }

    pub fn pipeline_layout(&self) -> vk::PipelineLayout {
        self.pipeline_layout
    }

    pub fn trapezoid_pipeline(&self) -> vk::Pipeline {
        self.trapezoid_pipeline
    }

    pub fn triangle_pipeline(&self) -> vk::Pipeline {
        self.triangle_pipeline
    }
}

impl Drop for TrapPipeline {
    fn drop(&mut self) {
        unsafe {
            let _ = self.vk.device.queue_wait_idle(self.vk.graphics_queue);
            self.vk
                .device
                .destroy_pipeline(self.triangle_pipeline, None);
            self.vk
                .device
                .destroy_pipeline(self.trapezoid_pipeline, None);
            self.vk
                .device
                .destroy_pipeline_layout(self.pipeline_layout, None);
        }
    }
}

fn build_trap_pipeline(
    vk: &VkContext,
    pipeline_layout: vk::PipelineLayout,
    mask_format: vk::Format,
) -> Result<vk::Pipeline, TrapPipelineError> {
    let device = &vk.device;
    let vert_module = create_shader_module(device, TRAP_VERT_SPV)?;
    let frag_module = match create_shader_module(device, TRAP_FRAG_SPV) {
        Ok(m) => m,
        Err(e) => {
            unsafe { device.destroy_shader_module(vert_module, None) };
            return Err(e);
        }
    };

    let entry = c"main";
    let stages = [
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::VERTEX)
            .module(vert_module)
            .name(entry),
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::FRAGMENT)
            .module(frag_module)
            .name(entry),
    ];

    // Per-instance vertex input. One binding (stride = 40), six
    // attributes splitting `TrapInstanceData` into its constituent
    // f32 / vec2 slots. INSTANCE rate so the same 4 quad verts
    // (TRIANGLE_STRIP) re-use one attribute set per instance.
    let bindings = [vk::VertexInputBindingDescription::default()
        .binding(0)
        .stride(std::mem::size_of::<TrapInstanceData>() as u32)
        .input_rate(vk::VertexInputRate::INSTANCE)];
    let attributes = [
        // location 0: top : f32
        vk::VertexInputAttributeDescription::default()
            .location(0)
            .binding(0)
            .format(vk::Format::R32_SFLOAT)
            .offset(0),
        // location 1: bottom : f32
        vk::VertexInputAttributeDescription::default()
            .location(1)
            .binding(0)
            .format(vk::Format::R32_SFLOAT)
            .offset(4),
        // location 2: left_p1 : vec2
        vk::VertexInputAttributeDescription::default()
            .location(2)
            .binding(0)
            .format(vk::Format::R32G32_SFLOAT)
            .offset(8),
        // location 3: left_p2 : vec2
        vk::VertexInputAttributeDescription::default()
            .location(3)
            .binding(0)
            .format(vk::Format::R32G32_SFLOAT)
            .offset(16),
        // location 4: right_p1 : vec2
        vk::VertexInputAttributeDescription::default()
            .location(4)
            .binding(0)
            .format(vk::Format::R32G32_SFLOAT)
            .offset(24),
        // location 5: right_p2 : vec2
        vk::VertexInputAttributeDescription::default()
            .location(5)
            .binding(0)
            .format(vk::Format::R32G32_SFLOAT)
            .offset(32),
    ];
    let vertex_input = vk::PipelineVertexInputStateCreateInfo::default()
        .vertex_binding_descriptions(&bindings)
        .vertex_attribute_descriptions(&attributes);

    let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
        .topology(vk::PrimitiveTopology::TRIANGLE_STRIP);
    let viewport_state = vk::PipelineViewportStateCreateInfo::default()
        .viewport_count(1)
        .scissor_count(1);
    let rasterization = vk::PipelineRasterizationStateCreateInfo::default()
        .polygon_mode(vk::PolygonMode::FILL)
        .cull_mode(vk::CullModeFlags::NONE)
        .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
        .line_width(1.0);
    let multisample = vk::PipelineMultisampleStateCreateInfo::default()
        .rasterization_samples(vk::SampleCountFlags::TYPE_1);

    // Saturated additive blend on R only. R8_UNORM clamps to
    // [0, 1] in the framebuffer, which gives the same union-with-
    // saturating-add semantics as the CPU path. Alpha factors are
    // set even though R8 has no alpha channel — some validators
    // complain about leaving them at defaults when blend is on.
    let color_blend_attachments = [vk::PipelineColorBlendAttachmentState::default()
        .blend_enable(true)
        .src_color_blend_factor(vk::BlendFactor::ONE)
        .dst_color_blend_factor(vk::BlendFactor::ONE)
        .color_blend_op(vk::BlendOp::ADD)
        .src_alpha_blend_factor(vk::BlendFactor::ONE)
        .dst_alpha_blend_factor(vk::BlendFactor::ONE)
        .alpha_blend_op(vk::BlendOp::ADD)
        .color_write_mask(vk::ColorComponentFlags::R)];
    let color_blend = vk::PipelineColorBlendStateCreateInfo::default()
        .logic_op_enable(false)
        .attachments(&color_blend_attachments);

    let dynamic_state_array = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
    let dynamic_state =
        vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_state_array);
    let color_formats = [mask_format];
    let mut rendering_info =
        vk::PipelineRenderingCreateInfo::default().color_attachment_formats(&color_formats);

    let pipeline_info = vk::GraphicsPipelineCreateInfo::default()
        .stages(&stages)
        .vertex_input_state(&vertex_input)
        .input_assembly_state(&input_assembly)
        .viewport_state(&viewport_state)
        .rasterization_state(&rasterization)
        .multisample_state(&multisample)
        .color_blend_state(&color_blend)
        .dynamic_state(&dynamic_state)
        .layout(pipeline_layout)
        .push_next(&mut rendering_info);

    let pipeline = match unsafe {
        device.create_graphics_pipelines(vk::PipelineCache::null(), &[pipeline_info], None)
    } {
        Ok(ps) => ps[0],
        Err((_, e)) => {
            unsafe {
                device.destroy_shader_module(vert_module, None);
                device.destroy_shader_module(frag_module, None);
            }
            return Err(e.into());
        }
    };
    unsafe {
        device.destroy_shader_module(vert_module, None);
        device.destroy_shader_module(frag_module, None);
    }
    Ok(pipeline)
}

fn build_triangle_pipeline(
    vk: &VkContext,
    pipeline_layout: vk::PipelineLayout,
    mask_format: vk::Format,
) -> Result<vk::Pipeline, TrapPipelineError> {
    let device = &vk.device;
    let vert_module = create_shader_module(device, TRIANGLE_VERT_SPV)?;
    let frag_module = match create_shader_module(device, TRIANGLE_FRAG_SPV) {
        Ok(m) => m,
        Err(e) => {
            unsafe { device.destroy_shader_module(vert_module, None) };
            return Err(e);
        }
    };

    let entry = c"main";
    let stages = [
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::VERTEX)
            .module(vert_module)
            .name(entry),
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::FRAGMENT)
            .module(frag_module)
            .name(entry),
    ];

    // Per-instance vertex input. One binding (stride = 24), three
    // attributes — three `vec2`s for `p1`, `p2`, `p3`. INSTANCE rate
    // so the same 4 quad verts (TRIANGLE_STRIP) re-use one attribute
    // set per instance.
    let bindings = [vk::VertexInputBindingDescription::default()
        .binding(0)
        .stride(std::mem::size_of::<TriangleInstanceData>() as u32)
        .input_rate(vk::VertexInputRate::INSTANCE)];
    let attributes = [
        // location 0: p1 : vec2
        vk::VertexInputAttributeDescription::default()
            .location(0)
            .binding(0)
            .format(vk::Format::R32G32_SFLOAT)
            .offset(0),
        // location 1: p2 : vec2
        vk::VertexInputAttributeDescription::default()
            .location(1)
            .binding(0)
            .format(vk::Format::R32G32_SFLOAT)
            .offset(8),
        // location 2: p3 : vec2
        vk::VertexInputAttributeDescription::default()
            .location(2)
            .binding(0)
            .format(vk::Format::R32G32_SFLOAT)
            .offset(16),
    ];
    let vertex_input = vk::PipelineVertexInputStateCreateInfo::default()
        .vertex_binding_descriptions(&bindings)
        .vertex_attribute_descriptions(&attributes);

    let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
        .topology(vk::PrimitiveTopology::TRIANGLE_STRIP);
    let viewport_state = vk::PipelineViewportStateCreateInfo::default()
        .viewport_count(1)
        .scissor_count(1);
    let rasterization = vk::PipelineRasterizationStateCreateInfo::default()
        .polygon_mode(vk::PolygonMode::FILL)
        .cull_mode(vk::CullModeFlags::NONE)
        .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
        .line_width(1.0);
    let multisample = vk::PipelineMultisampleStateCreateInfo::default()
        .rasterization_samples(vk::SampleCountFlags::TYPE_1);

    // Saturated additive blend on R only — same scheme as the
    // trapezoid pipeline. R8_UNORM clamps to [0, 1] in the
    // framebuffer, which gives the same union-with-saturating-add
    // semantics as the retired CPU path's per-pixel `saturating_add`.
    let color_blend_attachments = [vk::PipelineColorBlendAttachmentState::default()
        .blend_enable(true)
        .src_color_blend_factor(vk::BlendFactor::ONE)
        .dst_color_blend_factor(vk::BlendFactor::ONE)
        .color_blend_op(vk::BlendOp::ADD)
        .src_alpha_blend_factor(vk::BlendFactor::ONE)
        .dst_alpha_blend_factor(vk::BlendFactor::ONE)
        .alpha_blend_op(vk::BlendOp::ADD)
        .color_write_mask(vk::ColorComponentFlags::R)];
    let color_blend = vk::PipelineColorBlendStateCreateInfo::default()
        .logic_op_enable(false)
        .attachments(&color_blend_attachments);

    let dynamic_state_array = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
    let dynamic_state =
        vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_state_array);
    let color_formats = [mask_format];
    let mut rendering_info =
        vk::PipelineRenderingCreateInfo::default().color_attachment_formats(&color_formats);

    let pipeline_info = vk::GraphicsPipelineCreateInfo::default()
        .stages(&stages)
        .vertex_input_state(&vertex_input)
        .input_assembly_state(&input_assembly)
        .viewport_state(&viewport_state)
        .rasterization_state(&rasterization)
        .multisample_state(&multisample)
        .color_blend_state(&color_blend)
        .dynamic_state(&dynamic_state)
        .layout(pipeline_layout)
        .push_next(&mut rendering_info);

    let pipeline = match unsafe {
        device.create_graphics_pipelines(vk::PipelineCache::null(), &[pipeline_info], None)
    } {
        Ok(ps) => ps[0],
        Err((_, e)) => {
            unsafe {
                device.destroy_shader_module(vert_module, None);
                device.destroy_shader_module(frag_module, None);
            }
            return Err(e.into());
        }
    };
    unsafe {
        device.destroy_shader_module(vert_module, None);
        device.destroy_shader_module(frag_module, None);
    }
    Ok(pipeline)
}

fn create_shader_module(
    device: &ash::Device,
    spv_bytes: &[u8],
) -> Result<vk::ShaderModule, TrapPipelineError> {
    if !spv_bytes.len().is_multiple_of(4) {
        return Err(TrapPipelineError::SpirvUnaligned(spv_bytes.len()));
    }
    let mut code: Vec<u32> = Vec::with_capacity(spv_bytes.len() / 4);
    for chunk in spv_bytes.chunks_exact(4) {
        code.push(u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    let info = vk::ShaderModuleCreateInfo::default().code(&code);
    Ok(unsafe { device.create_shader_module(&info, None)? })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_data_size_is_40() {
        assert_eq!(std::mem::size_of::<TrapInstanceData>(), 40);
    }

    #[test]
    fn triangle_instance_data_size_is_24() {
        assert_eq!(std::mem::size_of::<TriangleInstanceData>(), 24);
    }

    #[test]
    fn push_consts_size_is_32() {
        assert_eq!(std::mem::size_of::<TrapDrawPushConsts>(), 32);
    }

    #[test]
    fn trap_pipeline_instance_data_as_bytes_roundtrip() {
        let data = TrapInstanceData {
            top: 1.0,
            bottom: 5.0,
            left_p1: [1.0, 1.0],
            left_p2: [1.0, 5.0],
            right_p1: [5.0, 1.0],
            right_p2: [5.0, 5.0],
        };
        let bytes = data.as_bytes();
        assert_eq!(bytes.len(), 40);
        // First f32 (top) is 1.0 → IEEE 754 little-endian 0x3F800000.
        assert_eq!(&bytes[0..4], &1.0_f32.to_ne_bytes());
        // Last vec2 component (right_p2.y) is 5.0.
        assert_eq!(&bytes[36..40], &5.0_f32.to_ne_bytes());
    }

    #[test]
    fn triangle_instance_data_as_bytes_roundtrip() {
        let data = TriangleInstanceData {
            p1: [1.0, 2.0],
            p2: [3.0, 4.0],
            p3: [5.0, 6.0],
        };
        let bytes = data.as_bytes();
        assert_eq!(bytes.len(), 24);
        // First f32 (p1.x) is 1.0.
        assert_eq!(&bytes[0..4], &1.0_f32.to_ne_bytes());
        // Last f32 (p3.y) is 6.0.
        assert_eq!(&bytes[20..24], &6.0_f32.to_ne_bytes());
    }

    #[test]
    fn trap_pipeline_push_consts_as_bytes_roundtrip() {
        let pc = TrapDrawPushConsts {
            mask_extent: [1920.0, 1080.0],
            bbox_origin_pixel: [10.0, 20.0],
            bbox_size_pixel: [200.0, 100.0],
            _pad: [0.0, 0.0],
        };
        let bytes = pc.as_bytes();
        assert_eq!(bytes.len(), 32);
        assert_eq!(&bytes[0..4], &1920.0_f32.to_ne_bytes());
        assert_eq!(&bytes[20..24], &100.0_f32.to_ne_bytes());
    }
}
