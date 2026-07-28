//! Graphics-pipeline + sampler + descriptor-set-layout for the
//! per-window composite pass (sub-phase 4.1.3.4).
//!
//! Spec: docs/superpowers/specs/2026-05-07-phase4-1-vulkan-compositor-design.md
//! "Frame composite pass" — one quad draw per visible window
//! sampling that window's `vk_mirror`, plus a final cursor quad.
//!
//! This module owns the *pipeline* — built once per backend
//! lifetime, reused every frame. Per-frame descriptor pools live in
//! the per-output `CompositePoolRing` (one ring per output); command-
//! buffer recording lives in the call site (4.1.3.4 wires that next).
//!
//! Pipeline shape:
//!
//! - Two pipelines (force-opaque vs alpha-pass-through, picked at
//!   build time via the frag shader's `SRC_ALPHA_MODE` spec-constant
//!   — see L1 plan task A.2). Both share one descriptor set layout
//!   (single combined image sampler at binding 0), one nearest-filter
//!   sampler, and one pipeline layout.
//! - Vertex stage: 4-vertex `vkCmdDraw(4, 1, ...)` driven by
//!   `gl_VertexIndex` — no vertex buffer is bound.
//! - Fragment stage: samples the bound texture; the spec-constant
//!   decides whether to keep alpha (cursor / alpha pixmaps) or force
//!   `1.0` (force-opaque). The caller picks the variant per draw via
//!   [`CompositorPipeline::pipeline_for`].
//! - Color blend state is src-over either way; the alpha override
//!   in the fragment shader is what differentiates opaque from
//!   alpha draws.
//! - Dynamic state: `viewport` + `scissor` (the scanout extent
//!   isn't known until a bo is selected per frame).
//! - Render-pass-equivalent: `VK_KHR_dynamic_rendering`
//!   (`VkPipelineRenderingCreateInfo` chained on the pipeline create
//!   info). The scanout bo's color format
//!   (`B8G8R8A8_UNORM`) is baked in at pipeline construction; if a
//!   future bo uses a different format we'd need a per-format
//!   pipeline cache (4.1.4 territory).

use std::sync::Arc;

use ash::vk;

use super::device::VkContext;

const VERTEX_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/composite.vert.spv"));
const FRAGMENT_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/composite.frag.spv"));

/// Push-constant block matching `composite.vert.glsl`'s `PushConsts`
/// layout. Total 40 bytes (well under the 128-byte minimum
/// `maxPushConstantsSize`). The old `use_src_alpha`/`_pad` fields
/// moved to a fragment specialization constant in A.2; the pipeline
/// variant — opaque vs pass-through — is now a build-time choice
/// (see [`CompositorPipeline::pipeline_for`]).
///
/// `repr(C)` keeps the field order stable; std140-style alignment
/// rules govern the GLSL side (vec2 alignment = 8). We use vec2
/// pairs so each push-constant member is naturally aligned without
/// trailing pad words.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CompositePushConsts {
    pub dst_origin: [f32; 2],
    pub dst_size: [f32; 2],
    pub viewport: [f32; 2],
    pub src_origin: [f32; 2],
    pub src_size: [f32; 2],
}

impl CompositePushConsts {
    /// Reinterpret as a byte slice for `vkCmdPushConstants`.
    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: `CompositePushConsts` is `#[repr(C)]` and contains
        // only plain f32 fields with no padding (verified by the
        // size assertion below). Reading any byte pattern as f32 is
        // safe (no invalid bit patterns); reading the struct as
        // bytes is the inverse direction.
        unsafe {
            std::slice::from_raw_parts(
                std::ptr::from_ref::<Self>(self).cast::<u8>(),
                std::mem::size_of::<Self>(),
            )
        }
    }
}

const _: () = assert!(std::mem::size_of::<CompositePushConsts>() == 40);

/// Built once per backend lifetime; reused for every composite pass.
///
/// Two graphics pipelines compile at construction — one per value
/// of the fragment shader's `SRC_ALPHA_MODE` specialization
/// constant — and the caller picks per draw via
/// [`Self::pipeline_for`]. Both pipelines share the same
/// descriptor-set layout, pipeline layout, and sampler.
/// Per-frame descriptor pools are managed by the per-output
/// `CompositePoolRing`.
pub struct CompositorPipeline {
    vk: Arc<VkContext>,
    /// Force-opaque variant (`SRC_ALPHA_MODE = 0`). Window-mirror
    /// draws bind this until L1 task A.16 flips the dial to
    /// alpha-pass-through.
    pub pipeline_opaque: vk::Pipeline,
    /// Alpha-pass-through variant (`SRC_ALPHA_MODE = 1`). Cursor +
    /// alpha-pixmap draws bind this today; window-mirror draws switch
    /// to it post-A.16.
    pub pipeline_passthrough: vk::Pipeline,
    pub pipeline_layout: vk::PipelineLayout,
    pub descriptor_set_layout: vk::DescriptorSetLayout,
    pub sampler: vk::Sampler,
    /// Color attachment format the pipeline was built for. The
    /// scanout bos use `B8G8R8A8_UNORM`; if anything else shows up
    /// the caller has to build a fresh pipeline.
    pub color_format: vk::Format,
}

