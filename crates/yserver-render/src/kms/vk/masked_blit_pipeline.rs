use std::{collections::HashMap, sync::Arc};

use ash::vk;

use crate::kms::vk::device::VkContext;

/// Push constants for the masked_blit draw. `#[repr(C)]`; the field
/// order and offsets MUST match `masked_blit.{vert,frag}.glsl`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct MaskedBlitPushConsts {
    pub(crate) dst_origin: [f32; 2],  // 0
    pub(crate) dst_size: [f32; 2],    // 8
    pub(crate) viewport: [f32; 2],    // 16
    pub(crate) copy_offset: [i32; 2], // 24  src_texel = dst_pixel + copy_offset
    pub(crate) clip_offset: [i32; 2], // 32  mask_texel = dst_pixel - clip_offset
    pub(crate) src_extent: [i32; 2],  // 40
    pub(crate) mask_extent: [i32; 2], // 48
                                      // size = 56
}

const _: () = assert!(std::mem::size_of::<MaskedBlitPushConsts>() == 56);

impl MaskedBlitPushConsts {
    pub(crate) fn as_bytes(&self) -> &[u8] {
        unsafe {
            std::slice::from_raw_parts(
                std::ptr::from_ref::<Self>(self).cast::<u8>(),
                std::mem::size_of::<Self>(),
            )
        }
    }
}

const VERTEX_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/masked_blit.vert.spv"));
const FRAGMENT_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/masked_blit.frag.spv"));

pub(crate) struct MaskedBlitPipeline {
    vk: Arc<VkContext>,
    pub(crate) pipeline_layout: vk::PipelineLayout,
    pub(crate) descriptor_set_layout: vk::DescriptorSetLayout,
    sampler: vk::Sampler,
    pipelines: HashMap<vk::Format, vk::Pipeline>,
}

impl MaskedBlitPipeline {
    pub(crate) fn new(vk: Arc<VkContext>) -> Result<Self, vk::Result> {
        let device = &vk.device;
        // NEAREST sampler: texelFetch ignores it, but COMBINED_IMAGE_SAMPLER
        // descriptors require a sampler object.
        let sampler_info = vk::SamplerCreateInfo::default()
            .mag_filter(vk::Filter::NEAREST)
            .min_filter(vk::Filter::NEAREST)
            .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE);
        let sampler = unsafe { device.create_sampler(&sampler_info, None)? };

        let dsl_bindings = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(1)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
        ];
        let dsl_info = vk::DescriptorSetLayoutCreateInfo::default().bindings(&dsl_bindings);
        let descriptor_set_layout =
            match unsafe { device.create_descriptor_set_layout(&dsl_info, None) } {
                Ok(d) => d,
                Err(e) => {
                    unsafe { device.destroy_sampler(sampler, None) };
                    return Err(e);
                }
            };

        let set_layouts = [descriptor_set_layout];
        let push_const_ranges = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
            .offset(0)
            .size(std::mem::size_of::<MaskedBlitPushConsts>() as u32)];
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
                return Err(e);
            }
        };

        Ok(Self {
            vk,
            pipeline_layout,
            descriptor_set_layout,
            sampler,
            pipelines: HashMap::new(),
        })
    }

    /// Get-or-build the pipeline for a dst color format. Blend is DISABLED
    /// (GXcopy raw copy); the frag writes the src texel verbatim.
    pub(crate) fn pipeline_for(
        &mut self,
        color_format: vk::Format,
    ) -> Result<vk::Pipeline, vk::Result> {
        if let Some(p) = self.pipelines.get(&color_format) {
            return Ok(*p);
        }
        let p = Self::build_pipeline(&self.vk, self.pipeline_layout, color_format)?;
        self.pipelines.insert(color_format, p);
        Ok(p)
    }

    fn build_pipeline(
        vk: &VkContext,
        pipeline_layout: vk::PipelineLayout,
        color_format: vk::Format,
    ) -> Result<vk::Pipeline, vk::Result> {
        let device = &vk.device;
        let vert_module = create_shader_module(device, VERTEX_SPV)?;
        let frag_module = match create_shader_module(device, FRAGMENT_SPV) {
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
        // Blend DISABLED — raw copy. Write all channels (incl. the depth-24 X byte).
        let color_blend_attachments = [vk::PipelineColorBlendAttachmentState::default()
            .blend_enable(false)
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
        let pipeline = match unsafe {
            device.create_graphics_pipelines(vk::PipelineCache::null(), &[pipeline_info], None)
        } {
            Ok(ps) => ps[0],
            Err((_, e)) => {
                unsafe {
                    device.destroy_shader_module(vert_module, None);
                    device.destroy_shader_module(frag_module, None);
                }
                return Err(e);
            }
        };
        unsafe {
            device.destroy_shader_module(vert_module, None);
            device.destroy_shader_module(frag_module, None);
        }
        Ok(pipeline)
    }
}

fn create_shader_module(
    device: &ash::Device,
    spv_bytes: &[u8],
) -> Result<vk::ShaderModule, vk::Result> {
    debug_assert!(spv_bytes.len().is_multiple_of(4));
    let mut code: Vec<u32> = Vec::with_capacity(spv_bytes.len() / 4);
    for chunk in spv_bytes.chunks_exact(4) {
        code.push(u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    let info = vk::ShaderModuleCreateInfo::default().code(&code);
    unsafe { device.create_shader_module(&info, None) }
}

impl MaskedBlitPipeline {
    /// Write src + mask IDENTITY views into a descriptor set acquired from the
    /// ring. Both views MUST be the IDENTITY `image_view` (NOT `sample_view`):
    /// the R8 mask bit lives in `.r` and depth-24 bytes must be raw.
    pub(crate) fn write_views(
        &self,
        set: vk::DescriptorSet,
        src_view: vk::ImageView,
        mask_view: vk::ImageView,
    ) {
        let src_info = [vk::DescriptorImageInfo::default()
            .image_view(src_view)
            .sampler(self.sampler)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
        let mask_info = [vk::DescriptorImageInfo::default()
            .image_view(mask_view)
            .sampler(self.sampler)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
        let writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&src_info),
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&mask_info),
        ];
        unsafe { self.vk.device.update_descriptor_sets(&writes, &[]) };
    }
}

impl Drop for MaskedBlitPipeline {
    fn drop(&mut self) {
        let device = &self.vk.device;
        unsafe {
            for (_, p) in self.pipelines.drain() {
                device.destroy_pipeline(p, None);
            }
            device.destroy_pipeline_layout(self.pipeline_layout, None);
            device.destroy_descriptor_set_layout(self.descriptor_set_layout, None);
            device.destroy_sampler(self.sampler, None);
        }
    }
}
