//! Graphics pipeline + sampler + descriptor set for the glyph-render
//! pass (sub-phase 4.1.4.5).
//!
//! Mirrors [`pipeline::CompositorPipeline`](super::pipeline) very
//! closely; the differences are the shaders (`text.vert.spv` /
//! `text.frag.spv`), the descriptor binding (single combined
//! image-sampler at binding 0 sampled by the fragment shader as an
//! `R8_UNORM` alpha mask), and the push-constant layout
//! (vec2 quintuple + vec4 foreground = 56 bytes).
//!
//! The atlas image view is bound *once* at pipeline construction —
//! [`GlyphAtlas`](super::glyph::GlyphAtlas) is a single
//! backend-wide image whose handle never changes for the rest of
//! the session — so a single descriptor set serves every
//! [`record_text_run`](super::ops::text::record_text_run) call. No
//! per-frame descriptor pool reset like the compositor pipeline.
//!
//! Sampler is `NEAREST`. Glyph quads in the atlas are tightly
//! packed (no padding), so `LINEAR` filtering would pull in
//! adjacent-glyph pixels at the edges and produce visible
//! bleeding artefacts (seen on bare-metal xterm runs). FreeType
//! already produces anti-aliased alpha bitmaps, so the sampler
//! doesn't need to do further blending — `NEAREST` lookup is
//! exactly the right thing.
//!
//! Color attachment format is `B8G8R8A8_UNORM` for window/pixmap
//! mirrors, or `R8_UNORM` for depth-8 a8 mask pixmaps (the
//! cairo/Pango component-alpha text intermediate — glyph coverage
//! accumulated with `op=Add`). Blend state is derived from
//! [`StdPictOp::blend_factors`] so glyph compositing and the general
//! RENDER `Composite` path agree on Porter-Duff semantics by
//! construction; the engine caches one pipeline per
//! `(op, dst_format, dst_has_alpha, component_alpha)` key — the same
//! four dimensions [`RenderPipelineCache`](super::render_pipeline)
//! keys on, so the two caches cannot disagree about which blend state
//! a given composite gets. The historical Over+BGRA8 entry is
//! bit-identical to the pre-cache singleton:
//! `blend_factors(Over, BGRA8, _, false)` yields exactly
//! `(ONE, ONE_MINUS_SRC_ALPHA)`.
//!
//! `component_alpha` is set for a run of four-plane packed glyphs
//! (subpixel/LCD text AA): the fragment shader then gathers logical
//! R, G, B and A coverage from four adjacent atlas texel runs and
//! emits the per-channel alpha factor to output INDEX 1 of
//! attachment 0, which the `SRC1_*` blend factors consume.

use std::sync::Arc;

use ash::vk;

use super::{device::VkContext, glyph::GlyphLayout, render_pipeline::StdPictOp};

const VERTEX_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/text.vert.spv"));
const FRAGMENT_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/text.frag.spv"));

/// Push constants for the glyph quad shader — now PER-RUN only
/// (sub-phase #1 glyph batching): 32 bytes = two `vec2` + one
/// `vec4`. Per-glyph dst rect + atlas coords moved to
/// [`GlyphInstanceData`] (instance-rate vertex attributes). The
/// shader's push-constant block uses std430 layout. Two consecutive
/// `vec2` values occupy bytes 0..16, so the following `vec4` naturally
/// starts at offset 16, exactly matching this `repr(C)` structure.
/// The `offset_of!` assertions below guard that contract without
/// requiring the optional `scalarBlockLayout` Vulkan feature.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct TextPushConsts {
    /// Dst mirror viewport in pixels (NDC divisor).
    pub viewport: [f32; 2],
    /// Glyph atlas extent in texels (UV divisor). Read at emit time
    /// from the current atlas, so a future atlas grow/repack stays
    /// correct without touching recorded instance data.
    pub atlas_extent: [f32; 2],
    /// RGB foreground, alpha set to 1.0 by the caller.
    pub foreground: [f32; 4],
}

