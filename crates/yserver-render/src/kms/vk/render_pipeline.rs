//! Lazy-built pipeline cache for RENDER `Composite` (sub-phase
//! 4.1.4.6).
//!
//! Spec: docs/superpowers/specs/2026-05-07-phase4-1-vulkan-compositor-design.md
//! "Render attribute matrix".
//!
//! All 38 X RENDER PictOps land in this cache:
//!   * standard family (0..=12) — fixed-function blend.
//!   * Saturate (13), Disjoint (16..=27), Conjoint (32..=43) —
//!     manual shader-side blend reading the dst pixel from a
//!     [`super::dst_readback::DstReadback`] scratch.
//!
//! Each pipeline is keyed on `(op, dst_format, dst_has_alpha,
//! component_alpha)`. Built lazily on first use; cached for the
//! session.
//!
//! Affine transforms + the four X11 RENDER repeat modes (None /
//! Normal / Pad / Reflect) are handled in shader; UV maths lives
//! in the fragment stage so a single sampler covers every repeat
//! mode and the descriptor cost stays at one set per call.
//!
//! Component-alpha pictures emit a second fragment output (the
//! per-channel src alpha factor) — pipelines built with
//! `component_alpha = true` substitute the SRC1_* blend factor
//! family for the regular SRC_ALPHA / ONE_MINUS_SRC_ALPHA cases,
//! which Vulkan implements via the `dualSrcBlend` feature.
//!
//! ## Out of scope
//!
//! - `alpha_map` (compositor's per-source alpha-mask attribute).
//! - Filters beyond NEAREST (Bilinear, Convolution).
//! - Persistent `~/.cache/yserver/pipeline-cache.bin`.
//! - General projective transforms (`m[2][0]` or `m[2][1]` non-zero);
//!   the constant-divisor case `m[2] = [0, 0, w]` is handled by
//!   `pixman_transform_to_affine` pre-dividing the affine portion.
//!
//! Caller drops the call (no-op) when any of those attributes is
//! in play.

use std::{collections::HashMap, sync::Arc};

use ash::vk;

use super::device::VkContext;

const VERTEX_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/render.vert.spv"));
const FRAGMENT_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/render.frag.spv"));

/// Push constants for the `render.{vert,frag}.glsl` shader. 128 bytes —
/// matches the 128-byte minimum `maxPushConstantsSize`. Carries the
/// destination quad, the source/mask origins + extents, the repeat
/// modes (per the X11 RENDER picture attribute), and an affine 2×3
/// transform per source/mask. Real X11 clients use affine transforms
/// in practice; projective transforms (non-`[0 0 1]` bottom row) round-
/// trip through the affine portion only.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct RenderPushConsts {
    pub dst_origin: [f32; 2],      // 0
    pub dst_size: [f32; 2],        // 8
    pub viewport: [f32; 2],        // 16
    pub src_origin: [f32; 2],      // 24
    pub mask_origin: [f32; 2],     // 32
    pub src_extent: [f32; 2],      // 40
    pub mask_extent: [f32; 2],     // 48
    pub repeat_modes: [i32; 2],    // 56 — (src, mask)
    pub src_xform_row0: [f32; 4],  // 64
    pub src_xform_row1: [f32; 4],  // 80
    pub mask_xform_row0: [f32; 4], // 96
    pub mask_xform_row1: [f32; 4], // 112
                                   // size = 128
}

const _: () = assert!(std::mem::size_of::<RenderPushConsts>() == 128);

/// X11 RENDER repeat-mode constants. Numeric values match the
/// `Repeat` enum in the `pixman` crate so call sites can keep
/// converting via `as i32`.
///
/// ## `RenderPushConsts::repeat_modes[i]` bit layout
///
/// `repeat_modes[0]` is the src picture's encoding,
/// `repeat_modes[1]` the mask's. The push-constants are tight
/// against the 128-byte `maxPushConstantsSize` minimum, so we
/// pack the X11 Render `PictFormat` force-opaque flag into the
/// upper bits of the existing `i32` rather than growing the
/// struct. Shader-side mask: `& REPEAT_MODE_MASK` for the repeat
/// mode, `& REPEAT_FORCE_OPAQUE_BIT` for the force-opaque flag.
///
/// ```text
///   bits  0..=7   — repeat mode (REPEAT_NONE..=REPEAT_REFLECT)
///   bit   8       — force_opaque (1 if the picture's source format
///                   has alpha_mask = 0 per X11 Render `PictFormat`)
///   bits  9..=31  — reserved (must be 0)
/// ```
pub const REPEAT_NONE: i32 = 0;
pub const REPEAT_NORMAL: i32 = 1;
pub const REPEAT_PAD: i32 = 2;
pub const REPEAT_REFLECT: i32 = 3;

