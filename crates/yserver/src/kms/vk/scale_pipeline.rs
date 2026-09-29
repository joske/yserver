//! RANDR CRTC transform scale pass (spec D4): one full-screen draw per
//! repaint of a transformed output, from its footprint-sized intermediate
//! into the mode-sized scanout image. Built on first use, so outputs without
//! a transform pay nothing.

use std::sync::Arc;

use ash::vk;

use super::{device::VkContext, pipeline::PipelineError};

const VERTEX_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/scale_pass.vert.spv"));
const FRAGMENT_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/scale_pass.frag.spv"));

/// Push constants matching `scale_pass.frag.glsl`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScalePushConsts {
    /// `m11`, `m22` as the 16.16 words the client sent.
    pub matrix: [u32; 2],
    /// The same diagonal as floats.
    pub scale: [f32; 2],
    /// Intermediate extent.
    pub src_size: [f32; 2],
    /// 1 = nearest, 0 = bilinear.
    pub nearest: u32,
    pub _pad: u32,
}

impl ScalePushConsts {
    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: `repr(C)`, plain 4-byte fields, no padding (size asserted).
        unsafe {
            std::slice::from_raw_parts(
                std::ptr::from_ref::<Self>(self).cast::<u8>(),
                std::mem::size_of::<Self>(),
            )
        }
    }
}

const _: () = assert!(std::mem::size_of::<ScalePushConsts>() == 32);

/// The scale pass pipeline: one combined image sampler (linear,
/// clamp-to-edge; the nearest path uses `texelFetch`), no blending.
pub struct ScalePassPipeline {
    vk: Arc<VkContext>,
    pub pipeline: vk::Pipeline,
    pub pipeline_layout: vk::PipelineLayout,
    pub descriptor_set_layout: vk::DescriptorSetLayout,
    pub sampler: vk::Sampler,
}

impl ScalePassPipeline {
    pub fn new(vk: Arc<VkContext>, color_format: vk::Format) -> Result<Self, PipelineError> {
        let device = &vk.device;
        let sampler_info = vk::SamplerCreateInfo::default()
            .mag_filter(vk::Filter::LINEAR)
            .min_filter(vk::Filter::LINEAR)
            .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .min_lod(0.0)
            .max_lod(0.0);
        let sampler = unsafe { device.create_sampler(&sampler_info, None)? };
        let bindings = [vk::DescriptorSetLayoutBinding::default()
            .binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)];
        let descriptor_set_layout = match unsafe {
            device.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                None,
            )
        } {
            Ok(layout) => layout,
            Err(e) => {
                unsafe { device.destroy_sampler(sampler, None) };
                return Err(e.into());
            }
        };
        let set_layouts = [descriptor_set_layout];
        let ranges = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)
            .offset(0)
            .size(std::mem::size_of::<ScalePushConsts>() as u32)];
        let pipeline_layout = match unsafe {
            device.create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default()
                    .set_layouts(&set_layouts)
                    .push_constant_ranges(&ranges),
                None,
            )
        } {
            Ok(layout) => layout,
            Err(e) => {
                unsafe {
                    device.destroy_descriptor_set_layout(descriptor_set_layout, None);
                    device.destroy_sampler(sampler, None);
                }
                return Err(e.into());
            }
        };
        let pipeline = match build_pipeline(device, pipeline_layout, color_format) {
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
        Ok(Self {
            vk,
            pipeline,
            pipeline_layout,
            descriptor_set_layout,
            sampler,
        })
    }
}

impl Drop for ScalePassPipeline {
    fn drop(&mut self) {
        unsafe {
            let _ = self.vk.device.queue_wait_idle(self.vk.graphics_queue);
            self.vk.device.destroy_pipeline(self.pipeline, None);
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

fn build_pipeline(
    device: &ash::Device,
    pipeline_layout: vk::PipelineLayout,
    color_format: vk::Format,
) -> Result<vk::Pipeline, PipelineError> {
    let vert = super::pipeline::create_shader_module(device, VERTEX_SPV)?;
    let frag = match super::pipeline::create_shader_module(device, FRAGMENT_SPV) {
        Ok(m) => m,
        Err(e) => {
            unsafe { device.destroy_shader_module(vert, None) };
            return Err(e);
        }
    };
    let entry = c"main";
    let stages = [
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::VERTEX)
            .module(vert)
            .name(entry),
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::FRAGMENT)
            .module(frag)
            .name(entry),
    ];
    let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
    let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
        .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
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
    // `PictOpSrc` into the scanout, as Xorg's shadow pass.
    let attachments = [vk::PipelineColorBlendAttachmentState::default()
        .blend_enable(false)
        .color_write_mask(vk::ColorComponentFlags::RGBA)];
    let color_blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(&attachments);
    let dynamic = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
    let dynamic_state = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic);
    let formats = [color_format];
    let mut rendering =
        vk::PipelineRenderingCreateInfo::default().color_attachment_formats(&formats);
    let info = vk::GraphicsPipelineCreateInfo::default()
        .stages(&stages)
        .vertex_input_state(&vertex_input)
        .input_assembly_state(&input_assembly)
        .viewport_state(&viewport_state)
        .rasterization_state(&rasterization)
        .multisample_state(&multisample)
        .color_blend_state(&color_blend)
        .dynamic_state(&dynamic_state)
        .layout(pipeline_layout)
        .push_next(&mut rendering);
    let pipeline =
        unsafe { device.create_graphics_pipelines(vk::PipelineCache::null(), &[info], None) };
    unsafe {
        device.destroy_shader_module(vert, None);
        device.destroy_shader_module(frag, None);
    }
    match pipeline {
        Ok(ps) => Ok(ps[0]),
        Err((_, e)) => Err(e.into()),
    }
}