impl TextPushConsts {
    pub fn new(viewport: [f32; 2], atlas_extent: [f32; 2], foreground: [f32; 4]) -> Self {
        Self {
            viewport,
            atlas_extent,
            foreground,
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: `repr(C)` with `f32` fields, no padding (asserted
        // below). Any byte pattern is a valid `f32`; round-tripping
        // through `&[u8]` is safe.
        unsafe {
            std::slice::from_raw_parts(
                std::ptr::from_ref::<Self>(self).cast::<u8>(),
                std::mem::size_of::<Self>(),
            )
        }
    }
}

const _: () = assert!(std::mem::size_of::<TextPushConsts>() == 32);
const _: () = assert!(std::mem::offset_of!(TextPushConsts, atlas_extent) == 8);
const _: () = assert!(std::mem::offset_of!(TextPushConsts, foreground) == 16);

/// Per-glyph instance data for the batched glyph draw (#1). One
/// `vkCmdDraw(4, N_glyphs, ..)` reads N of these as instance-rate
/// vertex attributes; the 4-vertex quad still comes from
/// `gl_VertexIndex`. Layout: 4 × `vec2` + one `u32` = 36 bytes,
/// split into vertex attributes at offsets 0/8/16/24/32. Atlas
/// coords are stored in **texels** (not normalized UVs) — the vertex
/// shader divides by the `atlas_extent` push constant — so recorded
/// instance data is independent of any later atlas resize.
///
/// **Every width in here is the glyph's LOGICAL width.** A
/// component-alpha glyph's atlas footprint is `4 * logical_w` texels
/// (four adjacent coverage planes) and that number must never reach
/// the shader: as `atlas_wh` it stretches one glyph across all four
/// planes, and as `dst_size` it paints a quad four times too wide
/// (design invariant 9). The planes are reached by `texelFetch`
/// through `plane_stride` instead.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct GlyphInstanceData {
    /// Dst top-left corner, pixels.
    pub dst_origin: [f32; 2],
    /// Dst quad size, pixels. Also the fragment shader's `v_local`
    /// span, from which it recovers the integer texel index inside
    /// the glyph.
    pub dst_size: [f32; 2],
    /// Atlas top-left corner, texels. Delivered `flat` to the
    /// fragment shader as the component-alpha fetch origin.
    pub atlas_xy: [f32; 2],
    /// The glyph's LOGICAL size in texels — never the packed
    /// footprint. Feeds the A8 path's interpolated UV; unused by the
    /// component-alpha path, which fetches by texel index.
    pub atlas_wh: [f32; 2],
    /// Texels between adjacent packed coverage planes, `flat` to the
    /// fragment shader. `logical_w` for a `ComponentAlpha` entry,
    /// `0` for a single-plane `A8` one (where nothing reads it).
    ///
    /// Carried explicitly even though it equals `dst_size.x` today:
    /// that identity holds only because glyph blitting is 1:1, and
    /// deriving from it couples the sampling to an unrelated
    /// invariant whose failure mode is a glyph sampling its
    /// NEIGHBOUR's plane.
    pub plane_stride: u32,
}

impl GlyphInstanceData {
    /// Build instance data for one glyph from integer dst position,
    /// atlas texel coords, the glyph's **logical** size and the atlas
    /// entry's [`GlyphLayout`]. Returns `None` for a zero-area glyph
    /// (it contributes nothing — e.g. a space after pen-only
    /// adjustment), so the caller drops it and it never becomes a
    /// wasted instance.
    #[must_use]
    pub fn from_glyph(
        dst_x: i32,
        dst_y: i32,
        atlas_x: u32,
        atlas_y: u32,
        logical_w: u32,
        h: u32,
        layout: GlyphLayout,
    ) -> Option<Self> {
        if logical_w == 0 || h == 0 {
            return None;
        }
        Some(Self {
            dst_origin: [dst_x as f32, dst_y as f32],
            dst_size: [logical_w as f32, h as f32],
            atlas_xy: [atlas_x as f32, atlas_y as f32],
            atlas_wh: [logical_w as f32, h as f32],
            plane_stride: match layout {
                // Single plane: the fragment shader samples the
                // interpolated UV and never reads the stride.
                GlyphLayout::A8 => 0,
                // Four planes of `logical_w` texels each.
                GlyphLayout::ComponentAlpha => logical_w,
            },
        })
    }

    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: `repr(C)` with `f32` fields, no padding (asserted
        // below); the slice never outlives `self`.
        unsafe {
            std::slice::from_raw_parts(
                std::ptr::from_ref::<Self>(self).cast::<u8>(),
                std::mem::size_of::<Self>(),
            )
        }
    }
}

// Compile-time lock on the vertex-input layout, the same treatment
// `TextPushConsts` gets. The vertex attribute descriptions below
// hard-code these offsets and the binding stride; a field reorder
// that slid `plane_stride` under `atlas_wh`'s attribute would
// silently feed the shader garbage.
const _: () = assert!(std::mem::size_of::<GlyphInstanceData>() == 36);
const _: () = assert!(std::mem::offset_of!(GlyphInstanceData, dst_size) == 8);
const _: () = assert!(std::mem::offset_of!(GlyphInstanceData, atlas_xy) == 16);
const _: () = assert!(std::mem::offset_of!(GlyphInstanceData, atlas_wh) == 24);
const _: () = assert!(std::mem::offset_of!(GlyphInstanceData, plane_stride) == 32);

pub struct TextPipeline {
    vk: Arc<VkContext>,
    pub pipeline: vk::Pipeline,
    pub pipeline_layout: vk::PipelineLayout,
    descriptor_set_layout: vk::DescriptorSetLayout,
    sampler: vk::Sampler,
    descriptor_pool: vk::DescriptorPool,
    pub descriptor_set: vk::DescriptorSet,
    pub color_format: vk::Format,
}

#[derive(Debug, thiserror::Error)]
pub enum TextPipelineError {
    #[error("vulkan: {0:?}")]
    Vk(vk::Result),
    #[error("text shader SPIR-V from build.rs is malformed (length not multiple of 4): {0} bytes")]
    SpirvUnaligned(usize),
}

impl From<vk::Result> for TextPipelineError {
    fn from(r: vk::Result) -> Self {
        TextPipelineError::Vk(r)
    }
}

impl TextPipeline {
    /// Build the text pipeline against the supplied glyph-atlas
    /// image view. The atlas image view is captured into the
    /// pipeline's single descriptor set at construction time; the
    /// atlas image itself must outlive the pipeline. Stage 3a's v2
    /// glyph atlas uses this directly; v1 passes its
    /// `GlyphAtlas::image_view()`.
    ///
    /// `op` + `dst_has_alpha` + `component_alpha` select the
    /// fixed-function blend state via [`StdPictOp::blend_factors`]
    /// (standard ops 0..=12 only — the caller gates out
    /// Saturate/Disjoint/Conjoint, which need dst readback the text
    /// shader doesn't implement). An `R8_UNORM` `color_format` sets
    /// the `A8_DST` specialization constant so the fragment shader
    /// replicates alpha across all channels (the a8 mask stores alpha
    /// in `.r`).
    ///
    /// `component_alpha` is the glyph run's effective
    /// [`GlyphLayout`] — true for a four-plane packed entry. It sets
    /// the fragment shader's `COMPONENT_ALPHA` constant AND swaps the
    /// `SRC_ALPHA` blend family for `SRC1_*` (dual-source), and both
    /// come from the ONE `blend_factors` table `render_pipeline.rs`
    /// uses, so the glyph and general RENDER paths cannot disagree on
    /// component-alpha blend state by review error (design invariant
    /// 5).
    ///
    /// The caller must have gated `component_alpha` on
    /// `VkContext::component_alpha_supported` — the engine does it
    /// once, in `effective_glyph_layout`, so the atlas layout, the
    /// pipeline key and the blend state answer the device question in
    /// exactly one place. Asserted below.
    pub fn new(
        vk: Arc<VkContext>,
        color_format: vk::Format,
        op: StdPictOp,
        dst_has_alpha: bool,
        component_alpha: bool,
        atlas_image_view: vk::ImageView,
    ) -> Result<Self, TextPipelineError> {
        debug_assert!(
            !component_alpha || vk.component_alpha_supported,
            "component-alpha text pipeline requested on a device without dualSrcBlend; \
             the layout derivation must gate on component_alpha_supported",
        );
        let device = &vk.device;

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

        let set_layouts = [descriptor_set_layout];
        let push_const_ranges = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
            .offset(0)
            .size(std::mem::size_of::<TextPushConsts>() as u32)];
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