/// Mask for the repeat-mode bits of `RenderPushConsts::repeat_modes[i]`
/// — `repeat_modes[i] & REPEAT_MODE_MASK` recovers the repeat constant.
pub const REPEAT_MODE_MASK: i32 = 0xff;
/// Force-opaque bit of `RenderPushConsts::repeat_modes[i]`. When set,
/// the fragment shader pins the sampled alpha to 1.0 regardless of the
/// texel's actual alpha — the spec behaviour for picture formats whose
/// `alpha_mask` is 0 (e.g. depth-24 RGB visuals).
pub const REPEAT_FORCE_OPAQUE_BIT: i32 = 1 << 8;

/// Colour write mask for a RENDER composite whose destination has no
/// client-meaningful alpha channel — the **destination** half of the
/// `REPEAT_FORCE_OPAQUE_BIT` rule.
///
/// On X11 a depth-24 drawable is opaque by definition: its picture
/// format (`PICT_x8r8g8b8` / `PICT_r8g8b8`) declares `alpha_mask = 0`,
/// so there is no alpha to store. Xorg gets this for free because
/// pixman's representation has no alpha bits to write — storing an
/// `a8r8g8b8` pixel into an `x8r8g8b8` image drops the alpha channel
/// outright (`pixman-access.c:254-261`, `convert_channel` returns 0
/// masked to 0 bits) and every fetch substitutes `0xff`
/// (`pixman-access.c:270-276`, `def_value = ~0`). Xorg's own RENDER
/// layer states the same invariant: `ReduceCompositeOp` treats
/// `PICT_FORMAT_A(pDst->format) == 0` as "the destination alpha is
/// always 1" (`render/picture.c:1456-1457`, `:1487-1488`).
///
/// We store depth-24 as `B8G8R8A8_UNORM`, which *does* have a real
/// alpha byte, so the equivalence has to be enforced. The read side
/// already is — [`PictOp::blend_factors`] substitutes `ONE`/`ZERO`
/// for the `DST_ALPHA` factors when `dst_has_alpha` is false, and
/// `DstReadback::view` binds an `a = ONE` swizzle for the shader-side
/// blend. The **write** side was not: the fixed-function blend still
/// evaluated the alpha channel and stored the result, so e.g.
/// `PictOpOver` with a half-transparent source left `α = 127` in a
/// depth-24 backing (measured on a mate-terminal frame) and
/// `PictOpSrc` / `PictOpClear` left `α = 0`. A depth-32 compositing
/// client then blends a hole X11 says cannot exist.
///
/// Masking α out of the colour write is the same mechanism
/// `logic_fill_pipeline.rs:234-245` already uses for core fills under
/// the `opaque_alpha` flag ("the L1 server-α invariant"): the blend
/// equation is untouched, only the store is narrowed, so the
/// destination keeps the opaque byte its storage was initialised
/// with. This deliberately does NOT pin the fragment's α output —
/// that was L1 task A.11, and it broke because the same α is the
/// `SRC_ALPHA` blend factor (see `render.frag.glsl:306-315`).
///
/// `R8_UNORM` destinations are A8 pictures — alpha-only by
/// definition, so they always write all (one) channel.
#[must_use]
pub fn dst_color_write_mask(
    color_format: vk::Format,
    dst_has_alpha: bool,
) -> vk::ColorComponentFlags {
    if color_format != vk::Format::R8_UNORM && !dst_has_alpha {
        vk::ColorComponentFlags::R | vk::ColorComponentFlags::G | vk::ColorComponentFlags::B
    } else {
        vk::ColorComponentFlags::RGBA
    }
}

impl RenderPushConsts {
    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: `repr(C)`, plain f32 fields, no padding.
        unsafe {
            std::slice::from_raw_parts(
                std::ptr::from_ref::<Self>(self).cast::<u8>(),
                std::mem::size_of::<Self>(),
            )
        }
    }
}