/// Initial per-slot descriptor capacity. Complex SHAPE regions are unbounded;
/// `CompositePoolRing` grows a free slot to the complete frame's demand.
pub const INITIAL_DESCRIPTOR_SETS_PER_FRAME: u32 = 1024;

#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    #[error("vulkan: {0:?}")]
    Vk(vk::Result),
    #[error("shader SPIR-V from build.rs is malformed (length not multiple of 4): {0} bytes")]
    SpirvUnaligned(usize),
}

impl From<vk::Result> for PipelineError {
    fn from(r: vk::Result) -> Self {
        PipelineError::Vk(r)
    }
}

impl CompositorPipeline {
    pub fn new(vk: Arc<VkContext>, color_format: vk::Format) -> Result<Self, PipelineError> {
        let device = &vk.device;

        // Sampler — nearest filter for pixel-perfect window blit
        // (mirrors are at the same scale as their dst quad). No
        // mipmaps; clamp to edge so a fragment outside the source
        // UV box reads the border pixel rather than wrapping.
        let sampler_info = vk::SamplerCreateInfo::default()
            .mag_filter(vk::Filter::NEAREST)
            .min_filter(vk::Filter::NEAREST)
            .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .min_lod(0.0)
            .max_lod(0.0);
        let sampler = unsafe { device.create_sampler(&sampler_info, None)? };

        // Descriptor set layout: single combined image sampler at
        // binding 0, fragment-stage only.
        let dsl_bindings = [vk::DescriptorSetLayoutBinding::default()
            .binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)];
        let dsl_info = vk::DescriptorSetLayoutCreateInfo::default().bindings(&dsl_bindings);
        let descriptor_set_layout =
            match unsafe { device.create_descriptor_set_layout(&dsl_info, None) } {
                Ok(d) => d,
                Err(e) => {
                    unsafe { device.destroy_sampler(sampler, None) };
                    return Err(e.into());
                }
            };

        // Pipeline layout: 1 descriptor set + 1 push constant range
        // (vertex+fragment, 40 bytes post-A.2).
        let set_layouts = [descriptor_set_layout];
        let push_const_ranges = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
            .offset(0)
            .size(std::mem::size_of::<CompositePushConsts>() as u32)];
        let pl_info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&set_layouts)
            .push_constant_ranges(&push_const_ranges);
        let pipeline_layout = match unsafe { device.create_pipeline_layout(&pl_info, None) } {
            Ok(p) => p,
            Err(e) => {
                unsafe {
                    device.destroy_descriptor_set_layout(descriptor_set_layout, None);
                    device.destroy_sampler(sampler, None);
                }
                return Err(e.into());
            }
        };

        // Build one pipeline per alpha-mode (SRC_ALPHA_MODE = 0
        // force-opaque, 1 pass-through). On any failure, tear down
        // everything created so far.
        let pipeline_opaque = match build_pipeline_variant(device, pipeline_layout, color_format, 0)
        {
            Ok(p) => p,
            Err(e) => {
                unsafe {
                    device.destroy_pipeline_layout(pipeline_layout, None);
                    device.destroy_descriptor_set_layout(descriptor_set_layout, None);
                    device.destroy_sampler(sampler, None);
                }
                return Err(e);
            }
        };
        let pipeline_passthrough =
            match build_pipeline_variant(device, pipeline_layout, color_format, 1) {
                Ok(p) => p,
                Err(e) => {
                    unsafe {
                        device.destroy_pipeline(pipeline_opaque, None);
                        device.destroy_pipeline_layout(pipeline_layout, None);
                        device.destroy_descriptor_set_layout(descriptor_set_layout, None);
                        device.destroy_sampler(sampler, None);
                    }
                    return Err(e);
                }
            };

        Ok(Self {
            vk,
            pipeline_opaque,
            pipeline_passthrough,
            pipeline_layout,
            descriptor_set_layout,
            sampler,
            color_format,
        })
    }

    /// Pick the pipeline variant for a draw. `alpha_passthrough = false`
    /// → force-opaque (`SRC_ALPHA_MODE = 0`); `true` → pass-through.
    #[must_use]
    pub fn pipeline_for(&self, alpha_passthrough: bool) -> vk::Pipeline {
        if alpha_passthrough {
            self.pipeline_passthrough
        } else {
            self.pipeline_opaque
        }
    }
}