        let vert_module = create_shader_module(device, VERTEX_SPV)?;
        let frag_module = match create_shader_module(device, FRAGMENT_SPV) {
            Ok(m) => m,
            Err(e) => {
                unsafe {
                    device.destroy_shader_module(vert_module, None);
                    device.destroy_pipeline_layout(pipeline_layout, None);
                    device.destroy_descriptor_set_layout(descriptor_set_layout, None);
                    device.destroy_sampler(sampler, None);
                }
                return Err(e);
            }
        };

        let entry = c"main";
        // Fragment-shader specialization constants (mirrors
        // render_pipeline.rs):
        //   id 0: A8_DST — 1 → replicate computed alpha across all
        //         channels so an R8 attachment stores alpha in `.r`.
        //   id 1: COMPONENT_ALPHA — 1 → gather the glyph's FOUR packed
        //         coverage planes (logical R, G, B, A) and emit the
        //         per-channel alpha factor to output index 1, which the
        //         `SRC1_*` blend factors below consume.
        let a8_dst: u32 = u32::from(color_format == vk::Format::R8_UNORM);
        let ca: u32 = u32::from(component_alpha);
        let mut spec_data = [0u8; 8];
        spec_data[..4].copy_from_slice(&a8_dst.to_ne_bytes());
        spec_data[4..].copy_from_slice(&ca.to_ne_bytes());
        let spec_map_entries = [
            vk::SpecializationMapEntry::default()
                .constant_id(0)
                .offset(0)
                .size(4),
            vk::SpecializationMapEntry::default()
                .constant_id(1)
                .offset(4)
                .size(4),
        ];
        let spec_info = vk::SpecializationInfo::default()
            .map_entries(&spec_map_entries)
            .data(&spec_data);
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