/// All PictOps recognised by the pipeline cache. Numeric values
/// match the X11 RENDER protocol. The standard family (0..=12)
/// blends via fixed-function; the Disjoint family (16..=27) and
/// Conjoint family (32..=43) read the dst pixel and compute the
/// blend manually in the fragment shader (see render.frag.glsl
/// `MODE=1`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum PictOp {
    Clear = 0,
    Src = 1,
    Dst = 2,
    Over = 3,
    OverReverse = 4,
    In = 5,
    InReverse = 6,
    Out = 7,
    OutReverse = 8,
    Atop = 9,
    AtopReverse = 10,
    Xor = 11,
    Add = 12,
    Saturate = 13,
    DisjointClear = 16,
    DisjointSrc = 17,
    DisjointDst = 18,
    DisjointOver = 19,
    DisjointOverReverse = 20,
    DisjointIn = 21,
    DisjointInReverse = 22,
    DisjointOut = 23,
    DisjointOutReverse = 24,
    DisjointAtop = 25,
    DisjointAtopReverse = 26,
    DisjointXor = 27,
    ConjointClear = 32,
    ConjointSrc = 33,
    ConjointDst = 34,
    ConjointOver = 35,
    ConjointOverReverse = 36,
    ConjointIn = 37,
    ConjointInReverse = 38,
    ConjointOut = 39,
    ConjointOutReverse = 40,
    ConjointAtop = 41,
    ConjointAtopReverse = 42,
    ConjointXor = 43,
}

impl PictOp {
    pub fn from_u8(op: u8) -> Option<Self> {
        match op {
            0 => Some(PictOp::Clear),
            1 => Some(PictOp::Src),
            2 => Some(PictOp::Dst),
            3 => Some(PictOp::Over),
            4 => Some(PictOp::OverReverse),
            5 => Some(PictOp::In),
            6 => Some(PictOp::InReverse),
            7 => Some(PictOp::Out),
            8 => Some(PictOp::OutReverse),
            9 => Some(PictOp::Atop),
            10 => Some(PictOp::AtopReverse),
            11 => Some(PictOp::Xor),
            12 => Some(PictOp::Add),
            13 => Some(PictOp::Saturate),
            16 => Some(PictOp::DisjointClear),
            17 => Some(PictOp::DisjointSrc),
            18 => Some(PictOp::DisjointDst),
            19 => Some(PictOp::DisjointOver),
            20 => Some(PictOp::DisjointOverReverse),
            21 => Some(PictOp::DisjointIn),
            22 => Some(PictOp::DisjointInReverse),
            23 => Some(PictOp::DisjointOut),
            24 => Some(PictOp::DisjointOutReverse),
            25 => Some(PictOp::DisjointAtop),
            26 => Some(PictOp::DisjointAtopReverse),
            27 => Some(PictOp::DisjointXor),
            32 => Some(PictOp::ConjointClear),
            33 => Some(PictOp::ConjointSrc),
            34 => Some(PictOp::ConjointDst),
            35 => Some(PictOp::ConjointOver),
            36 => Some(PictOp::ConjointOverReverse),
            37 => Some(PictOp::ConjointIn),
            38 => Some(PictOp::ConjointInReverse),
            39 => Some(PictOp::ConjointOut),
            40 => Some(PictOp::ConjointOutReverse),
            41 => Some(PictOp::ConjointAtop),
            42 => Some(PictOp::ConjointAtopReverse),
            43 => Some(PictOp::ConjointXor),
            _ => None,
        }
    }

    /// Saturate (`min(1, (1-Ad)/As)`) and the Disjoint/Conjoint
    /// families need shader-side blend with dst readback (see
    /// `render.frag.glsl` `MODE=1`); the other standard ops blend
    /// via fixed-function pipeline state.
    pub fn needs_dst_readback(self) -> bool {
        (self as u8) >= 13
    }

