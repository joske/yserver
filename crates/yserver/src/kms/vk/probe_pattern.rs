//! Deterministic diagnostic pattern for copied reverse-PRIME probing.
//!
//! The caller owns the dynamic-rendering scope and color attachment. This
//! module only binds a small full-screen-triangle pipeline, pushes the target
//! extent plus a frame token, and records the draw. The fragment shader's
//! radial color rays make gross corruption visible while its coordinate-coded
//! low bits and edge fiducials expose subtler pitch, offset, orientation,
//! tiling, and stale/partial-copy failures.

use std::sync::Arc;

use ash::vk;

use super::device::VkContext;

const VERTEX_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/probe_pattern.vert.spv"));
const FRAGMENT_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/probe_pattern.frag.spv"));

/// Matches the shader push-constant block's std430 layout exactly.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProbePatternPushConstants {
    extent: [u32; 2],
    frame_token: u32,
    _pad: u32,
}

const _: () = assert!(std::mem::size_of::<ProbePatternPushConstants>() == 16);
const _: () = assert!(std::mem::offset_of!(ProbePatternPushConstants, frame_token) == 8);

impl ProbePatternPushConstants {
    const fn new(width: u32, height: u32, frame_token: u32) -> Self {
        Self {
            extent: [width, height],
            frame_token,
            _pad: 0,
        }
    }

    fn as_bytes(&self) -> &[u8] {
        // SAFETY: `repr(C)` plus the size/offset assertions above establish a
        // fully initialized 16-byte layout shared with the GLSL block.
        unsafe {
            std::slice::from_raw_parts(
                std::ptr::from_ref::<Self>(self).cast::<u8>(),
                std::mem::size_of::<Self>(),
            )
        }
    }
}

/// RAII owner for the copied-scanout diagnostic pattern pipeline.
pub(crate) struct CopiedProbePatternPipeline {
    vk: Arc<VkContext>,
    pipeline: vk::Pipeline,
    pipeline_layout: vk::PipelineLayout,
}

impl CopiedProbePatternPipeline {
    pub(crate) fn new(vk: Arc<VkContext>) -> Result<Self, vk::Result> {
        let push_constant_ranges = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)
            .offset(0)
            .size(std::mem::size_of::<ProbePatternPushConstants>() as u32)];
        let layout_info =
            vk::PipelineLayoutCreateInfo::default().push_constant_ranges(&push_constant_ranges);
        let pipeline_layout = unsafe { vk.device.create_pipeline_layout(&layout_info, None)? };

        let pipeline = match build_pipeline(&vk.device, pipeline_layout) {
            Ok(pipeline) => pipeline,
            Err(error) => {
                unsafe {
                    vk.device.destroy_pipeline_layout(pipeline_layout, None);
                }
                return Err(error);
            }
        };

        Ok(Self {
            vk,
            pipeline,
            pipeline_layout,
        })
    }

    /// Record one full-attachment probe draw.
    ///
    /// The caller must already be inside dynamic rendering with a
    /// `B8G8R8A8_UNORM` color attachment whose render area covers `width` by
    /// `height`. No vertex buffers or descriptor sets are required.
    pub(crate) fn record(
        &self,
        command_buffer: vk::CommandBuffer,
        width: u32,
        height: u32,
        frame_token: u32,
    ) {
        debug_assert!(width > 0 && height > 0);

        let viewport = [vk::Viewport {
            x: 0.0,
            y: 0.0,
            width: width as f32,
            height: height as f32,
            min_depth: 0.0,
            max_depth: 1.0,
        }];
        let scissors = [vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: vk::Extent2D { width, height },
        }];
        let push_constants = ProbePatternPushConstants::new(width, height, frame_token);

        unsafe {
            self.vk.device.cmd_bind_pipeline(
                command_buffer,
                vk::PipelineBindPoint::GRAPHICS,
                self.pipeline,
            );
            self.vk
                .device
                .cmd_set_viewport(command_buffer, 0, &viewport);
            self.vk.device.cmd_set_scissor(command_buffer, 0, &scissors);
            self.vk.device.cmd_push_constants(
                command_buffer,
                self.pipeline_layout,
                vk::ShaderStageFlags::FRAGMENT,
                0,
                push_constants.as_bytes(),
            );
            self.vk.device.cmd_draw(command_buffer, 3, 1, 0, 0);
        }
    }
}