        // Per-instance vertex input (#1 glyph batching). One binding
        // (stride = 32), four vec2 attributes decomposing
        // `GlyphInstanceData`. INSTANCE rate so the same 4 quad verts
        // (TRIANGLE_STRIP from gl_VertexIndex) re-use one attribute set
        // per glyph — one `vkCmdDraw(4, N, ..)` draws the whole run.
        let vi_bindings = [vk::VertexInputBindingDescription::default()
            .binding(0)
            .stride(std::mem::size_of::<GlyphInstanceData>() as u32)
            .input_rate(vk::VertexInputRate::INSTANCE)];
        let vi_attributes = [
            // location 0: dst_origin : vec2 (offset 0)
            vk::VertexInputAttributeDescription::default()
                .location(0)
                .binding(0)
                .format(vk::Format::R32G32_SFLOAT)
                .offset(0),
            // location 1: dst_size : vec2 (offset 8)
            vk::VertexInputAttributeDescription::default()
                .location(1)
                .binding(0)
                .format(vk::Format::R32G32_SFLOAT)
                .offset(8),
            // location 2: atlas_xy : vec2 (offset 16)
            vk::VertexInputAttributeDescription::default()
                .location(2)
                .binding(0)
                .format(vk::Format::R32G32_SFLOAT)
                .offset(16),
            // location 3: atlas_wh : vec2 (offset 24) — LOGICAL size
            vk::VertexInputAttributeDescription::default()
                .location(3)
                .binding(0)
                .format(vk::Format::R32G32_SFLOAT)
                .offset(24),
            // location 4: plane_stride : uint (offset 32)
            vk::VertexInputAttributeDescription::default()
                .location(4)
                .binding(0)
                .format(vk::Format::R32_UINT)
                .offset(32),
        ];
        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default()
            .vertex_binding_descriptions(&vi_bindings)
            .vertex_attribute_descriptions(&vi_attributes);
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
        // Per-op fixed-function blend from the shared PictOp table
        // (same factors applied to colour and alpha channels, like
        // render_pipeline.rs). For (Over, BGRA8) this is exactly the
        // historical premultiplied src-over pair
        // (ONE, ONE_MINUS_SRC_ALPHA).
        let (src_factor, dst_factor) = op.blend_factors(
            color_format,
            dst_has_alpha,
            // The glyph path DOES have a dual-source output now:
            // `text.frag.glsl` declares `layout(location = 0, index = 1)`
            // exactly as `render.frag.glsl:66` does, so `SRC1_COLOR` /
            // `ONE_MINUS_SRC1_COLOR` resolve to the per-channel alpha
            // factor `fg.a * cov.rgb`.
            component_alpha,
        );
        // Same destination-side force-opaque rule the general
        // composite path uses: a depth-24 / xRGB destination has no
        // client-meaningful α, so a glyph draw must not store one.
        // See `render_pipeline::dst_color_write_mask`.
        let color_blend_attachments = [vk::PipelineColorBlendAttachmentState::default()
            .blend_enable(true)
            .src_color_blend_factor(src_factor)
            .dst_color_blend_factor(dst_factor)
            .color_blend_op(vk::BlendOp::ADD)
            .src_alpha_blend_factor(src_factor)
            .dst_alpha_blend_factor(dst_factor)
            .alpha_blend_op(vk::BlendOp::ADD)
            .color_write_mask(crate::kms::vk::render_pipeline::dst_color_write_mask(
                color_format,
                dst_has_alpha,
            ))];
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
                    device.destroy_pipeline_layout(pipeline_layout, None);
                    device.destroy_descriptor_set_layout(descriptor_set_layout, None);
                    device.destroy_sampler(sampler, None);
                }
                return Err(e.into());
            }
        };
        unsafe {
            device.destroy_shader_module(vert_module, None);
            device.destroy_shader_module(frag_module, None);
        }

        // Single descriptor set bound to the atlas. Atlas image
        // view never changes; we never reset or re-allocate.
        let pool_sizes = [vk::DescriptorPoolSize::default()
            .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)];
        let pool_info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(1)
            .pool_sizes(&pool_sizes);
        let descriptor_pool = match unsafe { device.create_descriptor_pool(&pool_info, None) } {
            Ok(p) => p,
            Err(e) => {
                unsafe {
                    device.destroy_pipeline(pipeline, None);
                    device.destroy_pipeline_layout(pipeline_layout, None);
                    device.destroy_descriptor_set_layout(descriptor_set_layout, None);
                    device.destroy_sampler(sampler, None);
                }
                return Err(e.into());
            }
        };
        let alloc_layouts = [descriptor_set_layout];
        let alloc_info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(descriptor_pool)
            .set_layouts(&alloc_layouts);
        let descriptor_set = match unsafe { device.allocate_descriptor_sets(&alloc_info) } {
            Ok(sets) => sets[0],
            Err(e) => {
                unsafe {
                    device.destroy_descriptor_pool(descriptor_pool, None);
                    device.destroy_pipeline(pipeline, None);
                    device.destroy_pipeline_layout(pipeline_layout, None);
                    device.destroy_descriptor_set_layout(descriptor_set_layout, None);
                    device.destroy_sampler(sampler, None);
                }
                return Err(e.into());
            }
        };
        let image_info = [vk::DescriptorImageInfo::default()
            .image_view(atlas_image_view)
            .sampler(sampler)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
        let writes = [vk::WriteDescriptorSet::default()
            .dst_set(descriptor_set)
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .image_info(&image_info)];
        unsafe { device.update_descriptor_sets(&writes, &[]) };

        Ok(Self {
            vk,
            pipeline,
            pipeline_layout,
            descriptor_set_layout,
            sampler,
            descriptor_pool,
            descriptor_set,
            color_format,
        })
    }
}