    /// `(src_factor, dst_factor)` for the chosen `dst_format`,
    /// `dst_has_alpha`, and `component_alpha`. Used only for
    /// fixed-function (standard) ops; Disjoint/Conjoint and
    /// Saturate pipelines disable blending and the shader writes
    /// the final colour directly.
    ///
    /// `dst_has_alpha` distinguishes "the attachment's alpha channel
    /// stores meaningful alpha" (a8r8g8b8 / a8) from "alpha is
    /// implicit 1" (r8g8b8 / x8r8g8b8). The X RENDER spec says missing
    /// components default — alpha defaults to 1 — and the only place
    /// the blend equation cares is when a factor references
    /// `DST_ALPHA`. We substitute three different reads accordingly:
    ///   * `R8_UNORM`: alpha is stored in `.r`, so `DST_COLOR`.
    ///   * BGRA + no alpha: alpha is an implicit 1, so `ONE`.
    ///   * BGRA + alpha: actual stored alpha, so `DST_ALPHA`.
    ///
    /// `component_alpha` swaps the `SRC_ALPHA`-derived factors for
    /// the `SRC1_COLOR` family (dual-source blending). The fragment
    /// shader emits `vec4(src.a * mask.rgb, src.a * mask.a)` to
    /// output location 1 when `COMPONENT_ALPHA = 1`; the pipeline
    /// then applies each mask channel as the per-channel alpha
    /// factor — matching X RENDER's component-alpha semantics
    /// (sub-pixel ClearType-style rendering).
    ///
    /// On `R8_UNORM` attachments the fragment shader replicates the
    /// computed alpha into all channels via the `A8_DST`
    /// specialization constant; the dst attachment then stores
    /// alpha in `.r`.
    ///
    /// `pub(crate)`: the text pipeline (`text_pipeline.rs`) derives
    /// its per-op glyph blend state from this same table so the two
    /// paths agree by construction.
    pub(crate) fn blend_factors(
        self,
        dst_format: vk::Format,
        dst_has_alpha: bool,
        component_alpha: bool,
    ) -> (vk::BlendFactor, vk::BlendFactor) {
        use vk::BlendFactor as BF;
        let r8 = dst_format == vk::Format::R8_UNORM;
        let (dst_a, one_minus_dst_a) = if r8 {
            (BF::DST_COLOR, BF::ONE_MINUS_DST_COLOR)
        } else if !dst_has_alpha {
            (BF::ONE, BF::ZERO)
        } else {
            (BF::DST_ALPHA, BF::ONE_MINUS_DST_ALPHA)
        };
        let (src_a, one_minus_src_a) = if component_alpha {
            (BF::SRC1_COLOR, BF::ONE_MINUS_SRC1_COLOR)
        } else {
            (BF::SRC_ALPHA, BF::ONE_MINUS_SRC_ALPHA)
        };
        match self {
            PictOp::Clear => (BF::ZERO, BF::ZERO),
            PictOp::Src => (BF::ONE, BF::ZERO),
            PictOp::Dst => (BF::ZERO, BF::ONE),
            PictOp::Over => (BF::ONE, one_minus_src_a),
            PictOp::OverReverse => (one_minus_dst_a, BF::ONE),
            PictOp::In => (dst_a, BF::ZERO),
            PictOp::InReverse => (BF::ZERO, src_a),
            PictOp::Out => (one_minus_dst_a, BF::ZERO),
            PictOp::OutReverse => (BF::ZERO, one_minus_src_a),
            PictOp::Atop => (dst_a, one_minus_src_a),
            PictOp::AtopReverse => (one_minus_dst_a, src_a),
            PictOp::Xor => (one_minus_dst_a, one_minus_src_a),
            PictOp::Add => (BF::ONE, BF::ONE),
            // Saturate / Disjoint / Conjoint never reach the
            // fixed-function path; their pipelines disable blending.
            // Returning ZERO/ZERO is a safety net.
            _ => (BF::ZERO, BF::ZERO),
        }
    }
}

/// Backwards-compatible alias for the old name.
pub use PictOp as StdPictOp;

/// Cache + sampler + layouts for the per-op-and-format pipeline
/// family. Built lazily on demand; reused for the rest of the
/// session. Pipelines key on `(op, dst_format)` because the
/// per-op blend factors depend on whether the dst is BGRA8 or
/// R8 (a8 picture).
pub struct RenderPipelineCache {
    vk: Arc<VkContext>,
    pipeline_layout: vk::PipelineLayout,
    descriptor_set_layout: vk::DescriptorSetLayout,
    sampler: vk::Sampler,
    /// Compiled pipelines, keyed by `(op, dst_format, dst_has_alpha,
    /// component_alpha)`. `dst_has_alpha` and `component_alpha` flip
    /// the per-op blend factor table — see [`PictOp::blend_factors`].
    /// Built on first use of each combination.
    pipelines: HashMap<(PictOp, vk::Format, bool, bool), vk::Pipeline>,
}

#[derive(Debug, thiserror::Error)]
pub enum RenderPipelineError {
    #[error("vulkan: {0:?}")]
    Vk(vk::Result),
    #[error(
        "render shader SPIR-V from build.rs is malformed (length not multiple of 4): {0} bytes"
    )]
    SpirvUnaligned(usize),
}

impl From<vk::Result> for RenderPipelineError {
    fn from(r: vk::Result) -> Self {
        RenderPipelineError::Vk(r)
    }
}

