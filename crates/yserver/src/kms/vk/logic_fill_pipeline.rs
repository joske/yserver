//! Per-`GcFunction` solid-fill pipeline cache (sub-phase 4.1.5).
//!
//! Each X11 GC `function` (Clear / And / Xor / Or / Invert / etc. —
//! 16 variants) maps 1:1 onto a Vulkan `VkLogicOp`. A solid-fill draw
//! quad through a pipeline with `logic_op_enable = true` and the
//! matching op gives the GC's bit-blit semantics in one draw call.
//!
//! Pipelines are built lazily the first time each function is used
//! and cached for the rest of the session. The vertex + fragment
//! shaders are shared (`logic_fill.{vert,frag}.spv`); the
//! per-pipeline difference is just the `VkPipelineColorBlendStateCreateInfo`
//! state.
//!
//! Push constants: 32 bytes — destination rect (origin + size),
//! viewport (for NDC), and the foreground colour as a `vec4`.

use std::{collections::HashMap, sync::Arc};

use ash::vk;
use yserver_protocol::x11::gc::GcFunction;

use super::device::VkContext;

const VERTEX_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/logic_fill.vert.spv"));
const FRAGMENT_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/logic_fill.frag.spv"));

/// 48-byte push-const block. The trailing `_pad` aligns `fg_color`
/// to a 16-byte boundary so the shader's `std430`/`scalar` view of
/// the layout matches the Rust struct's byte offsets.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct LogicFillPushConsts {
    pub dst_origin: [f32; 2], // offset 0
    pub dst_size: [f32; 2],   // offset 8
    pub viewport: [f32; 2],   // offset 16
    pub _pad: [f32; 2],       // offset 24
    pub fg_color: [f32; 4],   // offset 32
}

const _: () = assert!(std::mem::size_of::<LogicFillPushConsts>() == 48);

impl LogicFillPushConsts {
    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: `repr(C)` with f32 fields, no padding (asserted above).
        unsafe {
            std::slice::from_raw_parts(
                std::ptr::from_ref::<Self>(self).cast::<u8>(),
                std::mem::size_of::<Self>(),
            )
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LogicFillError {
    #[error("vulkan: {0:?}")]
    Vk(vk::Result),
    #[error(
        "logic-fill shader SPIR-V from build.rs is malformed (length not multiple of 4): {0} bytes"
    )]
    SpirvUnaligned(usize),
}

/// Which channels a logic fill writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LogicFillChannels {
    /// All four — the depth-32 ARGB path.
    All,
    /// RGB only, preserving the destination's alpha byte — the
    /// depth-24/8/1 (server-owned α) path.
    Color,
    /// Alpha only. With `GcFunction::Set` this stamps alpha = 0xFF and
    /// leaves RGB untouched: how a depth-24 window's pixels are made
    /// opaque inside the depth-32 ancestor backing it paints into, after
    /// a raw copy has carried the source's undefined X byte across.
    Alpha,
}

impl LogicFillChannels {
    /// The pre-enum `opaque_alpha` flag: `true` preserves alpha.
    #[must_use]
    pub fn from_opaque_alpha(opaque_alpha: bool) -> Self {
        if opaque_alpha { Self::Color } else { Self::All }
    }

    fn key(self) -> u8 {
        match self {
            Self::All => 0,
            Self::Color => 1,
            Self::Alpha => 2,
        }
    }

    fn write_mask(self) -> vk::ColorComponentFlags {
        match self {
            Self::All => vk::ColorComponentFlags::RGBA,
            Self::Color => {
                vk::ColorComponentFlags::R | vk::ColorComponentFlags::G | vk::ColorComponentFlags::B
            }
            Self::Alpha => vk::ColorComponentFlags::A,
        }
    }
}

impl From<vk::Result> for LogicFillError {
    fn from(r: vk::Result) -> Self {
        LogicFillError::Vk(r)
    }
}

pub struct LogicFillPipelineCache {
    vk: Arc<VkContext>,
    pipeline_layout: vk::PipelineLayout,
    /// Keyed by `(function_key, LogicFillChannels::key)`. The
    /// `opaque_alpha=true` variant masks the alpha channel out of
    /// the color write so the LogicOp only affects RGB — preserves
    /// the L1 server-owned α invariant on depth-24 destinations.
    /// `opaque_alpha=false` is the depth-32 ARGB path where the
    /// LogicOp applies to all four channels per X11 semantics.
    pipelines: HashMap<(u8, u8), vk::Pipeline>,
    color_format: vk::Format,
}