impl Drop for CompositorPipeline {
    fn drop(&mut self) {
        unsafe {
            let _ = self.vk.device.queue_wait_idle(self.vk.graphics_queue);
            self.vk
                .device
                .destroy_pipeline(self.pipeline_passthrough, None);
            self.vk.device.destroy_pipeline(self.pipeline_opaque, None);
            self.vk
                .device
                .destroy_pipeline_layout(self.pipeline_layout, None);
            self.vk
                .device
                .destroy_descriptor_set_layout(self.descriptor_set_layout, None);
            self.vk.device.destroy_sampler(self.sampler, None);
        }
    }
}

/// Compile one fragment-stage variant of the composite pipeline.
/// `src_alpha_mode` is the value of the frag shader's spec-constant
/// 0 (`SRC_ALPHA_MODE` — 0 = force-opaque, 1 = pass-through). The
/// vertex stage and all fixed-function state are identical across
/// variants.
fn build_pipeline_variant(
    device: &ash::Device,
    pipeline_layout: vk::PipelineLayout,
    color_format: vk::Format,
    src_alpha_mode: i32,
) -> Result<vk::Pipeline, PipelineError> {
    // Shader modules — destroyed after the pipeline owns the
    // compiled bytecode.
    let vert_module = create_shader_module(device, VERTEX_SPV)?;
    let frag_module = match create_shader_module(device, FRAGMENT_SPV) {
        Ok(m) => m,
        Err(e) => {
            unsafe { device.destroy_shader_module(vert_module, None) };
            return Err(e);
        }
    };

    // Specialization constant 0 → `SRC_ALPHA_MODE`. `i32` matches
    // the GLSL `const int` declaration.
    let spec_map = [vk::SpecializationMapEntry::default()
        .constant_id(0)
        .offset(0)
        .size(std::mem::size_of::<i32>())];
    let spec_data = src_alpha_mode.to_ne_bytes();
    let spec_info = vk::SpecializationInfo::default()
        .map_entries(&spec_map)
        .data(&spec_data);

    let entry = c"main";
    let stages = [
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::VERTEX)
            .module(vert_module)
            .name(entry),
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::FRAGMENT)
            .module(frag_module)
            .name(entry)
            .specialization_info(&spec_info),
    ];

    let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
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

    // Src-over alpha blending (premultiplied-alpha-ready):
    //   color_out = src.rgb + dst.rgb * (1 - src.a)
    //   alpha_out = src.a + dst.a * (1 - src.a)
    // The force-opaque variant forces src.a = 1 in the fragment
    // shader, so its output equals src.rgb regardless of dst (no
    // peek-through). The pass-through variant honours the sampled
    // alpha.
    let color_blend_attachments = [vk::PipelineColorBlendAttachmentState::default()
        .blend_enable(true)
        .src_color_blend_factor(vk::BlendFactor::ONE)
        .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
        .color_blend_op(vk::BlendOp::ADD)
        .src_alpha_blend_factor(vk::BlendFactor::ONE)
        .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
        .alpha_blend_op(vk::BlendOp::ADD)
        .color_write_mask(vk::ColorComponentFlags::RGBA)];
    let color_blend =
        vk::PipelineColorBlendStateCreateInfo::default().attachments(&color_blend_attachments);

    let dynamic_state_array = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
    let dynamic_state =
        vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_state_array);

    let color_formats = [color_format];
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

    let pipeline = unsafe {
        device.create_graphics_pipelines(vk::PipelineCache::null(), &[pipeline_info], None)
    };
    unsafe {
        device.destroy_shader_module(vert_module, None);
        device.destroy_shader_module(frag_module, None);
    }
    match pipeline {
        Ok(ps) => Ok(ps[0]),
        Err((_, e)) => Err(e.into()),
    }
}

fn create_shader_module(
    device: &ash::Device,
    spv_bytes: &[u8],
) -> Result<vk::ShaderModule, PipelineError> {
    if !spv_bytes.len().is_multiple_of(4) {
        return Err(PipelineError::SpirvUnaligned(spv_bytes.len()));
    }
    // SPIR-V is u32-aligned; build.rs's glslc output is little-endian
    // on little-endian hosts. Native byte order matches what the
    // driver expects on the same host. Copy bytes to an aligned
    // Vec<u32> rather than risk an unaligned cast.
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
    #[ignore = "needs live Vulkan ICD"]
    fn composite_pipeline_has_two_alpha_mode_variants() {
        let vk = VkContext::new().expect("vk init");
        let cp =
            CompositorPipeline::new(vk, vk::Format::B8G8R8A8_UNORM).expect("compositor pipeline");
        // Distinct pipeline objects for force-opaque vs pass-through.
        assert_ne!(cp.pipeline_for(false), cp.pipeline_for(true));
    }
}