impl RenderPipelineCache {
    pub fn new(vk: Arc<VkContext>) -> Result<Self, RenderPipelineError> {
        let device = &vk.device;

        // X RENDER's default picture filter is `Nearest` — pixman
        // matches this. rendercheck's tscoords/tmcoords transform
        // tests assume the default and compare against integer-pixel
        // mapping; LINEAR would bilerp adjacent texels and miss the
        // exact match. Pictures that explicitly set Filter=Bilinear
        // (Cairo, modern toolkits) will need a separate sampler
        // selected at descriptor-bind time once we honour
        // `RenderSetPictureFilter` — for now both rendercheck's
        // gradients and standard composites pass with NEAREST.
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
            vk::DescriptorSetLayoutBinding::default()
                .binding(2)
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
                    return Err(e.into());
                }
            };

        let set_layouts = [descriptor_set_layout];
        let push_const_ranges = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
            .offset(0)
            .size(std::mem::size_of::<RenderPushConsts>() as u32)];
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

        Ok(Self {
            vk,
            pipeline_layout,
            descriptor_set_layout,
            sampler,
            pipelines: HashMap::new(),
        })
    }

    pub fn pipeline_layout(&self) -> vk::PipelineLayout {
        self.pipeline_layout
    }

    /// Look up or lazily build the pipeline for `(op, dst_format,
    /// dst_has_alpha, component_alpha)`. Returns the raw handle;
    /// pipeline is cached for the rest of the session.
    pub fn get(
        &mut self,
        op: PictOp,
        dst_format: vk::Format,
        dst_has_alpha: bool,
        component_alpha: bool,
    ) -> Result<vk::Pipeline, RenderPipelineError> {
        // `dualSrcBlend` is an *optional* Vulkan feature, absent on some
        // tilers (Broadcom V3D / v3dv on the RPi 4/400). Without it the
        // SRC1_* blend factors the component-alpha path relies on are
        // unavailable, so clamp component-alpha off and build the
        // standard single-source pipeline: the mask is applied through
        // its alpha channel (the well-exercised non-component-alpha
        // path), degrading subpixel/LCD AA to grayscale AA. Clamping
        // here — the single pipeline choke point — keeps the cache key,
        // the fragment shader's COMPONENT_ALPHA spec constant, and the
        // blend factors consistent.
        let component_alpha = component_alpha && self.vk.component_alpha_supported;
        let key = (op, dst_format, dst_has_alpha, component_alpha);
        if let Some(p) = self.pipelines.get(&key) {
            return Ok(*p);
        }
        let p = build_pipeline(
            &self.vk,
            self.pipeline_layout,
            op,
            dst_format,
            dst_has_alpha,
            component_alpha,
        )?;
        self.pipelines.insert(key, p);
        Ok(p)
    }

    /// Stage 5 Task 4 layer 1: ring-backed descriptor allocation.
    /// The arena-backed sibling went away with v1 (2026-05-26); v2
    /// uses this exclusively. Allocates the descriptor
    /// set from the engine's long-lived `DescriptorPoolRing` (whose
    /// pools recycle on retirement) and writes the three view
    /// bindings via the shared `write_views_into_descriptor_set`
    /// helper. v2's engine call sites call this method; v1 keeps
    /// using the per-batch arena variant. Spec
    /// `2026-05-21-descriptor-pool-ring-design.md`.
    pub(crate) fn allocate_descriptor_for_views_into_ring(
        &self,
        ring: &mut crate::kms::render::descriptor_pool_ring::DescriptorPoolRing,
        generation: u64,
        src_view: vk::ImageView,
        mask_view: vk::ImageView,
        dst_view: vk::ImageView,
    ) -> Result<vk::DescriptorSet, vk::Result> {
        let set = ring.acquire_set(self.descriptor_set_layout, generation)?;
        write_views_into_descriptor_set(
            self.vk.as_ref(),
            set,
            self.sampler,
            src_view,
            mask_view,
            dst_view,
        );
        Ok(set)
    }
}