impl Drop for TextPipeline {
    fn drop(&mut self) {
        unsafe {
            let _ = self.vk.device.queue_wait_idle(self.vk.graphics_queue);
            self.vk
                .device
                .destroy_descriptor_pool(self.descriptor_pool, None);
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

fn create_shader_module(
    device: &ash::Device,
    spv_bytes: &[u8],
) -> Result<vk::ShaderModule, TextPipelineError> {
    if !spv_bytes.len().is_multiple_of(4) {
        return Err(TextPipelineError::SpirvUnaligned(spv_bytes.len()));
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
    use super::{GlyphInstanceData, GlyphLayout};

    #[test]
    fn from_glyph_maps_dst_and_texel_coords() {
        // dst position is signed pixels; atlas coords are texels; the
        // glyph size (w,h) is shared by the dst quad and the atlas rect
        // (atlas is 1:1 with glyph pixels). Sampler-space normalization
        // by atlas_extent happens in the shader, so instance data holds
        // raw texels here.
        let g =
            GlyphInstanceData::from_glyph(-3, 12, 40, 128, 7, 9, GlyphLayout::A8).expect("nonzero");
        assert_eq!(g.dst_origin, [-3.0, 12.0]);
        assert_eq!(g.dst_size, [7.0, 9.0]);
        assert_eq!(g.atlas_xy, [40.0, 128.0]);
        assert_eq!(g.atlas_wh, [7.0, 9.0]);
        // A single-plane glyph has no plane stride; the fragment
        // shader's A8 branch never reads it.
        assert_eq!(g.plane_stride, 0);
    }

    #[test]
    fn from_glyph_drops_zero_area() {
        assert!(GlyphInstanceData::from_glyph(0, 0, 5, 5, 0, 9, GlyphLayout::A8).is_none());
        assert!(GlyphInstanceData::from_glyph(0, 0, 5, 5, 9, 0, GlyphLayout::A8).is_none());
    }

    /// A component-alpha glyph's instance data carries the LOGICAL
    /// width in both geometry fields and the plane stride separately.
    ///
    /// The packed atlas footprint is `4 * logical_w`; the two ways it
    /// can leak are `atlas_wh = 4w` (one glyph stretched across all
    /// four planes) and `dst_size = 4w` (a quad four times too wide).
    /// Neither number appears here, which is design invariant 9 —
    /// asserted at the only struct that reaches the shader.
    #[test]
    fn a_component_alpha_glyph_carries_the_logical_width_and_an_explicit_plane_stride() {
        let g = GlyphInstanceData::from_glyph(4, 5, 64, 32, 7, 9, GlyphLayout::ComponentAlpha)
            .expect("nonzero");
        assert_eq!(g.dst_size, [7.0, 9.0], "dst quad is the logical width");
        assert_eq!(
            g.atlas_wh,
            [7.0, 9.0],
            "atlas_wh is the logical width — 4 * 7 = 28 would stretch the \
             glyph across all four planes",
        );
        assert_eq!(
            g.plane_stride, 7,
            "the stride between adjacent planes is one logical width",
        );
        // The packed footprint is nowhere in the instance data.
        assert!(
            !g.atlas_wh.contains(&28.0) && !g.dst_size.contains(&28.0),
            "the 4w allocation width must never reach the shader",
        );
    }
}