impl Drop for CopiedProbePatternPipeline {
    fn drop(&mut self) {
        // The normal consuming probe finalizer marks the disposable context
        // known-quiescent, so success and safe rejection skip this wait. This
        // branch is only the defensive fallback for an unfinalized unwind:
        // pipeline objects must not be destroyed while an uncertain submission
        // may still reference them. Device loss permits orderly Vulkan
        // teardown; any other idle failure keeps the context and child handles
        // leaked until process exit rather than risking a use-after-free in the
        // driver.
        if self.vk.requires_drop_device_idle() {
            let wait = unsafe { self.vk.device.device_wait_idle() };
            if !matches!(wait, Ok(()) | Err(vk::Result::ERROR_DEVICE_LOST)) {
                log::warn!(
                    "copied content-probe pipeline: vkDeviceWaitIdle failed during teardown: \
                     {wait:?}; leaking the uncertain pipeline and device context"
                );
                std::mem::forget(Arc::clone(&self.vk));
                return;
            }
        }
        unsafe {
            self.vk.device.destroy_pipeline(self.pipeline, None);
            self.vk
                .device
                .destroy_pipeline_layout(self.pipeline_layout, None);
        }
    }
}

fn build_pipeline(
    device: &ash::Device,
    pipeline_layout: vk::PipelineLayout,
) -> Result<vk::Pipeline, vk::Result> {
    let vertex_module = create_shader_module(device, VERTEX_SPV)?;
    let fragment_module = match create_shader_module(device, FRAGMENT_SPV) {
        Ok(module) => module,
        Err(error) => {
            unsafe { device.destroy_shader_module(vertex_module, None) };
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
    let color_blend_attachments = [vk::PipelineColorBlendAttachmentState::default()
        .blend_enable(false)
        .color_write_mask(vk::ColorComponentFlags::RGBA)];
    let color_blend =
        vk::PipelineColorBlendStateCreateInfo::default().attachments(&color_blend_attachments);
    let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
    let dynamic_state =
        vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
    let color_formats = [vk::Format::B8G8R8A8_UNORM];
    let mut rendering =
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
        .push_next(&mut rendering);

    let result = unsafe {
        device.create_graphics_pipelines(vk::PipelineCache::null(), &[pipeline_info], None)
    };
    unsafe {
        device.destroy_shader_module(vertex_module, None);
        device.destroy_shader_module(fragment_module, None);
    }

    match result {
        Ok(pipelines) => Ok(pipelines[0]),
        Err((pipelines, error)) => {
            unsafe {
                for pipeline in pipelines {
                    device.destroy_pipeline(pipeline, None);
                }
            }
            Err(error)
        }
    }
}

fn create_shader_module(
    device: &ash::Device,
    spirv_bytes: &[u8],
) -> Result<vk::ShaderModule, vk::Result> {
    debug_assert!(spirv_bytes.len().is_multiple_of(4));
    let code: Vec<u32> = spirv_bytes
        .chunks_exact(4)
        .map(|chunk| u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect();
    let info = vk::ShaderModuleCreateInfo::default().code(&code);
    unsafe { device.create_shader_module(&info, None) }
}

#[cfg(test)]
mod tests {
    use super::ProbePatternPushConstants;

    #[test]
    fn push_constants_match_shader_layout() {
        assert_eq!(std::mem::size_of::<ProbePatternPushConstants>(), 16);
        assert_eq!(
            std::mem::offset_of!(ProbePatternPushConstants, frame_token),
            8
        );
    }

    #[test]
    fn frame_tokens_produce_distinct_push_constants() {
        let first = ProbePatternPushConstants::new(120, 128, 0);
        let second = ProbePatternPushConstants::new(120, 128, 1);
        assert_ne!(first, second);
        assert_ne!(first.as_bytes(), second.as_bytes());
    }
}