impl LogicFillPipelineCache {
    pub fn new(vk: Arc<VkContext>, color_format: vk::Format) -> Result<Self, LogicFillError> {
        let device = &vk.device;
        let push_const_ranges = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
            .offset(0)
            .size(std::mem::size_of::<LogicFillPushConsts>() as u32)];
        let pl_info =
            vk::PipelineLayoutCreateInfo::default().push_constant_ranges(&push_const_ranges);
        let pipeline_layout = unsafe { device.create_pipeline_layout(&pl_info, None)? };
        Ok(Self {
            vk,
            pipeline_layout,
            pipelines: HashMap::new(),
            color_format,
        })
    }

    pub fn pipeline_layout(&self) -> vk::PipelineLayout {
        self.pipeline_layout
    }

    /// Look up or build the pipeline for `function` with the
    /// matching α policy. `opaque_alpha = true` is the depth-24
    /// (server-owned α) path: the color blend attachment's write
    /// mask drops alpha so the `VkLogicOp` only mutates RGB and
    /// the destination's existing α byte is left intact (the L1
    /// invariant: paint paths must never demote a previously-opaque
    /// pixel to transparent). `false` is the depth-32 ARGB path —
    /// LogicOp applies to all four channels.
    pub fn get(
        &mut self,
        function: GcFunction,
        channels: LogicFillChannels,
    ) -> Result<vk::Pipeline, LogicFillError> {
        let key = (function_key(function), channels.key());
        if let Some(p) = self.pipelines.get(&key) {
            return Ok(*p);
        }
        let p = build_pipeline(
            &self.vk,
            self.pipeline_layout,
            function,
            self.color_format,
            channels,
        )?;
        self.pipelines.insert(key, p);
        Ok(p)
    }
}

impl Drop for LogicFillPipelineCache {
    fn drop(&mut self) {
        unsafe {
            let _ = self.vk.device.queue_wait_idle(self.vk.graphics_queue);
            for &p in self.pipelines.values() {
                self.vk.device.destroy_pipeline(p, None);
            }
            self.vk
                .device
                .destroy_pipeline_layout(self.pipeline_layout, None);
        }
    }
}

fn function_key(f: GcFunction) -> u8 {
    // Numeric encoding matches the X11 protocol GC function values.
    match f {
        GcFunction::Clear => 0,
        GcFunction::And => 1,
        GcFunction::AndReverse => 2,
        GcFunction::Copy => 3,
        GcFunction::AndInverted => 4,
        GcFunction::NoOp => 5,
        GcFunction::Xor => 6,
        GcFunction::Or => 7,
        GcFunction::Nor => 8,
        GcFunction::Equiv => 9,
        GcFunction::Invert => 10,
        GcFunction::OrReverse => 11,
        GcFunction::CopyInverted => 12,
        GcFunction::OrInverted => 13,
        GcFunction::Nand => 14,
        GcFunction::Set => 15,
    }
}

fn function_to_logic_op(f: GcFunction) -> vk::LogicOp {
    match f {
        GcFunction::Clear => vk::LogicOp::CLEAR,
        GcFunction::And => vk::LogicOp::AND,
        GcFunction::AndReverse => vk::LogicOp::AND_REVERSE,
        GcFunction::Copy => vk::LogicOp::COPY,
        GcFunction::AndInverted => vk::LogicOp::AND_INVERTED,
        GcFunction::NoOp => vk::LogicOp::NO_OP,
        GcFunction::Xor => vk::LogicOp::XOR,
        GcFunction::Or => vk::LogicOp::OR,
        GcFunction::Nor => vk::LogicOp::NOR,
        GcFunction::Equiv => vk::LogicOp::EQUIVALENT,
        GcFunction::Invert => vk::LogicOp::INVERT,
        GcFunction::OrReverse => vk::LogicOp::OR_REVERSE,
        GcFunction::CopyInverted => vk::LogicOp::COPY_INVERTED,
        GcFunction::OrInverted => vk::LogicOp::OR_INVERTED,
        GcFunction::Nand => vk::LogicOp::NAND,
        GcFunction::Set => vk::LogicOp::SET,
    }
}

fn build_pipeline(
    vk: &VkContext,
    pipeline_layout: vk::PipelineLayout,
    function: GcFunction,
    color_format: vk::Format,
    channels: LogicFillChannels,
) -> Result<vk::Pipeline, LogicFillError> {
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

    let write_mask = channels.write_mask();
    let color_blend_attachments = [vk::PipelineColorBlendAttachmentState::default()
        .blend_enable(false)
        .color_write_mask(write_mask)];
    let color_blend = vk::PipelineColorBlendStateCreateInfo::default()
        .logic_op_enable(true)
        .logic_op(function_to_logic_op(function))
        .attachments(&color_blend_attachments);

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
) -> Result<vk::ShaderModule, LogicFillError> {
    if !spv_bytes.len().is_multiple_of(4) {
        return Err(LogicFillError::SpirvUnaligned(spv_bytes.len()));
    }
    let mut code: Vec<u32> = Vec::with_capacity(spv_bytes.len() / 4);
    for chunk in spv_bytes.chunks_exact(4) {
        code.push(u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    let info = vk::ShaderModuleCreateInfo::default().code(&code);
    Ok(unsafe { device.create_shader_module(&info, None)? })
}
