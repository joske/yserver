//! Pipeline for the `RecordedFillRect` instanced solid-copy path.
//!
//! The logic-fill pipeline has a different fixed-function contract: it enables
//! logic operations and uses channel-specific write masks. Solid fills need
//! neither feature, so they share only the fragment shader and push-constant
//! ABI while keeping this replacement state explicit.

use std::sync::Arc;

use ash::vk;

use super::{device::VkContext, logic_fill_pipeline::LogicFillPushConsts};

const VERTEX_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/solid_fill.vert.spv"));
const FRAGMENT_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/logic_fill.frag.spv"));

/// Instance-rate vertex record: destination origin and size in pixels.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct SolidFillInstance {
    pub(crate) origin: [f32; 2],
    pub(crate) size: [f32; 2],
}

const _: () = assert!(std::mem::size_of::<SolidFillInstance>() == 16);

impl SolidFillInstance {
    /// The record contains four native-endian f32 values and has no padding.
    pub(crate) fn as_bytes(&self) -> &[u8] {
        // SAFETY: `repr(C)` and the size assertion above establish that the
        // four f32 fields occupy exactly these 16 bytes.
        unsafe {
            std::slice::from_raw_parts(
                std::ptr::from_ref::<Self>(self).cast::<u8>(),
                std::mem::size_of::<Self>(),
            )
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SolidFillError {
    #[error("vulkan: {0:?}")]
    Vk(vk::Result),
    #[error("solid-fill shader SPIR-V is malformed (length not multiple of 4): {0} bytes")]
    SpirvUnaligned(usize),
}

impl From<vk::Result> for SolidFillError {
    fn from(result: vk::Result) -> Self {
        Self::Vk(result)
    }
}

/// One pipeline and its layout for a single color attachment format.
pub struct SolidFillPipelineCache {
    vk: Arc<VkContext>,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
}

impl SolidFillPipelineCache {
    pub fn new(vk: Arc<VkContext>, color_format: vk::Format) -> Result<Self, SolidFillError> {
        let device = &vk.device;
        let push_const_ranges = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
            .offset(0)
            .size(std::mem::size_of::<LogicFillPushConsts>() as u32)];
        let pipeline_layout = unsafe {
            device.create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default().push_constant_ranges(&push_const_ranges),
                None,
            )?
        };

        let vertex_module = match create_shader_module(device, VERTEX_SPV) {
            Ok(module) => module,
            Err(error) => {
                unsafe { device.destroy_pipeline_layout(pipeline_layout, None) };
                return Err(error);
            }
        };
        let fragment_module = match create_shader_module(device, FRAGMENT_SPV) {
            Ok(module) => module,
            Err(error) => {
                unsafe {
                    device.destroy_shader_module(vertex_module, None);
                    device.destroy_pipeline_layout(pipeline_layout, None);
                }
                return Err(error);
            }
        };

        let entry = c"main";
        let stages = [
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::VERTEX)
                .module(vertex_module)
                .name(entry),
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(fragment_module)
                .name(entry),
        ];
        let vertex_bindings = [vk::VertexInputBindingDescription::default()
            .binding(0)
            .stride(std::mem::size_of::<SolidFillInstance>() as u32)
            .input_rate(vk::VertexInputRate::INSTANCE)];
        let vertex_attributes = [
            vk::VertexInputAttributeDescription::default()
                .location(0)
                .binding(0)
                .format(vk::Format::R32G32_SFLOAT)
                .offset(0),
            vk::VertexInputAttributeDescription::default()
                .location(1)
                .binding(0)
                .format(vk::Format::R32G32_SFLOAT)
                .offset(8),
        ];
        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default()
            .vertex_binding_descriptions(&vertex_bindings)
            .vertex_attribute_descriptions(&vertex_attributes);
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
        let color_blend_attachments = [vk::PipelineColorBlendAttachmentState::default()
            .blend_enable(false)
            .color_write_mask(vk::ColorComponentFlags::RGBA)];
        let color_blend = vk::PipelineColorBlendStateCreateInfo::default()
            .logic_op_enable(false)
            .attachments(&color_blend_attachments);
        let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic_state =
            vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
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

        let pipeline = match unsafe {
            device.create_graphics_pipelines(vk::PipelineCache::null(), &[pipeline_info], None)
        } {
            Ok(pipelines) => pipelines[0],
            Err((_, error)) => {
                unsafe {
                    device.destroy_shader_module(vertex_module, None);
                    device.destroy_shader_module(fragment_module, None);
                    device.destroy_pipeline_layout(pipeline_layout, None);
                }
                return Err(error.into());
            }
        };
        unsafe {
            device.destroy_shader_module(vertex_module, None);
            device.destroy_shader_module(fragment_module, None);
        }

        Ok(Self {
            vk,
            pipeline_layout,
            pipeline,
        })
    }

    pub fn pipeline_layout(&self) -> vk::PipelineLayout {
        self.pipeline_layout
    }

    pub fn pipeline(&self) -> vk::Pipeline {
        self.pipeline
    }
}

impl Drop for SolidFillPipelineCache {
    fn drop(&mut self) {
        unsafe {
            let _ = self.vk.device.queue_wait_idle(self.vk.graphics_queue);
            self.vk.device.destroy_pipeline(self.pipeline, None);
            self.vk
                .device
                .destroy_pipeline_layout(self.pipeline_layout, None);
        }
    }
}

fn create_shader_module(
    device: &ash::Device,
    spv_bytes: &[u8],
) -> Result<vk::ShaderModule, SolidFillError> {
    if !spv_bytes.len().is_multiple_of(4) {
        return Err(SolidFillError::SpirvUnaligned(spv_bytes.len()));
    }
    let code: Vec<u32> = spv_bytes
        .chunks_exact(4)
        .map(|chunk| u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect();
    let info = vk::ShaderModuleCreateInfo::default().code(&code);
    Ok(unsafe { device.create_shader_module(&info, None)? })
}