/// Used by `allocate_descriptor_for_views_into_ring`. Takes a
/// pre-allocated `vk::DescriptorSet` and writes the
/// three `COMBINED_IMAGE_SAMPLER` bindings (src=0, mask=1, dst=2)
/// with the shared linear sampler.
///
/// Note: parameter is named `vk_ctx`, NOT `vk`, because `vk` is the
/// `ash::vk` module alias in this file — using `vk` as a parameter
/// name shadows the module and breaks every `vk::DescriptorImageInfo`
/// reference inside the function body.
fn write_views_into_descriptor_set(
    vk_ctx: &VkContext,
    set: vk::DescriptorSet,
    sampler: vk::Sampler,
    src_view: vk::ImageView,
    mask_view: vk::ImageView,
    dst_view: vk::ImageView,
) {
    let src_info = [vk::DescriptorImageInfo::default()
        .image_view(src_view)
        .sampler(sampler)
        .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
    let mask_info = [vk::DescriptorImageInfo::default()
        .image_view(mask_view)
        .sampler(sampler)
        .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
    let dst_info = [vk::DescriptorImageInfo::default()
        .image_view(dst_view)
        .sampler(sampler)
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
        vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(2)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .image_info(&dst_info),
    ];
    unsafe { vk_ctx.device.update_descriptor_sets(&writes, &[]) };
}

impl Drop for RenderPipelineCache {
    fn drop(&mut self) {
        unsafe {
            let _ = self.vk.device.queue_wait_idle(self.vk.graphics_queue);
            for &p in self.pipelines.values() {
                self.vk.device.destroy_pipeline(p, None);
            }
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

/// 1×1 BGRA8 image used as a sampled source for `SolidFill`
/// pictures (and for `render_fill_rectangles`'s solid colour).
/// `cmd_clear_color_image` rewrites it to the request's colour
/// inside each composite CB before sampling. Persistent across
/// the backend's lifetime; one allocation per backend, not one
/// per Composite request.
pub struct SolidColorImage {
    vk: Arc<VkContext>,
    image: vk::Image,
    view: vk::ImageView,
    memory: vk::DeviceMemory,
    /// Tracks the layout the image is in. Initialised `UNDEFINED`;
    /// flips to `SHADER_READ_ONLY_OPTIMAL` after the first clear+
    /// sample sequence and stays there.
    current_layout: vk::ImageLayout,
}

impl SolidColorImage {
    pub fn new(vk: Arc<VkContext>) -> Result<Self, RenderPipelineError> {
        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::B8G8R8A8_UNORM)
            .extent(vk::Extent3D {
                width: 1,
                height: 1,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image = unsafe { vk.device.create_image(&image_info, None)? };

        let mem_reqs = unsafe { vk.device.get_image_memory_requirements(image) };
        let mem_props = unsafe {
            vk.instance
                .get_physical_device_memory_properties(vk.physical_device)
        };
        let mt_index = (0..mem_props.memory_type_count).find(|&i| {
            mem_reqs.memory_type_bits & (1 << i) != 0
                && mem_props.memory_types[i as usize]
                    .property_flags
                    .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
        });
        let mt_index = match mt_index {
            Some(i) => i,
            None => {
                unsafe { vk.device.destroy_image(image, None) };
                return Err(RenderPipelineError::Vk(
                    vk::Result::ERROR_FEATURE_NOT_PRESENT,
                ));
            }
        };
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_reqs.size)
            .memory_type_index(mt_index)
            .push_next(&mut dedicated);
        let memory = match crate::kms::vk::mem_accounting::allocate_memory(
            &vk.device,
            &alloc_info,
            crate::kms::vk::mem_accounting::MemCategory::Other,
            &mem_props,
        ) {
            Ok(m) => m,
            Err(e) => {
                unsafe { vk.device.destroy_image(image, None) };
                return Err(e.into());
            }
        };
        if let Err(e) = unsafe { vk.device.bind_image_memory(image, memory, 0) } {
            unsafe {
                crate::kms::vk::mem_accounting::free_memory(&vk.device, memory);
                vk.device.destroy_image(image, None);
            }
            return Err(e.into());
        }
        let view_info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(vk::Format::B8G8R8A8_UNORM)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            );
        let view = match unsafe { vk.device.create_image_view(&view_info, None) } {
            Ok(v) => v,
            Err(e) => {
                unsafe {
                    crate::kms::vk::mem_accounting::free_memory(&vk.device, memory);
                    vk.device.destroy_image(image, None);
                }
                return Err(e.into());
            }
        };
        Ok(Self {
            vk,
            image,
            view,
            memory,
            current_layout: vk::ImageLayout::UNDEFINED,
        })
    }

    pub fn image(&self) -> vk::Image {
        self.image
    }

    pub fn image_view(&self) -> vk::ImageView {
        self.view
    }

    pub fn current_layout(&self) -> vk::ImageLayout {
        self.current_layout
    }

    pub fn set_current_layout(&mut self, layout: vk::ImageLayout) {
        self.current_layout = layout;
    }
}

impl Drop for SolidColorImage {
    fn drop(&mut self) {
        unsafe {
            let _ = self.vk.device.queue_wait_idle(self.vk.graphics_queue);
            self.vk.device.destroy_image_view(self.view, None);
            self.vk.device.destroy_image(self.image, None);
            crate::kms::vk::mem_accounting::free_memory(&self.vk.device, self.memory);
        }
    }
}

/// Record the barrier-clear-barrier sequence that loads `color`
/// into `solid` for sampling. After this returns the image is in
/// `SHADER_READ_ONLY_OPTIMAL` (and `solid.current_layout` reflects
/// that).
pub fn record_solid_color_clear(
    vk: &VkContext,
    cb: vk::CommandBuffer,
    solid: &mut SolidColorImage,
    color: [f32; 4],
) {
    let device = &vk.device;
    let old = solid.current_layout();

    let to_dst = [vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
        .src_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
        .dst_stage_mask(vk::PipelineStageFlags2::CLEAR)
        .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
        .old_layout(old)
        .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
        .image(solid.image())
        .subresource_range(color_subresource_range())];
    let dep = vk::DependencyInfo::default().image_memory_barriers(&to_dst);
    crate::vk_count!(cmd_pipeline_barrier2);
    unsafe { device.cmd_pipeline_barrier2(cb, &dep) };

    let clear_color = vk::ClearColorValue { float32: color };
    let ranges = [color_subresource_range()];
    unsafe {
        crate::vk_count!(cmd_clear_color_image);
        device.cmd_clear_color_image(
            cb,
            solid.image(),
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &clear_color,
            &ranges,
        );
    }

    let to_read = [vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::CLEAR)
        .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
        .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
        .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
        .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
        .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
        .image(solid.image())
        .subresource_range(color_subresource_range())];
    let dep = vk::DependencyInfo::default().image_memory_barriers(&to_read);
    crate::vk_count!(cmd_pipeline_barrier2);
    unsafe { device.cmd_pipeline_barrier2(cb, &dep) };
    solid.set_current_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
}

fn color_subresource_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1)
}

fn build_pipeline(
    vk: &VkContext,
    pipeline_layout: vk::PipelineLayout,
    op: PictOp,
    color_format: vk::Format,
    dst_has_alpha: bool,
    component_alpha: bool,
) -> Result<vk::Pipeline, RenderPipelineError> {
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
    // Fragment-shader specialization constants:
    //   id 0: MODE — 0 = standard fixed-function blend, 1 = manual
    //         shader-side blend (Disjoint/Conjoint).
    //   id 1: OP — wire op code (0..43); only consulted in MODE=1.
    //   id 2: A8_DST — 1 → replicate computed alpha across all
    //         channels so an R8 attachment receives alpha in `.r`.
    //   id 3: COMPONENT_ALPHA — 1 → emit a second fragment output
    //         (location 1) carrying per-channel src.a * mask.rgb so
    //         the pipeline's SRC1_* blend factors apply each mask
    //         channel as an independent alpha factor.
    let needs_dst_readback = op.needs_dst_readback();
    let mode: u32 = u32::from(needs_dst_readback);
    let op_code: u32 = u32::from(op as u8);
    let a8_dst: u32 = u32::from(color_format == vk::Format::R8_UNORM);
    let comp_alpha: u32 = u32::from(component_alpha);
    // L1 task A.11: depth-24 BGRA destinations are server-owned α —
    // the frag stage must emit α = 1.0 so the per-op blend lands an
    // opaque byte regardless of `XRenderColor.a`. Depth-32 ARGB and
    // R8 (alpha-only) destinations pass α through unchanged.
    let alpha_mode: u32 = u32::from(color_format != vk::Format::R8_UNORM && !dst_has_alpha);
    let mut spec_data = [0u8; 20];
    spec_data[0..4].copy_from_slice(&mode.to_ne_bytes());
    spec_data[4..8].copy_from_slice(&op_code.to_ne_bytes());
    spec_data[8..12].copy_from_slice(&a8_dst.to_ne_bytes());
    spec_data[12..16].copy_from_slice(&comp_alpha.to_ne_bytes());
    spec_data[16..20].copy_from_slice(&alpha_mode.to_ne_bytes());
    let spec_map_entries = [
        vk::SpecializationMapEntry::default()
            .constant_id(0)
            .offset(0)
            .size(4),
        vk::SpecializationMapEntry::default()
            .constant_id(1)
            .offset(4)
            .size(4),
        vk::SpecializationMapEntry::default()
            .constant_id(2)
            .offset(8)
            .size(4),
        vk::SpecializationMapEntry::default()
            .constant_id(3)
            .offset(12)
            .size(4),
        vk::SpecializationMapEntry::default()
            .constant_id(4)
            .offset(16)
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

    // Disjoint/Conjoint pipelines compute the blend in the shader
    // and write the final premultiplied colour directly — disable
    // fixed-function blending. Standard ops use the per-op factor
    // table.
    // Destination-side force-opaque: a depth-24 / xRGB destination
    // has no client-meaningful α, so the composite must not store one.
    // See `dst_color_write_mask`.
    let write_mask = dst_color_write_mask(color_format, dst_has_alpha);
    let color_blend_attachments = if needs_dst_readback {
        [vk::PipelineColorBlendAttachmentState::default()
            .blend_enable(false)
            .color_write_mask(write_mask)]
    } else {
        let (src_factor, dst_factor) =
            op.blend_factors(color_format, dst_has_alpha, component_alpha);
        [vk::PipelineColorBlendAttachmentState::default()
            .blend_enable(true)
            .src_color_blend_factor(src_factor)
            .dst_color_blend_factor(dst_factor)
            .color_blend_op(vk::BlendOp::ADD)
            .src_alpha_blend_factor(src_factor)
            .dst_alpha_blend_factor(dst_factor)
            .alpha_blend_op(vk::BlendOp::ADD)
            .color_write_mask(write_mask)]
    };
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
) -> Result<vk::ShaderModule, RenderPipelineError> {
    if !spv_bytes.len().is_multiple_of(4) {
        return Err(RenderPipelineError::SpirvUnaligned(spv_bytes.len()));
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
    use super::{PictOp, dst_color_write_mask};
    use ash::vk;

    fn rgb() -> vk::ColorComponentFlags {
        vk::ColorComponentFlags::R | vk::ColorComponentFlags::G | vk::ColorComponentFlags::B
    }

    /// The destination half of the X11 "a depth-24 drawable has no
    /// alpha channel" rule.
    ///
    /// Oracle is Xorg, not our own encoder: pixman's `x8r8g8b8` has
    /// zero alpha bits, so a store into it drops the channel outright
    /// (`pixman-access.c:254-261` — `convert_channel` with
    /// `n_to_bits == 0`), and RENDER's `ReduceCompositeOp` says the
    /// destination alpha "is always 1" whenever
    /// `PICT_FORMAT_A(pDst->format) == 0` (`render/picture.c:1487`).
    /// Our BGRA8 storage has a real alpha byte, so the equivalent is
    /// to never write it — otherwise `PictOpSrc` from a half-
    /// transparent source leaves `α = 127` in a depth-24 backing
    /// (the measured mate-terminal frame failure).
    #[test]
    fn a_no_alpha_destination_masks_alpha_out_of_the_colour_write() {
        assert_eq!(
            dst_color_write_mask(vk::Format::B8G8R8A8_UNORM, false),
            rgb(),
            "depth-24 / xRGB destination must not store an alpha byte",
        );
        assert_eq!(
            dst_color_write_mask(vk::Format::B8G8R8A8_UNORM, true),
            vk::ColorComponentFlags::RGBA,
            "ARGB32 destination keeps its client-meaningful alpha",
        );
        assert_eq!(
            dst_color_write_mask(vk::Format::R8_UNORM, true),
            vk::ColorComponentFlags::RGBA,
            "an A8 picture is alpha-only; its single channel is always written",
        );
        // Defensive: an R8 attachment can never be "no alpha" (see
        // `dst_has_alpha_for_pict_format`), but if it somehow were,
        // masking its only channel would drop the whole draw.
        assert_eq!(
            dst_color_write_mask(vk::Format::R8_UNORM, false),
            vk::ColorComponentFlags::RGBA,
            "R8 must never have its only channel masked out",
        );
    }

    /// The *read* half of the same rule was already correct and must
    /// stay that way — pinned here so the write-mask change above is
    /// not mistaken for the whole invariant. A no-alpha destination
    /// substitutes `ONE` / `ZERO` for the `DST_ALPHA` factor pair,
    /// which is pixman's `def_value = ~0` alpha fetch
    /// (`pixman-access.c:270-276`) expressed as blend state.
    #[test]
    fn a_no_alpha_destination_reads_dst_alpha_as_one() {
        let bgra = vk::Format::B8G8R8A8_UNORM;
        // In = (Ad, 0): with Ad == 1 the src factor is ONE.
        assert_eq!(
            PictOp::In.blend_factors(bgra, false, false),
            (vk::BlendFactor::ONE, vk::BlendFactor::ZERO),
            "PictOpIn over an opaque-by-definition dst is PictOpSrc",
        );
        // OverReverse = (1 - Ad, 1): with Ad == 1 the src factor is ZERO.
        assert_eq!(
            PictOp::OverReverse.blend_factors(bgra, false, false),
            (vk::BlendFactor::ZERO, vk::BlendFactor::ONE),
            "PictOpOverReverse over an opaque-by-definition dst is PictOpDst",
        );
        // Same ops against a real ARGB32 dst keep the DST_ALPHA reads.
        assert_eq!(
            PictOp::In.blend_factors(bgra, true, false),
            (vk::BlendFactor::DST_ALPHA, vk::BlendFactor::ZERO),
        );
        assert_eq!(
            PictOp::OverReverse.blend_factors(bgra, true, false),
            (vk::BlendFactor::ONE_MINUS_DST_ALPHA, vk::BlendFactor::ONE),
        );
    }
}
