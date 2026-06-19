//! `RenderEngine` — drawing primitives into [`DrawableStore`] storage.
//!
//! Stage 2c lands the three single-drawable paint ops the v2 model
//! needs for offscreen pixel-correctness gates:
//! [`fill_rect`](RenderEngine::fill_rect),
//! [`put_image`](RenderEngine::put_image),
//! [`get_image`](RenderEngine::get_image). Each op is a self-
//! contained `vkQueueSubmit2` against a fresh [`FenceTicket`] from
//! [`PlatformBackend`]. The ticket is recorded on every drawable
//! the op touched (cross-cutting §5) so a later compose-read can
//! see the in-flight write; and parked on
//! [`RenderEngine::submitted`] for retirement via
//! [`poll_retired`](RenderEngine::poll_retired).
//!
//! What's deliberately NOT in 2c (per the Stage 2 plan):
//!
//! - `copy_area` (joins 2d alongside scene/blit).
//! - RENDER / glyphs / text / poly_line / poly_segment / etc.
//!   Logged-gap on `KmsBackendV2` until Stage 3.
//! - Per-op batching across multiple ops. 2c uses one
//!   submission per Backend method call — equivalent perf-wise to
//!   v1's per-op shape; submit-aggregation arrives in Stage 5.
//! - `vkQueueWaitIdle` anywhere. Only `get_image` waits on its own
//!   `FenceTicket` (off the hot path; sync RPC by protocol design).
//! - GC `function != GXcopy` and non-zero `planemask`. Stage 2 plan
//!   §"What doesn't ship in Stage 2": v2 logs a gap + drops the op.
//!   These come back in Stage 3 alongside RENDER.
//!
//! Layout discipline: every paint op brackets its work with two
//! [`Drawable::record_layout_transition`] calls so the storage is
//! returned to `SHADER_READ_ONLY_OPTIMAL` for the next consumer
//! (compose-read in 2d, another paint op in 2c).

#![allow(
    dead_code,
    reason = "RenderEngine primitives are consumed by Stages 2d–2f"
)]

use std::{
    collections::{HashMap, HashSet, VecDeque},
    ptr::NonNull,
    sync::Arc,
};

use ash::vk;

use super::{
    glyph_atlas::V2GlyphAtlas,
    platform::{FenceTicket, PlatformBackend, PresentCompletionSignal},
    present_completion::{PendingPresentBatch, PendingPresentEntry, PresentBatchWait},
    store::{DrawableId, DrawableStore, RetiredImage},
};
use crate::kms::{
    cpu_types::{PictTransform, Rectangle16, Repeat},
    vk::{
        device::VkContext,
        dst_readback::DstReadback,
        glyph::{AtlasEntry, GlyphKey},
        ops::{render::CompositeTarget, text::TextRunTarget},
        render_pipeline::{RenderPipelineCache, SolidColorImage},
        text_pipeline::TextPipeline,
    },
};

// ────────────────────────────────────────────────────────────────
// Errors
// ────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub(crate) enum RenderError {
    #[error("vk: {0:?}")]
    Vk(vk::Result),
    #[error("drawable {0:?} not present in store")]
    UnknownDrawable(DrawableId),
    #[error("renderer not initialised (no VkContext)")]
    NoVk,
    #[error("renderer in failed state — refusing further ops")]
    RendererFailed,
    #[error("unsupported depth {0} for v2 Stage 2c ops")]
    UnsupportedDepth(u8),
    #[error("source byte slice too short for {expected} bytes")]
    TruncatedSource { expected: usize },
}

impl From<vk::Result> for RenderError {
    fn from(r: vk::Result) -> Self {
        RenderError::Vk(r)
    }
}

// ────────────────────────────────────────────────────────────────
// SubmittedOp — one in-flight CB awaiting fence retirement.
//
// Holds onto the resources whose destruction must wait for the
// I6a fence: the CB itself + any per-op staging buffer the op
// allocated. On `poll_retired`, signaled entries are destroyed.
// ────────────────────────────────────────────────────────────────

/// Stage 5 Task 3 POC: pending coalescing batch for `copy_area`
/// ops whose destination is the COMPOSITE Overlay Window. The
/// hot pattern (silence trace 2026-05-22: 47k of 62k copy_areas)
/// is marco issuing `XCopyArea(backing, COW, …)` per visible
/// window per frame, producing runs of 12-50 back-to-back
/// submits against one dst. Coalescing collapses each run into
/// one CB + one `vkQueueSubmit2` while preserving every
/// individual `vkCmdCopyImage`.
///
/// Lifecycle:
/// - First `cow_copy_area` allocates `cb` + `ticket`, transitions
///   `dst` → `TRANSFER_DST_OPTIMAL`, transitions each new `src`
///   → `TRANSFER_SRC_OPTIMAL` once on first appearance, records
///   `vkCmdCopyImage`, accumulates dst damage.
/// - Subsequent appends record only `vkCmdCopyImage` (and a new
///   src transition if the src hasn't appeared in this batch).
///
/// Stage 5 Task 3 (render-composite generalization): conservative
/// aggregation key. Two consecutive `render_composite` calls
/// coalesce into one CB iff every field of their keys is equal.
/// The predicate deliberately excludes Solid / Gradient sources
/// and ops needing dst readback, so the existing
/// `record_solid_color_clear` + `dst_readback` paths inside a
/// render pass don't have to change.
/// Fields chosen for what affects pipeline binding + render-pass
/// attachments (must match across the batch). Per-append data
/// — `clip_rects`, `src_transform`, `mask_transform`, src/mask
/// id, src/mask repeat, src/mask pict_format — is NOT in the
/// key because each append builds its own descriptor set and
/// `record_render_composite_draws` re-encodes scissor + push
/// constants per-call. Crucially this means N different srcs
/// painting onto one dst all coalesce into one CB (marco's
/// dominant compositor-pump pattern).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RenderBatchKey {
    dst: DrawableId,
    /// Drives pipeline selection (with `dst_pict_format` +
    /// `mask_component_alpha`). Distinct ops can't share a
    /// `cmd_bind_pipeline`-once batch.
    op: u8,
    /// Drives pipeline `dst_has_alpha`.
    dst_pict_format: u32,
    /// Drives pipeline `mask_component_alpha`.
    mask_component_alpha: bool,
}

/// Pending RENDER composite batch: long-lived CB across N appends,
/// exit transitions + submit at flush. `cmd_begin_rendering` is
/// active across the whole batch (one pair per flush) and the
/// pipeline + descriptor set bound once at batch start serve
/// every append.
struct PendingRenderBatch {
    cb: vk::CommandBuffer,
    ticket: FenceTicket,
    key: RenderBatchKey,
    /// All accumulated dst-relative damage rects for the batch
    /// (one per CompositeRect per append); applied on flush.
    dst_damage: Vec<vk::Rect2D>,
    /// Every drawable id this batch sampled (src + mask across
    /// all appends). Used at flush to clone the fence ticket onto
    /// every touched drawable. Dst is tracked separately via
    /// `key.dst`.
    touched_drawables: HashSet<DrawableId>,
    /// True if at least one append in this batch carried a mask.
    /// Reported on the flush record for trace-event mask_class.
    any_mask: bool,
    /// Number of `vkCmdDraw` calls recorded so far (rects ×
    /// clip-scissors across all appends). Returned in
    /// `CompositeStats.recorded_draws` for the LAST appending
    /// call so the backend still has a non-zero signal where
    /// appropriate (zero would suppress the wake-for-damage in
    /// some callers).
    accumulated_draws: u32,
    /// Number of protocol-level `render_composite` calls folded
    /// into the batch. Reported via the flush record for
    /// telemetry + submit-trace.
    coalesced_count: u32,
}

/// One flush record per `render_batch` flush. Carries enough
/// info for the backend drain to emit a parametrised submit
/// trace event (op + src class + mask class + batch_size).
#[derive(Debug, Clone, Copy)]
pub(crate) struct RenderFlushRecord {
    pub(crate) dst: DrawableId,
    pub(crate) op: u8,
    /// `true` if the mask was a Drawable (vs `None`).
    pub(crate) has_mask: bool,
    pub(crate) coalesced_count: u32,
}

struct SubmittedOp {
    cb: vk::CommandBuffer,
    ticket: FenceTicket,
    /// Per-op staging buffer (only for `put_image` and Stage 3a
    /// glyph upload). Destroyed only after the fence signals;
    /// dropping it earlier would race the GPU's TRANSFER_READ.
    staging: Option<Arc<StagingBuffer>>,
    /// Phase B.3 (N8): per-op self-overlap scratch images. Renamed from
    /// `Option<ScratchImage>` to `Vec<ScratchImage>` so the frame builder
    /// close-path walk over `open_frame.ops` can `std::mem::take` every
    /// `RecordedCopyArea::self_overlap_scratch` into one batch's
    /// `SubmittedOp`. Legacy `copy_area` self-overlap path (engine.rs:2937)
    /// transiently pushes a single-element Vec until that body is rewritten
    /// in Task 2.
    scratch: Vec<ScratchImage>,
    /// Stage 3a: cloned `atlas_last_upload_ticket` snapshot.
    /// Atlas-sampling ops (text runs, RENDER glyphs in Stage 3d)
    /// stash the engine's then-current upload ticket here so the
    /// atlas image + the upload's staging buffer can't retire
    /// before the consume CB has executed. Same-queue submission
    /// order is the GPU dependency; this Arc keeps CPU-side
    /// destruction gated on retirement of both submissions.
    atlas_ticket: Option<FenceTicket>,
    /// Stage 5 Task 4 layer 1: monotonic acquire-generation stamp.
    /// `release_retired_ops` calls
    /// `descriptor_pool_ring.release_up_to(op.generation)` once this
    /// op pops from the FIFO; pools whose `high_water_generation
    /// <= op.generation` move back to Free. Spec
    /// `2026-05-21-descriptor-pool-ring-design.md`.
    generation: u64,
    /// Phase B.2 Mechanism 3: retired scratch `BatchResource`s
    /// attached to this op via
    /// `RenderEngineInner::adopt_retired_resource_for_gpu_retirement`
    /// case (b) — the newest in-flight fence owner. Drained and
    /// released (via explicit `BatchResource::release(&vk)`, NOT
    /// `Drop`) at retirement in `poll_retired` / `drain_all`.
    ///
    /// Parallel to the concrete `scratch: Option<ScratchImage>`
    /// slot above. Empty for ops that did not adopt a retired
    /// resource — which is the common case under B.2 (`ensure_*_old`
    /// returns `Ok(None)` when no grow fires).
    retired_resources: Vec<Box<dyn crate::kms::v2::batch_resource::BatchResource>>,
}

impl SubmittedOp {
    /// Phase B.2 Mechanism 3 helper: attach a retired
    /// `BatchResource` to this op. Called via
    /// `RenderEngineInner::adopt_retired_resource_for_gpu_retirement`
    /// case (b) when `submitted.back` is the newest fence owner.
    #[allow(
        dead_code,
        reason = "B.2 Task 1: case (b) of adopt_retired_resource_for_gpu_retirement \
                  feeds this. The helper is wired in this commit; the first call \
                  site from a real grow event lands once the _legacy paths route \
                  their ensure_returning_old returns through the engine helper."
    )]
    fn append_retired_scratch(
        &mut self,
        boxed: Box<dyn crate::kms::v2::batch_resource::BatchResource>,
    ) {
        self.retired_resources.push(boxed);
    }

    /// Phase B.2 Mechanism 3 helper: drain the per-op retired
    /// `BatchResource`s for release at retirement. Caller calls
    /// `release(&vk)` per Box.
    fn drain_retired_scratch(
        &mut self,
    ) -> std::vec::Drain<'_, Box<dyn crate::kms::v2::batch_resource::BatchResource>> {
        self.retired_resources.drain(..)
    }
}

/// One-shot device-local image used by `copy_area`'s same-image
/// overlap path (Stage 2d). Destroyed only after the owning op's
/// fence signals.
pub(crate) struct ScratchImage {
    vk: Arc<VkContext>,
    image: vk::Image,
    memory: vk::DeviceMemory,
    /// Bytes allocated for this image (from `mem_reqs.size`). Used
    /// by `active_resource_bytes` to account for active scratch
    /// memory without querying the driver.
    size_bytes: u64,
}

impl std::fmt::Debug for ScratchImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScratchImage")
            .field("size_bytes", &self.size_bytes)
            .finish_non_exhaustive()
    }
}

impl ScratchImage {
    fn size_bytes(&self) -> u64 {
        self.size_bytes
    }
}

impl Drop for ScratchImage {
    fn drop(&mut self) {
        unsafe {
            self.vk.device.destroy_image(self.image, None);
            self.vk.device.free_memory(self.memory, None);
        }
    }
}

/// One-shot host-visible buffer used for `put_image` upload or
/// `get_image` readback. Destroyed on drop.
pub(crate) struct StagingBuffer {
    vk: Arc<VkContext>,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    mapped: NonNull<u8>,
    size: u64,
}

impl std::fmt::Debug for StagingBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StagingBuffer")
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

// SAFETY: the v2 backend's single-threaded core invariant keeps
// `StagingBuffer` pinned to the backend thread; `NonNull<u8>` is
// only sound to Send/Sync under that invariant. Sync is additionally
// required so Arc<StagingBuffer> satisfies Send (Arc<T>: Send requires
// T: Send + Sync). Shared access is never exercised in practice — all
// callers hold either a unique `Arc` or have already retired the op.
unsafe impl Send for StagingBuffer {}
unsafe impl Sync for StagingBuffer {}

impl StagingBuffer {
    fn new(vk: Arc<VkContext>, size: u64) -> Result<Self, vk::Result> {
        Self::new_with_usage(
            vk,
            size,
            vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST,
        )
    }

    /// Stage 3e.2: variant with explicit usage flags. The trap
    /// path needs `VERTEX_BUFFER` usage on its instance-data
    /// upload buffer (cmd_bind_vertex_buffers requires that bit).
    fn new_with_usage(
        vk: Arc<VkContext>,
        size: u64,
        usage: vk::BufferUsageFlags,
    ) -> Result<Self, vk::Result> {
        let buf_info = vk::BufferCreateInfo::default()
            .size(size)
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let buffer = unsafe { vk.device.create_buffer(&buf_info, None)? };
        let mem_reqs = unsafe { vk.device.get_buffer_memory_requirements(buffer) };
        let mem_props = unsafe {
            vk.instance
                .get_physical_device_memory_properties(vk.physical_device)
        };
        let want = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
        let Some(mt) = (0..mem_props.memory_type_count).find(|&i| {
            mem_reqs.memory_type_bits & (1 << i) != 0
                && mem_props.memory_types[i as usize]
                    .property_flags
                    .contains(want)
        }) else {
            unsafe { vk.device.destroy_buffer(buffer, None) };
            return Err(vk::Result::ERROR_FEATURE_NOT_PRESENT);
        };
        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_reqs.size)
            .memory_type_index(mt);
        let memory = match unsafe { vk.device.allocate_memory(&alloc_info, None) } {
            Ok(m) => m,
            Err(e) => {
                unsafe { vk.device.destroy_buffer(buffer, None) };
                return Err(e);
            }
        };
        if let Err(e) = unsafe { vk.device.bind_buffer_memory(buffer, memory, 0) } {
            unsafe {
                vk.device.free_memory(memory, None);
                vk.device.destroy_buffer(buffer, None);
            }
            return Err(e);
        }
        let mapped_raw = match unsafe {
            vk.device
                .map_memory(memory, 0, size, vk::MemoryMapFlags::empty())
        } {
            Ok(p) => p,
            Err(e) => {
                unsafe {
                    vk.device.free_memory(memory, None);
                    vk.device.destroy_buffer(buffer, None);
                }
                return Err(e);
            }
        };
        let mapped = NonNull::new(mapped_raw.cast::<u8>()).expect("vkMapMemory non-null");
        Ok(Self {
            vk,
            buffer,
            memory,
            mapped,
            size,
        })
    }
}

impl Drop for StagingBuffer {
    fn drop(&mut self) {
        unsafe {
            self.vk.device.unmap_memory(self.memory);
            self.vk.device.destroy_buffer(self.buffer, None);
            self.vk.device.free_memory(self.memory, None);
        }
    }
}

// ────────────────────────────────────────────────────────────────
// Stage 3c: drawable view cache (plan §1).
//
// A Drawable can be sampled in three roles (source / mask /
// alpha-only) with different sampler + swizzle bindings. Keying
// the cache on `DrawableId` alone would over-share; keying on
// `(DrawableId, SamplerConfig, SwizzleClass)` gives the same
// `Drawable` a separate cached view per role. Eviction is driven
// by drawable retirement (see `Drawable` lifecycle in
// `DrawableStore`); no LRU.
// ────────────────────────────────────────────────────────────────

/// Sampler configuration the cache key cares about. Filter is
/// `Nearest` only in Stage 3 (per spec § "Out of scope"); the
/// address mode mirrors the four X RENDER `Repeat` values.
#[allow(
    dead_code,
    reason = "Variants are populated by Stage 3c's render_composite path"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SamplerConfig {
    /// `Repeat::None` — clamp-to-border at picture edges (or
    /// `REPEAT_PAD` for synthetic 1×1 sources; see render plan §3c).
    Clamp,
    /// `Repeat::Normal` — wrap.
    Repeat,
    /// `Repeat::Pad` — clamp-to-edge.
    Pad,
    /// `Repeat::Reflect` — mirrored repeat.
    Reflect,
}

/// Swizzle bucket for the cached view. Distinguishes the three
/// formats v2 supports for RENDER sources / masks (per plan §3b
/// `RenderEngine adds`):
///
/// - `RgbaIdent` — depth-32 BGRA picture: regular `(b, g, r, a)`
///   sample.
/// - `AlphaOnlyR8` — R8 storage sampled as an alpha mask;
///   swizzle `(0, 0, 0, R)` so the shader's `.a` returns the
///   alpha byte.
/// - `BgraNoAlpha` — depth-24 BGRA picture (r8g8b8 / x8r8g8b8):
///   swizzle `(IDENT, IDENT, IDENT, ONE)` so the shader sees
///   alpha = 1 per X RENDER's "alpha defaults to 1 when missing"
///   rule.
#[allow(
    dead_code,
    reason = "Variants are populated by Stage 3c's render_composite path"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SwizzleClass {
    RgbaIdent,
    AlphaOnlyR8,
    BgraNoAlpha,
}

/// Cached `vk::ImageView` for a `(DrawableId, SamplerConfig,
/// SwizzleClass)` triple. The engine destroys these on Drawable
/// retire (signalled by `DrawableStore::poll_pending_retire`).
/// The underlying `Drawable.storage.image` lifetime gates view
/// validity.
#[allow(
    dead_code,
    reason = "Built and consumed by Stage 3c's render_composite path"
)]
pub(crate) struct CachedDrawableView {
    pub(crate) view: vk::ImageView,
}

// ────────────────────────────────────────────────────────────────
// RenderEngine
// ────────────────────────────────────────────────────────────────

/// v2's rendering layer. Wraps an optional [`RenderEngineInner`]
/// so the test fixture (Vk-less) can construct an engine that
/// declines paint ops with a `NoVk` error instead of panicking.
pub(crate) struct RenderEngine {
    inner: Option<RenderEngineInner>,
}

struct RenderEngineInner {
    vk: Arc<VkContext>,
    /// Per-op CBs awaiting fence retirement. Drained by
    /// [`RenderEngine::poll_retired`] (called periodically by
    /// `KmsBackendV2` and at shutdown).
    submitted: VecDeque<SubmittedOp>,
    /// Stage 3b: per-picture GPU-side state. Today only carries
    /// gradient `GradientPicture` instances built lazily by Stage
    /// 3c's first `render_composite`; Stage 3b just ensures
    /// `render_free_picture` has a cleanup hook so an in-flight
    /// gradient's Vk handles get destroyed at the right moment.
    /// The empty `PicturePaintState` placeholder enum sits here
    /// until 3c needs to differentiate variants.
    picture_paint: HashMap<u32, PicturePaintState>,
    /// Stage 3a: glyph atlas. Lazy — first text run pays the
    /// 16 MiB R8 allocation. `None` until first image_text op.
    glyph_atlas: Option<V2GlyphAtlas>,
    /// Stage 3a: text pipeline (TextRunTarget descriptor bound to
    /// the atlas image view). Lazy — built once after the atlas
    /// is constructed. The pipeline's descriptor set references
    /// the atlas image view permanently; the atlas image's
    /// long-lived ownership makes this safe.
    text_pipeline: Option<TextPipeline>,
    /// Stage 3a: latest atlas-upload ticket. Cloned onto every
    /// atlas-consuming SubmittedOp (text runs, RENDER glyphs in
    /// Stage 3d) so the upload's per-call staging buffer and the
    /// atlas image stay alive on the CPU side until both upload
    /// and consume have retired. None when no upload has happened
    /// in the current session (atlas freshly created or every
    /// upload already retired).
    atlas_last_upload_ticket: Option<FenceTicket>,
    /// Stage 3c: lazy-built RENDER `Composite` pipeline cache.
    /// Adopted wholesale from v1. Pipelines compile on first use
    /// of each `(op, dst_format, dst_has_alpha, component_alpha)`
    /// key. `None` until the first `render_composite` call.
    render_pipelines: Option<RenderPipelineCache>,
    /// Stage 3c: 1×1 BGRA8 source scratch for `SolidFill` source.
    /// `record_solid_color_clear` rewrites the texel inside each
    /// composite CB before sampling. Lazy.
    solid_src_image: Option<SolidColorImage>,
    /// Stage 3c: 1×1 BGRA8 mask scratch for `SolidFill` mask.
    /// Same shape as `solid_src_image`. Lazy.
    solid_mask_image: Option<SolidColorImage>,
    /// Stage 3c: 1×1 BGRA8 mask scratch cleared once to opaque
    /// white. Bound as `mask_tex` for Composite calls without a
    /// mask — `mask.a == 1.0` makes the multiplication a no-op
    /// and keeps the shader / descriptor layout uniform. Lazy
    /// (pays one allocation + one-shot clear at first
    /// `render_composite`).
    white_mask_image: Option<SolidColorImage>,
    /// Stage 3c: `Disjoint` / `Conjoint` shader-side blend reads
    /// the current dst into this scratch before the draw samples
    /// it. Lazy.
    dst_readback: Option<DstReadback>,
    /// Stage 3c.3: self-alias scratch. When the resolved source
    /// (or mask) picture wraps the same backing as the destination
    /// (`src.drawable_id() == dst_id`), we copy dst into this
    /// scratch before the composite pass and bind its view as the
    /// `src_tex` / `mask_tex` descriptor instead of dst's own
    /// drawable view. Vulkan can't sample an image while it's bound
    /// as a color attachment in the same draw; the scratch breaks
    /// the alias. Reuses [`DstReadback`]'s growable per-format
    /// scratch shape — identical Vk requirements (sampled image +
    /// dst-format swizzle for no-alpha picture formats).
    src_alias_readback: Option<DstReadback>,
    /// Stage 3e.2: GPU rasterizer for RENDER `Trapezoids` /
    /// `Triangles`. Lazy — first trap/tri request pays the
    /// pipeline build.
    trap_pipeline: Option<crate::kms::vk::trap_pipeline::TrapPipeline>,
    /// Stage 3e.2: R8 coverage scratch the trap pipeline writes
    /// into, then the composite pass samples as a mask. Grows on
    /// demand (per-bbox). Lazy.
    ///
    /// Growth previously dropped the returned
    /// `Box<dyn BatchResource>` on the floor; B.2 Task 1
    /// ([`RenderEngineInner::adopt_retired_resource_for_gpu_retirement`])
    /// now routes it to the right fence-gated owner so the old
    /// backing's Vk handles are released only after the fence that
    /// last sampled them signals.
    mask_scratch: Option<crate::kms::vk::mask_scratch::MaskScratch>,
    /// Stage 3c: drawable view cache (plan §1). Keyed by
    /// `(DrawableId, SamplerConfig, SwizzleClass)`. Views are
    /// destroyed on Drawable retire; the engine's
    /// `notify_drawable_retired` hook prunes matching entries.
    drawable_view_cache: HashMap<(DrawableId, SamplerConfig, SwizzleClass), CachedDrawableView>,
    /// Stage 3f.2: per-`vk::Format` `LogicFillPipelineCache`. Built
    /// lazily on first non-`GXcopy` fill against a given dst format.
    /// The inner cache already keys its pipelines by
    /// `(GcFunction, opaque_alpha)`; we shard by `vk::Format` because
    /// each pipeline is bound to a single color attachment format at
    /// build time. Typical sessions only ever hold the
    /// `B8G8R8A8_UNORM` entry; R8 dst (depth 1/8) ops paint via
    /// `put_image` rather than fill, so the R8 branch only fires for
    /// rendercheck's `copy_plane` corner.
    logic_fill_caches:
        HashMap<vk::Format, crate::kms::vk::logic_fill_pipeline::LogicFillPipelineCache>,
    /// Stage 5 Task 4 layer 1: long-lived descriptor pool ring used
    /// by `try_vk_render_composite` + `try_vk_render_traps_or_tris`.
    /// Replaces per-call descriptor-pool instantiation. Spec
    /// `2026-05-21-descriptor-pool-ring-design.md`.
    descriptor_pool_ring: super::descriptor_pool_ring::DescriptorPoolRing,
    /// Stage 5 Task 4 layer 1: monotonic generation tag. Bumped on
    /// every paint-op submission; used as the watermark for ring
    /// pool recycling. The current value is passed to `acquire_set`
    /// and stamped onto the resulting `SubmittedOp` so the retirement
    /// loop can call `release_up_to(op.generation)`.
    acquire_generation: u64,
    /// Stage 5 Task 3 (render-composite generalization): pending
    /// render batch. See [`PendingRenderBatch`] above.
    pending_render_batch: Option<PendingRenderBatch>,
    /// Stage 5 Task 3: flush-records queue. Each render-batch flush
    /// pushes one record carrying op + has_mask + coalesced_count so
    /// the backend drain can emit a parametrised submit trace event.
    render_flush_records: Vec<RenderFlushRecord>,
    /// Stage 5 Task 6.1: submitted COW PRESENT-completion batches
    /// whose sync_file fds still need to be registered with the
    /// backend's inner epoll.
    pending_present_batches: Vec<PendingPresentBatch>,
    /// Phase A: per-group pending SubmittedOps. Each `end_and_submit_op`
    /// pushes here instead of directly into `submitted`. On successful
    /// `flush_submit_group` they drain into `submitted` (where
    /// poll_retired sees them). On failure (renderer_failed branch)
    /// they drop, releasing CBs + staging + scratch + their shared-
    /// ticket clones together.
    ///
    /// All entries in this vec share the same `FenceTicket` (Model A1).
    pending_group_ops: Vec<SubmittedOp>,
    /// Phase A: FlushOutcome records produced by flush_submit_group.
    /// Drained by the backend telemetry path (Task 3.5).
    pending_flush_outcomes: Vec<super::platform::FlushOutcome>,
    /// Phase B.1: in-flight frames awaiting retirement. Parallel to
    /// `submitted`; both gate on the same `FenceTicket`s when the
    /// frame builder is in play. Walked by `poll_retired` and
    /// `drain_all`.
    pending_frames: std::collections::VecDeque<super::frame_builder::FrameSubmittedRecord>,
    /// Phase B.1: telemetry events from close paths. Drained by the
    /// backend via `RenderEngine::drain_frame_close_events()`. Task 21
    /// wires the consumer side. Bounded at 1024 to prevent unbounded
    /// growth if maybe_composite stops ticking.
    pending_frame_close_events: Vec<super::frame_builder::FrameCloseEvent>,
    /// Phase B.1: monotonic frame sequence for telemetry attribution.
    /// Bumped on every `FrameBuilder::close_into_cb` success.
    frame_seq: u64,
    /// Phase B.1: per-frame deferred op-list recorder. `Closed` is
    /// the hot path; transitions to `OpenForPaint` only when a ported
    /// paint op (composite_glyphs in B.1) appends. Embedded so the
    /// engine can drive open/close from its existing paint entry
    /// points (Tasks 12-20 wire the transitions).
    frame_builder: super::frame_builder::FrameBuilder,
    /// Phase B.1 close trigger 4: cached timeout duration. Read once
    /// at engine construction from YSERVER_FRAME_BUILDER_TIMEOUT_MS
    /// (default 16 ms). Hot-path check in maybe_composite.
    frame_builder_timeout: std::time::Duration,
    /// GLX-TFP (Task 1.2): old Vk handles displaced by pixmap
    /// promotion, each paired with the fence guarding the old image's
    /// last render. Drained by [`RenderEngine::poll_retired`] once the
    /// fence signals (or `None`/already-signaled → freed eagerly by
    /// `retire_image_after`). Kept separate from `submitted` because
    /// the retire isn't gated on one of *our* CBs — it rides whatever
    /// ticket last touched the drawable.
    retired_promoted_images: Vec<(RetiredImage, Option<FenceTicket>)>,
}

impl RenderEngineInner {
    /// Phase B.2 Mechanism 3: route a retired scratch
    /// `BatchResource` (returned by
    /// [`crate::kms::vk::dst_readback::DstReadback::ensure_returning_old`]
    /// or
    /// [`crate::kms::vk::mask_scratch::MaskScratch::ensure_image_size_returning_old`]
    /// on a grow) to the right fence-gated owner.
    ///
    /// **Crucially:** `BatchResource::release(self: Box<Self>, &VkContext)`
    /// is explicit (see
    /// `crates/yserver/src/kms/scheduler/paint_batch.rs:146-147`).
    /// The trait does NOT implement `Drop` for Vk-handle teardown;
    /// dropping a `Box<dyn BatchResource>` without calling
    /// [`release`](crate::kms::v2::batch_resource::BatchResource::release)
    /// would LEAK the underlying Vk handles. Every retirement path
    /// MUST call `boxed.release(&inner.vk)` explicitly.
    ///
    /// Ownership cases (in order of precedence):
    /// - (a) Open frame's [`FramePinSet::retired_resources`]
    ///   (`self.frame_builder.open.as_mut().unwrap().pins`). The pin
    ///   set rides the frame's `FenceTicket`; the
    ///   `pending_frames` retire walk in
    ///   [`RenderEngine::poll_retired`] /
    ///   [`RenderEngine::drain_all`] releases each entry once the
    ///   ticket signals. Under B.2's grow-before-open rule (Phase
    ///   9A — to land in a later Task), this case is rarely hit
    ///   because every grow forces a close-reopen before any
    ///   in-frame op runs. Wiring it now keeps the helper complete
    ///   for B.3+ when mid-frame retire becomes possible.
    /// - (b) Newest [`SubmittedOp`] on `self.submitted`. After
    ///   `close_open_frame` succeeds the just-closed frame's CB has
    ///   appended one `SubmittedOp` carrying the frame's ticket;
    ///   attaching the retired Box here rides that fence. For
    ///   legacy callers (per-op submits), `submitted.back` is
    ///   likewise the newest fence owner. Using `submitted.back`
    ///   instead of `pending_frames.back` guarantees we pick the
    ///   NEWEST in-flight ticket (legacy SubmittedOps queued AFTER
    ///   a frame close are newer than the frame's record).
    /// - (c) Explicit release if both `frame_builder.open` is None
    ///   AND `submitted` is empty. Safe because no in-flight CB
    ///   can still be sampling the retired backing.
    ///
    ///   **M1 invariant assumption:** case (c) additionally
    ///   requires that `pending_group_ops` is empty at call time.
    ///   Under the Phase B M1 invariant (`submit_group_max_size = 1`
    ///   in production → auto-flush per op via
    ///   [`Self::maybe_auto_flush_submit_group`]), the parked
    ///   `Vec<SubmittedOp>` is drained at every op boundary, so a
    ///   grow that fires inside a paint op finds it empty by the
    ///   time the engine returns to a quiescent state. Tests that
    ///   raise the cap (e.g. `submit_group_max_size_for_tests(16)`)
    ///   can violate this invariant by leaving a previous paint
    ///   op's CB parked in `pending_group_ops` with a reference to
    ///   the OLD scratch's `vk::Image` handle; case (c) would then
    ///   destroy that handle before the parked CB submits. If a
    ///   later sub-phase (B.3+) relaxes M1 to allow cap>1 in
    ///   production, this helper must grow a fourth tier that
    ///   routes the retired Box onto
    ///   `pending_group_ops.back_mut()`'s retired-resources slot
    ///   (the type would need a `SubmittedOp`-style extension).
    ///   The `debug_assert!` below catches the regression in
    ///   debug builds.
    ///
    /// `None` input is a no-op (the common case: no grow fired).
    #[allow(
        dead_code,
        reason = "B.2 Task 1: helper lands now so the SubmittedOp + FramePinSet \
                  extensions compile. The _legacy ensure_returning_old call sites \
                  will be re-wired to call this helper in this same commit; the \
                  open-frame case (a) is exercised once Phase 9A's grow-before-open \
                  path lands in a later Task."
    )]
    pub(crate) fn adopt_retired_resource_for_gpu_retirement(
        &mut self,
        retired: Option<Box<dyn crate::kms::v2::batch_resource::BatchResource>>,
    ) {
        let Some(boxed) = retired else { return };
        // (a) Open frame — adopt into its pin set.
        if let Some(open) = self.frame_builder.open.as_mut() {
            open.pins.adopt_retired(boxed);
            return;
        }
        // (b) Newest in-flight SubmittedOp — append to its
        //     retired_resources; the op's fence retires it.
        if let Some(submitted) = self.submitted.back_mut() {
            submitted.append_retired_scratch(boxed);
            return;
        }
        // (c) Nothing in flight — safe to release immediately.
        //     M1 invariant: pending_group_ops MUST be empty here.
        //     If a future sub-phase relaxes cap=1 auto-flush and a
        //     parked op's CB still references the retired backing,
        //     this release would destroy a live Vk handle. See the
        //     docstring above for the fix shape (fourth tier onto
        //     pending_group_ops.back_mut()).
        debug_assert!(
            self.pending_group_ops.is_empty(),
            "adopt_retired_resource_for_gpu_retirement case (c): \
             pending_group_ops must be empty under M1 (cap=1 \
             auto-flush per op). If B.3+ relaxes M1, add a fourth \
             tier that routes onto pending_group_ops.back_mut()."
        );
        boxed.release(&self.vk);
    }

    /// Phase B.2 Mechanism 2: acquire a descriptor set tagged with
    /// the right generation watermark. When a frame is open, every
    /// acquire shares the frame's captured `frame_generation`; the
    /// SubmittedOp pushed at close carries the same value, so the
    /// retire walk's `release_up_to(op.generation)` retires exactly
    /// the frame's pools. When no frame is open (legacy per-op
    /// fallback path), bump `acquire_generation` and use the new
    /// value — same shape as the pre-B.2 code.
    ///
    /// **Load-bearing safety invariant** (codex round 3 finding 3):
    /// `DescriptorPoolRing::acquire_set(layout, generation)` only
    /// allocates from pools whose state is `Active` (currently
    /// growing — never seen `vkResetDescriptorPool`) OR was just
    /// transitioned `Free → Active` via `ensure_active_with_capacity`
    /// after the ring's `release_up_to` reset it. The ring's
    /// `release_up_to(retired_watermark)` only resets pools whose
    /// `high_water_generation <= retired_watermark` (via
    /// `vkResetDescriptorPool`), and Vulkan
    /// VUID-vkResetDescriptorPool-descriptorPool-00313 mandates that
    /// all CBs referencing the pool's sets must have completed
    /// execution before reset. Therefore:
    ///
    /// - **Active pool case:** allocating from a still-growing pool
    ///   produces a handle to backing storage that has NEVER been
    ///   written to by `vkAllocateDescriptorSets` before; no prior
    ///   CB can possibly reference it.
    /// - **Just-reset pool case:** the reset guarantees no in-flight
    ///   CB depends on any of the pool's prior sets; the new
    ///   `vkAllocateDescriptorSets` call produces fresh handles
    ///   whose backing storage is also CB-independent.
    ///
    /// Either way, the descriptor set returned by `acquire_set` has
    /// zero in-flight CB dependencies. `vkUpdateDescriptorSets`
    /// against it at op-append time is safe per Vulkan host-mutation
    /// rules (VUID-vkUpdateDescriptorSets-pDescriptorWrites-06493):
    /// the targeted set must not be used by any pending command
    /// buffer.
    ///
    /// **This invariant is load-bearing for B.2.** If a future
    /// refactor changes the ring to recycle descriptor sets without
    /// going through reset (e.g. a hypothetical "fast-reuse" path),
    /// `vkUpdateDescriptorSets`-at-append would become unsafe. The
    /// audit at
    /// `crates/yserver/src/kms/v2/descriptor_pool_ring.rs` (Task 3
    /// audit gate) confirms the current ring matches this invariant.
    ///
    /// # Errors
    ///
    /// Propagates `vkAllocateDescriptorSets` / `vkResetDescriptorPool`
    /// errors verbatim. Callers convert to `RenderError::Vk`.
    #[allow(
        dead_code,
        reason = "B.2 Task 3: helper lands now for B.3+ render-composite porting. \
                  Task 11 in B.2 routes render_composite through \
                  RenderPipeline::allocate_descriptor_for_views_into_ring, \
                  not this helper. The frame-open branch becomes hot in B.3."
    )]
    pub(crate) fn acquire_descriptor_set_for_frame_or_op(
        &mut self,
        layout: vk::DescriptorSetLayout,
    ) -> Result<vk::DescriptorSet, vk::Result> {
        let generation = if let Some(open) = self.frame_builder.open.as_ref() {
            open.frame_generation
        } else {
            self.acquire_generation = self.acquire_generation.saturating_add(1);
            self.acquire_generation
        };
        self.descriptor_pool_ring.acquire_set(layout, generation)
    }

    /// Phase B.2 Task 4 (USER-codex U-R6.F1 — LOAD-BEARING):
    /// overlay-as-source-of-truth read accessor for the layout of
    /// `id` from the perspective of the next in-frame paint op.
    ///
    /// - When a frame is open: consults the `FrameLayoutTable`. If the
    ///   drawable has been first-touched in-frame, returns its
    ///   `current_in_frame_layout`. Otherwise falls back to
    ///   `Drawable::storage.current_layout` (the pre-frame value).
    /// - When no frame is open: returns `Drawable::storage.current_layout`
    ///   directly (legacy per-op path; storage is the source of truth).
    ///
    /// Storage fallback: a drawable that isn't in `store` resolves to
    /// `UNDEFINED` — matches `Storage::for_tests_null`'s default and
    /// is the only sensible answer for a missing entry; callers that
    /// dereference the result for a barrier source must have already
    /// validated the id.
    ///
    /// Open-frame paint-op ports (Tasks 11-13) MUST use this accessor
    /// to read the dst/src/mask drawable's old_layout when emitting
    /// barriers — see Pitfall 5 in
    /// `docs/superpowers/plans/2026-05-24-frame-builder-phase-b2.md`.
    /// Reading `storage.current_layout` directly during recording
    /// returns a STALE value (storage is deliberately not mutated
    /// during recording so failed frames roll back via overlay drop).
    #[allow(
        dead_code,
        reason = "B.2 Task 4: helper lands now; Tasks 11+ rewire the open-frame \
                  render_composite path to call this accessor instead of \
                  reading storage directly."
    )]
    pub(crate) fn current_layout_for_drawable(
        &self,
        store: &DrawableStore,
        id: DrawableId,
    ) -> vk::ImageLayout {
        let storage_fallback = store
            .get(id)
            .map(|d| d.storage.current_layout)
            .unwrap_or(vk::ImageLayout::UNDEFINED);
        if let Some(open) = self.frame_builder.open.as_ref() {
            open.layouts
                .current_layout_for_drawable(id, storage_fallback)
        } else {
            storage_fallback
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameBuilderTraceFilter {
    DrawableId(u64),
    RedirectedArgbBackings,
}

impl RenderEngine {
    fn frame_builder_trace_filter() -> Option<FrameBuilderTraceFilter> {
        let raw = std::env::var("YSERVER_FB_TRACE_DRAWABLE_ID").ok()?;
        let s = raw.trim();
        if s.eq_ignore_ascii_case("redirected-argb-backings") {
            return Some(FrameBuilderTraceFilter::RedirectedArgbBackings);
        }
        if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
            return u64::from_str_radix(hex, 16)
                .ok()
                .map(FrameBuilderTraceFilter::DrawableId);
        }
        s.parse::<u64>()
            .ok()
            .map(FrameBuilderTraceFilter::DrawableId)
    }

    fn frame_builder_trace_matches_dst(
        store: &DrawableStore,
        filter: FrameBuilderTraceFilter,
        dst_id: DrawableId,
    ) -> bool {
        match filter {
            FrameBuilderTraceFilter::DrawableId(raw) => dst_id.as_u64() == raw,
            FrameBuilderTraceFilter::RedirectedArgbBackings => store
                .get(dst_id)
                .is_some_and(|d| d.depth == 32 && store.is_active_redirect_target(dst_id)),
        }
    }

    fn trace_frame_ops(
        store: &DrawableStore,
        frame_seq: u64,
        ops: &[super::frame_builder::RecordedOp],
        filter: FrameBuilderTraceFilter,
    ) {
        use super::frame_builder::{RecordedOp, RecordedTrapSrcKind};

        let hits = ops
            .iter()
            .filter(|op| {
                op.dst_id().is_some_and(|dst_id| {
                    Self::frame_builder_trace_matches_dst(store, filter, dst_id)
                })
            })
            .count();
        if hits == 0 {
            return;
        }
        log::warn!(
            target: "yserver::kms::v2::fbtrace",
            "fbtrace frame_seq={} filter={:?} matched_ops={} total_ops={}",
            frame_seq,
            filter,
            hits,
            ops.len(),
        );
        for (idx, op) in ops.iter().enumerate() {
            match op {
                RecordedOp::RenderComposite(rc)
                    if Self::frame_builder_trace_matches_dst(store, filter, rc.dst_id) =>
                {
                    log::warn!(
                        target: "yserver::kms::v2::fbtrace",
                        "fbtrace frame_seq={} op#{} RenderComposite dst={} op={} dst_has_alpha={} mask_ca={} rects={} clips={} src_clear={} mask_clear={} old_layout={:?}",
                        frame_seq, idx, rc.dst_id.as_u64(), rc.op, rc.dst_has_alpha,
                        rc.mask_component_alpha, rc.rects.len(),
                        rc.clip_rects.as_deref().map_or(0, |r| r.len()),
                        rc.src_clear_color.is_some(), rc.mask_clear_color.is_some(),
                        rc.dst_old_layout,
                    )
                }
                RecordedOp::CopyArea(ca)
                    if Self::frame_builder_trace_matches_dst(store, filter, ca.dst_id) =>
                {
                    log::warn!(
                        target: "yserver::kms::v2::fbtrace",
                        "fbtrace frame_seq={} op#{} CopyArea dst={} src={} src_off=({}, {}) dst_off=({}, {}) extent={}x{} self_overlap={} old_layouts=({:?}->{:?})",
                        frame_seq, idx, ca.dst_id.as_u64(), ca.src_id.as_u64(),
                        ca.src_rect.offset.x, ca.src_rect.offset.y, ca.dst_rect.offset.x,
                        ca.dst_rect.offset.y, ca.dst_rect.extent.width, ca.dst_rect.extent.height,
                        ca.self_overlap_scratch.is_some(), ca.src_old_layout, ca.dst_old_layout,
                    )
                }
                RecordedOp::PutImage(pi)
                    if Self::frame_builder_trace_matches_dst(store, filter, pi.dst_id) =>
                {
                    log::warn!(
                        target: "yserver::kms::v2::fbtrace",
                        "fbtrace frame_seq={} op#{} PutImage dst={} off=({}, {}) extent={}x{} old_layout={:?}",
                        frame_seq, idx, pi.dst_id.as_u64(), pi.dst_rect.offset.x,
                        pi.dst_rect.offset.y, pi.dst_rect.extent.width, pi.dst_rect.extent.height,
                        pi.dst_old_layout,
                    )
                }
                RecordedOp::FillRect(fr)
                    if Self::frame_builder_trace_matches_dst(store, filter, fr.dst_id) =>
                {
                    log::warn!(
                        target: "yserver::kms::v2::fbtrace",
                        "fbtrace frame_seq={} op#{} FillRect dst={} rects={} color={:?} old_layout={:?}",
                        frame_seq, idx, fr.dst_id.as_u64(), fr.rects.len(), fr.color, fr.dst_old_layout,
                    )
                }
                RecordedOp::LogicFill(lf)
                    if Self::frame_builder_trace_matches_dst(store, filter, lf.dst_id) =>
                {
                    log::warn!(
                        target: "yserver::kms::v2::fbtrace",
                        "fbtrace frame_seq={} op#{} LogicFill dst={} mode={:?} opaque_alpha={} rects={} color={:?} old_layout={:?}",
                        frame_seq, idx, lf.dst_id.as_u64(), lf.logic_mode, lf.opaque_alpha,
                        lf.rects.len(), lf.color, lf.dst_old_layout,
                    )
                }
                RecordedOp::ImageText(it)
                    if Self::frame_builder_trace_matches_dst(store, filter, it.dst_id) =>
                {
                    log::warn!(
                        target: "yserver::kms::v2::fbtrace",
                        "fbtrace frame_seq={} op#{} ImageText dst={} glyphs={} fg={:?} old_layout={:?}",
                        frame_seq, idx, it.dst_id.as_u64(), it.glyphs.len(), it.foreground_rgba,
                        it.dst_old_layout,
                    )
                }
                RecordedOp::CompositeGlyphs(cg)
                    if Self::frame_builder_trace_matches_dst(store, filter, cg.dst_id) =>
                {
                    log::warn!(
                        target: "yserver::kms::v2::fbtrace",
                        "fbtrace frame_seq={} op#{} CompositeGlyphs dst={} glyphs={} clips={} fg={:?} old_layout={:?}",
                        frame_seq, idx, cg.dst_id.as_u64(), cg.glyphs.len(), cg.clip_scissors.len(),
                        cg.foreground_rgba, cg.dst_old_layout,
                    )
                }
                RecordedOp::RenderTrapsOrTris(rt)
                    if Self::frame_builder_trace_matches_dst(store, filter, rt.dst_id) =>
                {
                    let src_kind = match &rt.src_kind {
                        RecordedTrapSrcKind::Drawable { .. } => "drawable",
                        RecordedTrapSrcKind::Solid(_) => "solid",
                        RecordedTrapSrcKind::Gradient { .. } => "gradient",
                    };
                    log::warn!(
                        target: "yserver::kms::v2::fbtrace",
                        "fbtrace frame_seq={} op#{} RenderTrapsOrTris dst={} op_byte={} dst_has_alpha={} src_kind={} clips={} bbox=({},{} {}x{}) old_layout={:?}",
                        frame_seq, idx, rt.dst_id.as_u64(), rt.op_byte, rt.dst_has_alpha,
                        src_kind, rt.clip_scissors.len(), rt.bbox_x, rt.bbox_y,
                        rt.bbox_w, rt.bbox_h, rt.dst_old_layout,
                    );
                }
                _ => {}
            }
        }
    }

    /// Production constructor. Borrows the platform's `VkContext`
    /// (cloned `Arc`); CB allocation goes through the platform's
    /// shared `OpsCommandPool` on each op.
    ///
    /// # Errors
    ///
    /// Returns `NoVk` if `platform` was built via `for_tests`
    /// (no Vk). Production paths always have Vk.
    pub(crate) fn new(platform: &PlatformBackend) -> Result<Self, RenderError> {
        let vk = platform.vk().ok_or(RenderError::NoVk)?.clone();
        let descriptor_pool_ring =
            super::descriptor_pool_ring::DescriptorPoolRing::new(Arc::clone(&vk));
        Ok(Self {
            inner: Some(RenderEngineInner {
                vk,
                submitted: VecDeque::new(),
                picture_paint: HashMap::new(),
                glyph_atlas: None,
                text_pipeline: None,
                atlas_last_upload_ticket: None,
                render_pipelines: None,
                solid_src_image: None,
                solid_mask_image: None,
                white_mask_image: None,
                dst_readback: None,
                src_alias_readback: None,
                trap_pipeline: None,
                mask_scratch: None,
                drawable_view_cache: HashMap::new(),
                logic_fill_caches: HashMap::new(),
                descriptor_pool_ring,
                acquire_generation: 0,
                pending_render_batch: None,
                render_flush_records: Vec::new(),
                pending_present_batches: Vec::new(),
                pending_group_ops: Vec::new(),
                pending_flush_outcomes: Vec::new(),
                pending_frames: std::collections::VecDeque::new(),
                pending_frame_close_events: Vec::new(),
                frame_seq: 0,
                frame_builder: super::frame_builder::FrameBuilder::new(),
                frame_builder_timeout:
                    super::frame_builder::FrameBuilder::timeout_from_env_default_16ms(),
                retired_promoted_images: Vec::new(),
            }),
        })
    }

    /// Vk-less constructor — used by `KmsBackendV2::for_tests` and
    /// Stage 1b-era callers that haven't migrated yet. Every paint
    /// op on a stubbed engine returns `NoVk`.
    pub(crate) fn stub() -> Self {
        Self { inner: None }
    }

    /// Whether the engine has a live Vk inner. Tests use this to
    /// skip Vk-backed assertions on the stub fixture.
    pub(crate) fn is_live(&self) -> bool {
        self.inner.is_some()
    }

    /// Walk `submitted`, dropping entries whose [`FenceTicket`]
    /// has signaled. Their CB is freed and any staging buffer
    /// destroyed.
    pub(crate) fn poll_retired(&mut self, platform: &PlatformBackend) {
        let Some(inner) = self.inner.as_mut() else {
            return;
        };
        let Some(pool) = platform.ops_command_pool_handle() else {
            return;
        };
        let device = &inner.vk.device;
        // Walk front-to-back, removing prefixes that have signaled.
        // Same-queue submission order guarantees prefix-signal
        // monotonicity; if entry N's ticket is signaled, entry
        // N-1's also is. We could short-circuit on first
        // unsignaled but the loop is small enough to walk all.
        while let Some(front) = inner.submitted.front() {
            if !front.ticket.poll_signaled(&inner.vk) {
                break;
            }
            let mut op = inner.submitted.pop_front().expect("non-empty");
            unsafe {
                device.free_command_buffers(pool, &[op.cb]);
            }
            // staging drops at end of scope → destroys Vk handles.
            drop(op.staging.take());
            // Phase B.2 Mechanism 3: release retired BatchResources
            // attached via adopt_retired_resource_for_gpu_retirement
            // case (b). BatchResource::release is explicit (no Drop);
            // dropping the Box without this call would LEAK Vk handles
            // (see paint_batch.rs:147).
            for r in op.drain_retired_scratch() {
                r.release(&inner.vk);
            }
            // Stage 5 Task 4 layer 1: signal the descriptor pool
            // ring that everything up to and including this op's
            // generation has retired. Pools whose high_water_
            // generation <= op.generation transition InFlight → Free
            // via vkResetDescriptorPool.
            inner.descriptor_pool_ring.release_up_to(op.generation);
        }
        // Phase B.1: walk pending_frames. Same ticket-signaled monotonicity
        // argument as the `submitted` loop above (same-queue submission
        // order signals tickets in order).
        while let Some(front) = inner.pending_frames.front() {
            if !front.ticket.poll_signaled(&inner.vk) {
                break;
            }
            let mut record = inner.pending_frames.pop_front().expect("non-empty");
            // Phase B.2 Mechanism 3 (defensive): release retired
            // BatchResources attached via case (a) of
            // adopt_retired_resource_for_gpu_retirement. Under B.2
            // this Vec is structurally empty — the grow-before-open
            // rule routes all retires through submitted.back — but
            // explicit release here keeps the invariant honest for
            // B.3+ when mid-frame retire becomes possible. Without
            // it, Vk handles inside the Boxes would leak (no Drop on
            // BatchResource; see paint_batch.rs:147).
            for r in record.pins.retired_resources.drain(..) {
                r.release(&inner.vk);
            }
            // The Arcs inside the record drop here, releasing pinned resources.
            drop(record);
        }
        // GLX-TFP (Task 1.2): free old promotion-displaced images whose
        // guarding fence has signaled. No ordering relationship to the
        // queues above (each rides its own ticket), so retain-filter
        // rather than pop-prefix.
        if !inner.retired_promoted_images.is_empty() {
            let vk = Arc::clone(&inner.vk);
            inner.retired_promoted_images.retain(|(retired, guard)| {
                let signaled = guard.as_ref().is_none_or(|t| t.poll_signaled(&vk));
                if signaled {
                    Self::destroy_retired_image(&vk, retired);
                }
                !signaled
            });
        }
    }

    /// Destroy the Vk handles of a promotion-displaced [`RetiredImage`].
    /// Order: sample_view, image_view, image, memory (views before the
    /// image they reference; image before its backing memory). Null
    /// handles are no-ops per the Vulkan spec.
    fn destroy_retired_image(vk: &VkContext, retired: &RetiredImage) {
        unsafe {
            if retired.sample_view != vk::ImageView::null() {
                vk.device.destroy_image_view(retired.sample_view, None);
            }
            if retired.image_view != vk::ImageView::null() {
                vk.device.destroy_image_view(retired.image_view, None);
            }
            if retired.image != vk::Image::null() {
                vk.device.destroy_image(retired.image, None);
            }
            if retired.memory != vk::DeviceMemory::null() {
                vk.device.free_memory(retired.memory, None);
            }
        }
    }

    /// Push old promotion-displaced handles onto the deferred-destroy
    /// list. If `guard` is `None` or already signaled, the handles are
    /// destroyed immediately; otherwise they're parked until
    /// [`Self::poll_retired`] observes the fence signal.
    fn retire_image_after(&mut self, retired: RetiredImage, guard: Option<FenceTicket>) {
        let Some(inner) = self.inner.as_mut() else {
            // No Vk inner: nothing to destroy against. (The handles are
            // necessarily null in the stub case.)
            return;
        };
        let ready = guard.as_ref().is_none_or(|t| t.poll_signaled(&inner.vk));
        if ready {
            Self::destroy_retired_image(&inner.vk, &retired);
        } else {
            inner.retired_promoted_images.push((retired, guard));
        }
    }

    /// Phase B.1: production-side shutdown. Closes any open frame
    /// first, then defers to `drain_all` for the existing
    /// `SubmitGroup` + submitted-queue + `pending_frames` drain.
    ///
    /// Test call sites that construct a fresh engine/platform/store
    /// and never open a frame can keep using `drain_all` directly.
    pub(crate) fn shutdown(&mut self, store: &mut DrawableStore, platform: &mut PlatformBackend) {
        if let Err(e) =
            self.close_open_frame(store, platform, super::frame_builder::CloseReason::Shutdown)
        {
            log::warn!("v2 shutdown: close_open_frame failed: {e:?}");
        }
        self.drain_all(platform);
    }

    /// Drain every in-flight submit, waiting on the deepest
    /// ticket. Called at shutdown to ensure all CB / staging
    /// resources are reclaimed before pool destruction.
    ///
    /// PRECONDITION: callers must close any open render batches
    /// (via `flush_render_batch`) BEFORE calling `drain_all`,
    /// because that method needs `&mut DrawableStore` which is not
    /// available here. The production call site (`disable_output`)
    /// already satisfies this. Any open batch that reaches here is
    /// dropped with a warning to avoid a non-empty/no-ticket panic
    /// on `flush_submit_group(Shutdown)`.
    pub(crate) fn drain_all(&mut self, platform: &mut PlatformBackend) {
        // Drop any open render batch that reached us without being
        // flushed. This should never happen when called from the
        // production code path (disable_output closes batches first),
        // but guards against shutdown-time panics if the invariant is
        // violated (e.g. a future call site that forgets to close
        // batches first).
        if self
            .inner
            .as_mut()
            .is_some_and(|i| i.pending_render_batch.take().is_some())
        {
            log::warn!(
                "v2 drain_all: open render_batch dropped without flush \
                 (caller must close batches before drain_all)"
            );
        }
        // Flush any open SubmitGroup first; this commits parked ops
        // into `submitted` so the loop below sees the right set.
        //
        // GLX-TFP: this shutdown flush drives `platform.flush_submit_group`
        // directly (no exported-write publish): at drain_all the engine
        // has no `store` borrow, and the GL consumer is going away, so
        // re-publishing write fences here is pointless. The engine's
        // commit-of-parked-ops bookkeeping below is replicated from the
        // `flush_submit_group` wrapper.
        let result = platform.flush_submit_group(super::submit_group::FlushReason::Shutdown);
        if let Some(outcome) = platform.take_last_flush_outcome()
            && let Some(inner) = self.inner.as_mut()
        {
            inner.pending_flush_outcomes.push(outcome);
        }
        match (result, self.inner.as_mut()) {
            (Ok(_), Some(inner)) => {
                for op in inner.pending_group_ops.drain(..) {
                    inner.submitted.push_back(op);
                }
            }
            (Err(e), inner_opt) => {
                if let Some(inner) = inner_opt {
                    inner.pending_group_ops.clear();
                }
                log::warn!("v2 drain_all: flush_submit_group failed: {e:?}");
            }
            (Ok(_), None) => {}
        }
        let Some(inner) = self.inner.as_mut() else {
            return;
        };
        let Some(pool) = platform.ops_command_pool_handle() else {
            return;
        };
        let device = &inner.vk.device;
        // Wait on each ticket in order. Off-hot-path; one wait
        // per pending op is fine at shutdown.
        while let Some(mut op) = inner.submitted.pop_front() {
            let _ = op.ticket.wait(&inner.vk);
            unsafe {
                device.free_command_buffers(pool, &[op.cb]);
            }
            drop(op.staging.take());
            // Phase B.2 Mechanism 3: explicit release of retired
            // BatchResources attached via case (b). See
            // poll_retired for the rationale (BatchResource has no
            // Drop — paint_batch.rs:147).
            for r in op.drain_retired_scratch() {
                r.release(&inner.vk);
            }
            inner.descriptor_pool_ring.release_up_to(op.generation);
        }
        // Phase B.1: drain in-flight frame pins. wait() ensures Vk-side
        // completion before the Arc<StagingBuffer> drops would otherwise
        // race with GPU reads. Off-hot-path; one wait per pending frame
        // is fine at shutdown.
        while let Some(mut record) = inner.pending_frames.pop_front() {
            let _ = record.ticket.wait(&inner.vk);
            // Phase B.2 Mechanism 3 (defensive): release retired
            // BatchResources attached via case (a). See poll_retired
            // for the rationale.
            for r in record.pins.retired_resources.drain(..) {
                r.release(&inner.vk);
            }
            // Record drops; pins drop; Arcs decrement.
            drop(record);
        }
        // GLX-TFP (Task 1.2): wait out + free any promotion-displaced
        // images still parked. The earlier `submitted` / `pending_frames`
        // waits don't necessarily cover their guarding tickets (a guard
        // can be a foreign ticket), so wait each explicitly.
        let vk = Arc::clone(&inner.vk);
        for (retired, guard) in inner.retired_promoted_images.drain(..) {
            if let Some(t) = guard.as_ref() {
                let _ = t.wait(&vk);
            }
            Self::destroy_retired_image(&vk, &retired);
        }
    }

    /// Phase A: flush the platform's SubmitGroup and commit/drop the
    /// engine's parked per-op state atomically. THIS is the API every
    /// flush-trigger site calls (scene compose, get_image, PRESENT
    /// signal, pageflip retire, shutdown, MaxSize auto-flush) —
    /// NEVER call `platform.flush_submit_group` directly from outside
    /// the engine.
    pub(crate) fn flush_submit_group(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        reason: super::submit_group::FlushReason,
    ) -> Result<super::platform::FlushOutcome, vk::Result> {
        // GLX-TFP (Task 2.3): drain the exported drawables written since
        // the last flush; the platform waits on / publishes their dma-buf
        // implicit-sync fences around this submit. `Arc<OwnedFd>` clones
        // keep the fds alive across the call without re-borrowing `store`.
        //
        // Only drain when a real `vkQueueSubmit2` will occur (group
        // non-empty). The platform short-circuits an empty group without
        // submitting (an open render-batch/COW may be mid-recording); if
        // we drained here on that no-submit path we'd silently drop the
        // recorded exported writes and the eventual real submit would skip
        // their wait/publish. Leaving them queued lets the next non-empty
        // flush pick them up.
        let exported = if platform.submit_group_size() > 0 {
            store.take_exported_writes()
        } else {
            Vec::new()
        };
        let exported_borrows: Vec<std::os::fd::BorrowedFd<'_>> = {
            use std::os::fd::AsFd as _;
            exported.iter().map(|f| f.as_fd()).collect()
        };
        let result = platform.flush_submit_group_with_exports(reason, &exported_borrows);
        // Drain the platform's last_flush_outcome regardless of Ok/Err
        // — both branches in platform's flush_submit_group populate it
        // before returning. The engine queues it for backend telemetry
        // drain (Task 3.5 wires that side).
        if let Some(outcome) = platform.take_last_flush_outcome()
            && let Some(inner) = self.inner.as_mut()
        {
            inner.pending_flush_outcomes.push(outcome);
        }
        let Some(inner) = self.inner.as_mut() else {
            return result;
        };
        match result {
            Ok(outcome) => {
                // Commit: parked ops graduate to `submitted`.
                for op in inner.pending_group_ops.drain(..) {
                    inner.submitted.push_back(op);
                }
                Ok(outcome)
            }
            Err(e) => {
                // Rollback. CBs were already freed by platform's Err
                // branch. Engine just clears the parked SubmittedOps so
                // their staging / scratch / atlas_ticket / shared-fence-
                // Arc clones drop together.
                inner.pending_group_ops.clear();
                Err(e)
            }
        }
    }

    /// Phase A: check whether the platform's SubmitGroup has hit its
    /// cap; if so, drive a `MaxSize` flush.
    pub(crate) fn maybe_auto_flush_submit_group(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
    ) -> Result<(), RenderError> {
        if platform.submit_group_size() >= platform.submit_group_max_size() {
            self.flush_submit_group(store, platform, super::submit_group::FlushReason::MaxSize)
                .map_err(RenderError::Vk)?;
        }
        Ok(())
    }

    /// Phase B.1 Task 21: drain queued `FrameCloseEvent`s for telemetry.
    /// Returns empty when no events queued (or when engine is stubbed).
    pub(crate) fn drain_frame_close_events(
        &mut self,
    ) -> Vec<super::frame_builder::FrameCloseEvent> {
        self.inner
            .as_mut()
            .map(|i| std::mem::take(&mut i.pending_frame_close_events))
            .unwrap_or_default()
    }

    /// Phase B.1 Task 12: close the open frame (if any) for `reason`,
    /// replay its op list into ONE primary CB, submit through the
    /// `SubmitGroup` (cap=1 → one vkQueueSubmit2), and ONLY THEN park
    /// the pin set onto `pending_frames` + commit overlays. On any
    /// failure before submit-success, the local `OpenFrame` drops
    /// (pins evaporate, overlays evaporate); rollback writes
    /// `pre_frame_layout` values back to storage where the recorder
    /// already mutated them.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn close_open_frame(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        reason: super::frame_builder::CloseReason,
    ) -> Result<super::frame_builder::CloseOutcome, RenderError> {
        // Take the open frame from the FrameBuilder.
        let (mut open_frame, frame_seq) = {
            let Some(inner) = self.inner.as_mut() else {
                return Ok(super::frame_builder::CloseOutcome::AlreadyClosed);
            };
            let Some(open_frame_box) = inner.frame_builder.take_open_for_close(reason) else {
                return Ok(super::frame_builder::CloseOutcome::AlreadyClosed);
            };
            inner.frame_seq = inner.frame_seq.wrapping_add(1);
            (*open_frame_box, inner.frame_seq)
        };
        let frame_ticket = open_frame.ticket.clone();
        if let Some(filter) = Self::frame_builder_trace_filter() {
            Self::trace_frame_ops(store, frame_seq, &open_frame.ops, filter);
        }

        // Phase B.2 Task 14: count RenderComposite ops once for the
        // telemetry close-event. `open_frame.ops` is not mutated
        // between this point and any of the 5 close-event push sites
        // below (success + 4 error paths: begin_op_cb err, record err,
        // end_and_submit err, flush_submit_group err), so a single
        // tally is safe and avoids re-walking the op list on each path.
        let renders_in_frame: u32 = u32::try_from(
            open_frame
                .ops
                .iter()
                .filter(|op| matches!(op, super::frame_builder::RecordedOp::RenderComposite(_)))
                .count(),
        )
        .unwrap_or(u32::MAX);

        // Allocate the primary CB.
        let cb = {
            let inner = self.inner.as_mut().expect("inner");
            match begin_op_cb(inner, platform) {
                Ok((cb, op_ticket)) => {
                    // Phase B.1 invariant: the begin_op_cb ticket MUST be the
                    // same fence the frame opened against. If they diverge,
                    // the SubmitGroup was flushed mid-frame (no current
                    // code path does that, but a future regression would
                    // park pins on the wrong fence).
                    debug_assert_eq!(
                        op_ticket.fence(),
                        frame_ticket.fence(),
                        "begin_op_cb returned a different fence than the open frame's — \
                         SubmitGroup was flushed mid-frame?"
                    );
                    cb
                }
                Err(e) => {
                    rollback_pre_submit(store, &mut open_frame);
                    let inner_post = self.inner.as_mut().expect("inner");
                    rollback_atlas(
                        inner_post,
                        open_frame.layouts.atlas,
                        open_frame.atlas_prev_ticket_snapshot.clone(),
                    );
                    // Phase B.2 Mechanism 3 (defensive): release any
                    // retired BatchResources attached to the open
                    // frame's pin set. Structurally empty under B.2's
                    // grow-before-open rule, but BatchResource has no
                    // Drop (paint_batch.rs:147); preserved for B.3+.
                    for r in open_frame.pins.retired_resources.drain(..) {
                        r.release(&inner_post.vk);
                    }
                    if inner_post.pending_frame_close_events.len() < 1024 {
                        inner_post.pending_frame_close_events.push(
                            super::frame_builder::FrameCloseEvent {
                                reason,
                                ops_in_frame: open_frame.ops.len(),
                                glyph_uploads_in_frame: open_frame.glyph_uploads_in_frame,
                                renders_in_frame,
                                pin_count: open_frame.pins.len(),
                                aborted: true,
                            },
                        );
                    }
                    inner_post.frame_builder.complete_close_failure();
                    return Err(e);
                }
            }
        };

        // Pass 1 (resource) — no-op in B.1.
        // Pass 2 (record) — record each op into cb.
        let record_result: Result<(), RenderError> = {
            let inner = self.inner.as_mut().expect("inner");
            let mut acc: Result<(), RenderError> = Ok(());
            let frame_generation = open_frame.frame_generation;
            for op in &open_frame.ops {
                if let Err(e) = emit_recorded_op_into_cb(
                    inner,
                    store,
                    cb,
                    &open_frame.pins,
                    frame_generation,
                    op,
                ) {
                    acc = Err(e);
                    break;
                }
            }
            acc
        };

        if let Err(e) = record_result {
            // CB never appended to SubmitGroup. Free it ourselves.
            {
                let inner = self.inner.as_mut().expect("inner");
                let device = &inner.vk.device;
                if let Some(pool) = platform.ops_command_pool_handle() {
                    // SAFETY: cb was allocated from `pool` and never
                    // submitted; safe to free in Recording state.
                    unsafe { device.free_command_buffers(pool, &[cb]) };
                }
            }
            rollback_pre_submit(store, &mut open_frame);
            platform.renderer_failed = true;
            let inner_post = self.inner.as_mut().expect("inner");
            rollback_atlas(
                inner_post,
                open_frame.layouts.atlas,
                open_frame.atlas_prev_ticket_snapshot.clone(),
            );
            // Phase B.2 Mechanism 3 (defensive): release any retired
            // BatchResources attached to the open frame's pin set.
            // See path 1 above for rationale.
            for r in open_frame.pins.retired_resources.drain(..) {
                r.release(&inner_post.vk);
            }
            if inner_post.pending_frame_close_events.len() < 1024 {
                inner_post
                    .pending_frame_close_events
                    .push(super::frame_builder::FrameCloseEvent {
                        reason,
                        ops_in_frame: open_frame.ops.len(),
                        glyph_uploads_in_frame: open_frame.glyph_uploads_in_frame,
                        renders_in_frame,
                        pin_count: open_frame.pins.len(),
                        aborted: true,
                    });
            }
            inner_post.frame_builder.complete_close_failure();
            return Err(e);
        }

        // Phase B.3 (N10) — branch (a) PRE-SUBMIT: acquire PresentCompletionSignal
        // BEFORE end_and_submit_op_with_signal so the semaphore is queued on the
        // submit's signal list. Acquiring after submit means the semaphore is
        // never queued; the exported sync_file fd would never fire (Pitfall 8).
        let completion_signal: Option<PresentCompletionSignal> = {
            let pending_count = open_frame.pending_present_completions.len();
            if pending_count == 0 {
                None
            } else {
                match platform.acquire_present_completion_signal() {
                    Ok(s) => Some(s),
                    Err(e) => {
                        // Signal-acquire failure: route through the same
                        // append-failure rollback (free CB + rollback + mark failed).
                        {
                            let inner = self.inner.as_mut().expect("inner");
                            let device = &inner.vk.device;
                            if let Some(pool) = platform.ops_command_pool_handle() {
                                unsafe { device.free_command_buffers(pool, &[cb]) };
                            }
                        }
                        rollback_pre_submit(store, &mut open_frame);
                        platform.renderer_failed = true;
                        let inner_post = self.inner.as_mut().expect("inner");
                        rollback_atlas(
                            inner_post,
                            open_frame.layouts.atlas,
                            open_frame.atlas_prev_ticket_snapshot.clone(),
                        );
                        for r in open_frame.pins.retired_resources.drain(..) {
                            r.release(&inner_post.vk);
                        }
                        if inner_post.pending_frame_close_events.len() < 1024 {
                            inner_post.pending_frame_close_events.push(
                                super::frame_builder::FrameCloseEvent {
                                    reason,
                                    ops_in_frame: open_frame.ops.len(),
                                    glyph_uploads_in_frame: open_frame.glyph_uploads_in_frame,
                                    renders_in_frame,
                                    pin_count: open_frame.pins.len(),
                                    aborted: true,
                                },
                            );
                        }
                        inner_post.frame_builder.complete_close_failure();
                        return Err(RenderError::Vk(e));
                    }
                }
            }
        };
        let completion_semaphore = completion_signal
            .as_ref()
            .map(PresentCompletionSignal::semaphore);

        // End CB + append to SubmitGroup. Does NOT vkQueueSubmit2 yet.
        // Uses end_and_submit_op_with_signal so the completion semaphore
        // (if any) is queued on the submit's signal list.
        let append_result = {
            let inner = self.inner.as_mut().expect("inner");
            end_and_submit_op_with_signal(inner, platform, cb, &frame_ticket, completion_semaphore)
        };
        if let Err(e) = append_result {
            {
                let inner = self.inner.as_mut().expect("inner");
                let device = &inner.vk.device;
                if let Some(pool) = platform.ops_command_pool_handle() {
                    unsafe { device.free_command_buffers(pool, &[cb]) };
                }
            }
            rollback_pre_submit(store, &mut open_frame);
            platform.renderer_failed = true;
            let inner_post = self.inner.as_mut().expect("inner");
            rollback_atlas(
                inner_post,
                open_frame.layouts.atlas,
                open_frame.atlas_prev_ticket_snapshot.clone(),
            );
            // Phase B.2 Mechanism 3 (defensive): release any retired
            // BatchResources attached to the open frame's pin set.
            // See path 1 above for rationale.
            for r in open_frame.pins.retired_resources.drain(..) {
                r.release(&inner_post.vk);
            }
            if inner_post.pending_frame_close_events.len() < 1024 {
                inner_post
                    .pending_frame_close_events
                    .push(super::frame_builder::FrameCloseEvent {
                        reason,
                        ops_in_frame: open_frame.ops.len(),
                        glyph_uploads_in_frame: open_frame.glyph_uploads_in_frame,
                        renders_in_frame,
                        pin_count: open_frame.pins.len(),
                        aborted: true,
                    });
            }
            inner_post.frame_builder.complete_close_failure();
            // completion_signal drops with the local — submit never
            // queued the signal-op so the fd would never fire.
            return Err(e);
        }

        // Phase B.3 (N8): collect every self-overlap scratch from the recorded
        // ops into a local Vec — the SubmittedOp will own them through fence
        // retire. std::mem::take leaves the ops in place with `None` for the
        // scratch slot (idempotent if the op never carried one). Done BEFORE
        // flush_submit_group so close-failure drops the local on the stack
        // (ScratchImage::Drop destroys Vk handles cleanly — no fence ticket
        // exists yet at this point).
        let frame_scratches: Vec<ScratchImage> = open_frame
            .ops
            .iter_mut()
            .filter_map(|op| match op {
                super::frame_builder::RecordedOp::CopyArea(ca) => ca.self_overlap_scratch.take(),
                _ => None,
            })
            .collect();

        // Park a SubmittedOp into pending_group_ops.
        //
        // Phase B.2 Mechanism 2: consume the frame's captured-at-open
        // `frame_generation` instead of bumping at close. Every
        // descriptor acquisition that ran during the open frame
        // tagged the descriptor pool with this same value, so the
        // retire walk's `release_up_to(op.generation)` retires
        // exactly the frame's pools (and no others).
        {
            let inner = self.inner.as_mut().expect("inner");
            let generation = open_frame.frame_generation;
            inner.pending_group_ops.push(SubmittedOp {
                cb,
                ticket: frame_ticket.clone(),
                staging: None,
                scratch: frame_scratches, // NEW (B.3 N8)
                atlas_ticket: None,
                generation,
                retired_resources: Vec::new(),
            });
        }

        // Drive the actual vkQueueSubmit2 via engine's flush_submit_group wrapper.
        let flush_outcome = self.flush_submit_group(
            store,
            platform,
            super::submit_group::FlushReason::FrameBuilder,
        );

        match flush_outcome {
            Ok(_) => {
                // Commit-after-Ok.
                let op_count = open_frame.ops.len();
                let glyph_uploads = open_frame.glyph_uploads_in_frame;
                let pin_count = open_frame.pins.len();
                {
                    let inner = self.inner.as_mut().expect("inner");
                    // Phase B.3 (N10) branch (b) POST-FLUSH SUCCESS: drain
                    // pending_present_completions into a PendingPresentBatch
                    // alongside the exported sync_file fd from the signal
                    // we queued on the submit. The batch keeps the signal
                    // alive until the fd fires.
                    let drained_completions: Vec<super::present_completion::PendingPresentEntry> =
                        std::mem::take(&mut open_frame.pending_present_completions);
                    if !drained_completions.is_empty() {
                        let (wait, signal) = match completion_signal {
                            Some(signal) => match signal.export_sync_file_fd() {
                                Ok(Some(fd)) => (PresentBatchWait::Fd(fd), Some(signal)),
                                Ok(None) => (PresentBatchWait::Ready, Some(signal)),
                                Err(e) => {
                                    log::warn!(
                                        "B.3 close_open_frame: vkGetSemaphoreFdKHR(SYNC_FD) \
                                         failed: {e:?}; falling back to FenceTicket polling"
                                    );
                                    (PresentBatchWait::Poll, Some(signal))
                                }
                            },
                            None => {
                                // Non-empty completions but no signal — only possible if
                                // the pending_count check above returned 0 but something
                                // was pushed between the check and here. Treat as Ready.
                                (PresentBatchWait::Ready, None)
                            }
                        };
                        let ticket = inner.submitted.back().map(|op| op.ticket.clone());
                        inner.pending_present_batches.push(PendingPresentBatch {
                            wait,
                            ticket,
                            signal,
                            events: drained_completions,
                        });
                    }
                    // B.3 hotfix 2: adopt gradient Arc clones from every
                    // RenderTrapsOrTris op into pins.retired_resources
                    // BEFORE taking the pins. This keeps the GradientPicture
                    // Arc alive in FrameSubmittedRecord until the GPU fence
                    // fires — otherwise the recorded-op clone would drop with
                    // open_frame at the end of this function while the GPU CB
                    // is still in flight.
                    for op in &open_frame.ops {
                        if let super::frame_builder::RecordedOp::RenderTrapsOrTris(rt) = op
                            && let super::frame_builder::RecordedTrapSrcKind::Gradient {
                                ref picture,
                                ..
                            } = rt.src_kind
                        {
                            open_frame.pins.adopt_retired(Box::new(picture.clone())
                                as Box<dyn crate::kms::v2::batch_resource::BatchResource>);
                        }
                    }
                    inner
                        .pending_frames
                        .push_back(super::frame_builder::FrameSubmittedRecord {
                            ticket: frame_ticket.clone(),
                            pins: std::mem::take(&mut open_frame.pins),
                            frame_seq,
                        });
                    commit_close_success(
                        inner,
                        store,
                        std::mem::take(&mut open_frame.layouts),
                        std::mem::take(&mut open_frame.touched),
                        std::mem::take(&mut open_frame.pending_glyph_inserts),
                        &frame_ticket,
                    );
                    if inner.pending_frame_close_events.len() < 1024 {
                        inner.pending_frame_close_events.push(
                            super::frame_builder::FrameCloseEvent {
                                reason,
                                ops_in_frame: op_count,
                                glyph_uploads_in_frame: glyph_uploads,
                                renders_in_frame,
                                pin_count,
                                aborted: false,
                            },
                        );
                    }
                    inner.frame_builder.complete_close_success();
                }
                Ok(super::frame_builder::CloseOutcome::Submitted {
                    frame_seq,
                    op_count,
                    pin_count,
                    ticket: frame_ticket,
                    reason,
                })
            }
            Err(e) => {
                // Platform's abort_flush already freed CBs + set renderer_failed.
                rollback_pre_submit(store, &mut open_frame);
                let atlas_overlay = open_frame.layouts.atlas;
                let atlas_prev = open_frame.atlas_prev_ticket_snapshot.clone();
                let ops_in_frame = open_frame.ops.len();
                let glyph_uploads_in_frame = open_frame.glyph_uploads_in_frame;
                let pin_count = open_frame.pins.len();
                let inner = self.inner.as_mut().expect("inner");
                // Phase B.3 (N10) branch (c) POST-FLUSH FAILURE: force-enqueue
                // a degraded PendingPresentBatch BEFORE returning Err.
                // Never silent-drop — X PRESENT protocol observes events regardless
                // of submit success (Pitfall 8). The completion_signal drops with
                // the local; the failed submit never queued a signal-op so the fd
                // would never fire anyway.
                let drained_completions: Vec<super::present_completion::PendingPresentEntry> =
                    std::mem::take(&mut open_frame.pending_present_completions);
                if !drained_completions.is_empty() {
                    inner.pending_present_batches.push(PendingPresentBatch {
                        wait: PresentBatchWait::Ready,
                        ticket: None,
                        signal: None,
                        events: drained_completions,
                    });
                }
                rollback_atlas(inner, atlas_overlay, atlas_prev);
                // Phase B.2 Mechanism 3 (defensive): release any
                // retired BatchResources attached to the open frame's
                // pin set. See path 1 above for rationale.
                for r in open_frame.pins.retired_resources.drain(..) {
                    r.release(&inner.vk);
                }
                if inner.pending_frame_close_events.len() < 1024 {
                    inner
                        .pending_frame_close_events
                        .push(super::frame_builder::FrameCloseEvent {
                            reason,
                            ops_in_frame,
                            glyph_uploads_in_frame,
                            renders_in_frame,
                            pin_count,
                            aborted: true,
                        });
                }
                inner.frame_builder.complete_close_failure();
                Err(RenderError::Vk(e))
            }
        }
    }

    /// Phase B Invariant M2: close the open frame (if any) BEFORE a
    /// non-ported paint op records its own CB. The non-ported op
    /// samples committed `Drawable::storage.current_layout` and
    /// `last_render_ticket`; without the close, it would race against
    /// the deferred frame on the GPU. Retires when every paint op is
    /// ported (end of sub-phase B.3 at the latest).
    ///
    /// Fast path: no frame open → no-op. Preserves existing
    /// batch-coalescing discipline in `render_composite`,
    /// `cow_copy_area`, etc.
    ///
    /// Slow path: frame open → flush pre-existing batches first
    /// (chronological ordering: pre-frame batches must submit before
    /// the frame's CB), then close the frame. Each non-ported op's
    /// own batch prelude runs afterward against an empty batch state.
    pub(crate) fn close_open_frame_for_non_ported_op(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
    ) -> Result<(), RenderError> {
        let frame_open = self
            .inner
            .as_ref()
            .is_some_and(|i| i.frame_builder.is_open());
        if !frame_open {
            return Ok(());
        }
        self.flush_render_batch(store, platform)?;
        match self.close_open_frame(
            store,
            platform,
            super::frame_builder::CloseReason::NonPortedPaintOp,
        )? {
            super::frame_builder::CloseOutcome::Submitted { .. }
            | super::frame_builder::CloseOutcome::AlreadyClosed => Ok(()),
        }
    }

    /// Phase B.1 close trigger 4: close the open frame if its open
    /// duration has exceeded the cached timeout. No-op if no frame
    /// open or below threshold. Called by `maybe_composite` at the
    /// top of every tick.
    pub(crate) fn close_open_frame_if_timed_out(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
    ) -> Result<(), RenderError> {
        let timed_out = self
            .inner
            .as_ref()
            .is_some_and(|i| i.frame_builder.open_for_at_least(i.frame_builder_timeout));
        if !timed_out {
            return Ok(());
        }
        match self.close_open_frame(store, platform, super::frame_builder::CloseReason::Timeout)? {
            super::frame_builder::CloseOutcome::Submitted { .. }
            | super::frame_builder::CloseOutcome::AlreadyClosed => Ok(()),
        }
    }

    /// Count of in-flight submits awaiting retirement. Tests use
    /// this to assert the lifecycle book-keeping.
    pub(crate) fn pending_count(&self) -> usize {
        self.inner.as_ref().map(|i| i.submitted.len()).unwrap_or(0)
    }

    /// Phase A: count of ops parked in pending_group_ops (not yet
    /// committed to `submitted`). Test helper — also used by the
    /// backend wrapper exposed to v2_acceptance integration tests.
    pub(crate) fn pending_group_ops_count_for_tests(&self) -> usize {
        self.inner.as_ref().map_or(0, |i| i.pending_group_ops.len())
    }

    /// Phase B.3 (N8) test helper: scratch vec length of the most recently
    /// submitted op. Used by `b3_close_path_scratch_walk_yields_empty_for_no_copy_area_frames`
    /// integration test to verify the close-path walk's Vec<ScratchImage>.
    pub(crate) fn most_recent_submitted_op_scratch_len_for_tests(&self) -> usize {
        self.inner
            .as_ref()
            .and_then(|i| i.pending_group_ops.last().or_else(|| i.submitted.back()))
            .map_or(0, |op| op.scratch.len())
    }

    /// Phase B.1 Task 15: test introspection — is the frame builder
    /// currently open?
    pub(crate) fn frame_builder_is_open(&self) -> bool {
        self.inner
            .as_ref()
            .is_some_and(|i| i.frame_builder.is_open())
    }

    /// Phase B.1 Task 15: test introspection — lifetime count of
    /// `FrameBuilder` closes.
    pub(crate) fn frame_builder_lifetime_closes(&self) -> u64 {
        self.inner
            .as_ref()
            .map_or(0, |i| i.frame_builder.lifetime_closes())
    }

    /// Phase B.2 Task 11: test introspection — walk the open frame's
    /// recorded op list and return each
    /// `RecordedOp::RenderComposite`'s `dst_old_layout` in append
    /// order. Returns an empty vec if no frame is open. Used by the
    /// second-op-in-frame overlay test (see
    /// `KmsBackendV2::frame_builder_peek_render_composite_dst_old_layouts_for_tests`).
    /// The `RecordedRenderComposite` payload is `pub(crate)` so the
    /// integration crate cannot match on it directly — this returns
    /// the minimum scalar needed for the assertion.
    pub(crate) fn frame_builder_peek_render_composite_dst_old_layouts(
        &self,
    ) -> Vec<vk::ImageLayout> {
        let Some(inner) = self.inner.as_ref() else {
            return Vec::new();
        };
        let Some(open) = inner.frame_builder.open.as_ref() else {
            return Vec::new();
        };
        open.ops
            .iter()
            .filter_map(|op| match op {
                super::frame_builder::RecordedOp::RenderComposite(rc) => Some(rc.dst_old_layout),
                _ => None,
            })
            .collect()
    }

    /// Phase B.1 Task 21: monotonic count of all `FrameBuilder` opens
    /// since init. Delta-tracked by `KmsBackendV2::drain_frame_builder_telemetry`
    /// to emit one `record_frame_builder_open` per new open.
    pub(crate) fn frame_builder_lifetime_opens(&self) -> u64 {
        self.inner
            .as_ref()
            .map_or(0, |i| i.frame_builder.lifetime_opens())
    }

    /// Phase B.1 Task 15: test introspection — monotonic `frame_seq`
    /// counter. Bumped by `close_open_frame` on every successful close.
    pub(crate) fn engine_frame_seq(&self) -> u64 {
        self.inner.as_ref().map_or(0, |i| i.frame_seq)
    }

    /// Phase A T9: CB handles of ops parked in `pending_group_ops`
    /// in append order. Used by ordering-invariant tests that need
    /// to match a CB handle observed during recording against the
    /// handle visible in the SubmitGroup's `peek_entries` slice.
    #[cfg(test)]
    pub(crate) fn pending_group_ops_cbs_for_tests(&self) -> Vec<vk::CommandBuffer> {
        self.inner.as_ref().map_or_else(Vec::new, |i| {
            i.pending_group_ops.iter().map(|op| op.cb).collect()
        })
    }

    /// True if either the frame builder has an open frame OR a render-
    /// composite coalescing batch is currently open (CB recorded but
    /// not yet submitted). Used by the eager-touch regression tests and by
    /// `KmsBackendV2::has_pending_batches_for_tests` (the wrapper
    /// the v2_acceptance test asserts on).
    ///
    /// Phase B.3 (N10): the frame builder's open frame is the
    /// equivalent of "pending COW work" after the cow-batch deletion.
    pub fn has_pending_batches_for_tests(&self) -> bool {
        self.inner
            .as_ref()
            .is_some_and(|i| i.frame_builder.is_open() || i.pending_render_batch.is_some())
    }

    /// Stage 5 Task 4 layer 1: lifetime count of `vkCreateDescriptorPool`
    /// calls inside the ring. Backend polls this and bumps telemetry.
    pub(crate) fn descriptor_pool_creates_lifetime(&self) -> u64 {
        self.inner
            .as_ref()
            .map_or(0, |i| i.descriptor_pool_ring.lifetime_creates())
    }

    /// Stage 5 Task 4 layer 1: lifetime count of successful
    /// `vkResetDescriptorPool` calls inside the ring.
    pub(crate) fn descriptor_pool_resets_lifetime(&self) -> u64 {
        self.inner
            .as_ref()
            .map_or(0, |i| i.descriptor_pool_ring.lifetime_resets())
    }

    /// Stage 5 Task 4 layer 1: ring residency for the acceptance
    /// gate (`v2_render_composite_pool_creates_bounded_after_warmup`).
    pub(crate) fn descriptor_pool_ring_pool_count(&self) -> usize {
        self.inner
            .as_ref()
            .map_or(0, |i| i.descriptor_pool_ring.pool_count())
    }

    /// Phase B.2 Task 3 test introspection: maximum `high_water_generation`
    /// across the descriptor pool ring's resident pools. The Mechanism 2
    /// integration test reads this to assert that every `acquire_set`
    /// during an open frame tags the active pool with the frame's
    /// captured `frame_generation`.
    pub(crate) fn descriptor_pool_ring_high_water_generation(&self) -> u64 {
        self.inner
            .as_ref()
            .map_or(0, |i| i.descriptor_pool_ring.max_high_water_generation())
    }

    /// Phase B.2 Task 3 test introspection: set `acquire_generation`
    /// directly. The Mechanism 2 integration test uses this to seed
    /// a known baseline before opening a frame so the assertions on
    /// the captured `frame_generation` are deterministic.
    pub(crate) fn set_acquire_generation_for_tests(&mut self, value: u64) {
        if let Some(inner) = self.inner.as_mut() {
            inner.acquire_generation = value;
        }
    }

    /// Phase B.2 Task 3 test introspection: read the open frame's
    /// captured `frame_generation`. Returns `None` if no frame is
    /// open. Used by the Mechanism 2 watermark integration test to
    /// confirm the open-time bump landed.
    pub(crate) fn open_frame_generation(&self) -> Option<u64> {
        self.inner
            .as_ref()
            .and_then(|i| i.frame_builder.open.as_ref().map(|o| o.frame_generation))
    }

    /// Phase B.2 Task 3 test introspection: drive the engine's
    /// frame-builder open path. Bumps `acquire_generation` and calls
    /// `FrameBuilder::open_for_paint(ticket, frame_generation)` —
    /// the same shape the production callers use. Used by the
    /// Mechanism 2 integration test to exercise the watermark
    /// without going through a real paint op.
    pub(crate) fn open_frame_for_paint_for_tests(&mut self, ticket: FenceTicket) {
        let Some(inner) = self.inner.as_mut() else {
            return;
        };
        debug_assert!(
            !inner.frame_builder.is_open(),
            "open_frame_for_paint_for_tests while a frame is open"
        );
        inner.acquire_generation = inner.acquire_generation.saturating_add(1);
        let frame_generation = inner.acquire_generation;
        inner.frame_builder.open_for_paint(ticket, frame_generation);
    }

    /// Phase B.2 Task 3 test introspection: invoke
    /// `RenderEngineInner::acquire_descriptor_set_for_frame_or_op`
    /// with a caller-supplied layout. Returns the raw Vk handle.
    /// The Mechanism 2 integration test uses this to assert the
    /// helper's behavior (uses `open.frame_generation` when a frame
    /// is open; bumps `acquire_generation` otherwise).
    pub(crate) fn acquire_descriptor_set_for_frame_or_op_for_tests(
        &mut self,
        layout: vk::DescriptorSetLayout,
    ) -> Result<vk::DescriptorSet, vk::Result> {
        let inner = self
            .inner
            .as_mut()
            .ok_or(vk::Result::ERROR_INITIALIZATION_FAILED)?;
        inner.acquire_descriptor_set_for_frame_or_op(layout)
    }

    /// Phase B.2 Task 3 test introspection: close the open frame
    /// with `CloseReason::Timeout`. Mirrors
    /// `close_open_frame_if_timed_out` but unconditionally closes
    /// (so the test doesn't have to wait for the wall-clock timeout
    /// to elapse).
    pub(crate) fn close_open_frame_for_timeout_for_tests(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
    ) -> Result<(), RenderError> {
        if !self
            .inner
            .as_ref()
            .is_some_and(|i| i.frame_builder.is_open())
        {
            return Ok(());
        }
        match self.close_open_frame(store, platform, super::frame_builder::CloseReason::Timeout)? {
            super::frame_builder::CloseOutcome::Submitted { .. }
            | super::frame_builder::CloseOutcome::AlreadyClosed => Ok(()),
        }
    }

    /// Phase A Task 3.5: drain all queued `FlushOutcome` records
    /// accumulated since the last drain. Backend calls this once
    /// per `maybe_composite` tick to route outcomes to telemetry.
    pub(crate) fn drain_flush_outcomes(&mut self) -> Vec<super::platform::FlushOutcome> {
        self.inner
            .as_mut()
            .map(|i| std::mem::take(&mut i.pending_flush_outcomes))
            .unwrap_or_default()
    }

    /// Phase A Task 3.5: total active staging + scratch bytes
    /// across both submitted (in-flight) and parked (pending_group)
    /// ops. Used for per-tick high-water sampling. Returns
    /// `(staging_bytes, scratch_bytes)`.
    pub(crate) fn active_resource_bytes(&self) -> (u64, u64) {
        let Some(inner) = self.inner.as_ref() else {
            return (0, 0);
        };
        let staging_submitted: u64 = inner
            .submitted
            .iter()
            .map(|op| op.staging.as_ref().map_or(0, |s| s.size))
            .sum();
        let staging_parked: u64 = inner
            .pending_group_ops
            .iter()
            .map(|op| op.staging.as_ref().map_or(0, |s| s.size))
            .sum();
        let scratch_submitted: u64 = inner
            .submitted
            .iter()
            .map(|op| op.scratch.iter().map(|s| s.size_bytes()).sum::<u64>())
            .sum();
        let scratch_parked: u64 = inner
            .pending_group_ops
            .iter()
            .map(|op| op.scratch.iter().map(|s| s.size_bytes()).sum::<u64>())
            .sum();
        (
            staging_submitted + staging_parked,
            scratch_submitted + scratch_parked,
        )
    }

    /// Task 3 test helper: allocate a pixmap drawable in `store` backed
    /// by a real Vk storage. Returns the `DrawableId`.
    #[cfg(test)]
    pub(crate) fn create_pixmap(
        &self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        xid: u32,
        w: u16,
        h: u16,
        depth: u8,
    ) -> Result<DrawableId, RenderError> {
        let storage = platform
            .allocate_drawable_storage(w, h, depth)
            .map_err(RenderError::Vk)?;
        store
            .allocate(
                xid,
                super::store::DrawableKind::Pixmap,
                depth,
                false,
                storage,
            )
            .map_err(|_| RenderError::NoVk)
    }

    /// Stage 3b + B.2 fix + B.3 hotfix 2: drop the engine's
    /// `picture_paint` entry for `host_pic`. Called by
    /// `KmsBackendV2::render_free_picture` after removing the picture
    /// record from `KmsCore.pictures`.
    ///
    /// **B.2 fix**: routes the `GradientPicture` through
    /// `adopt_retired_resource_for_gpu_retirement`. The engine's
    /// HashMap clone is an Arc clone; `BatchResource::release` drops
    /// it (decrements the Arc). If a recorded deferred op holds
    /// another clone (B.3 hotfix 2 path), the Vk handles stay alive
    /// until BOTH clones drop — after the GPU fence fires.
    ///
    /// **B.3 hotfix 2**: `GradientPicture` is now `Arc`-backed; the
    /// `picture_paint_remove` drop here is safe regardless of any
    /// in-flight recorded ops holding their own clones.
    ///
    /// `SolidFill` variants carry no Vk handles — HashMap::remove
    /// drop with no fence gating needed.
    pub(crate) fn picture_paint_remove(&mut self, host_pic: u32) {
        let Some(inner) = self.inner.as_mut() else {
            return;
        };
        let Some(state) = inner.picture_paint.remove(&host_pic) else {
            return;
        };
        match state {
            PicturePaintState::Gradient(gradient) => {
                inner
                    .adopt_retired_resource_for_gpu_retirement(Some(Box::new(gradient)
                        as Box<dyn crate::kms::v2::batch_resource::BatchResource>));
            }
        }
    }

    /// Stage 3f.13: build the LUT for a `RenderCreateLinearGradient`
    /// picture and stash it on the engine's `picture_paint` map.
    /// Subsequent `render_composite` calls referencing `host_pic`
    /// as src or mask sample this LUT instead of falling back to
    /// the 3f.12 first-stop SolidFill collapse.
    ///
    /// # Errors
    ///
    /// Returns `NoVk` on the test fixture; `Vk` if the LUT image /
    /// view / memory allocation fails.
    pub(crate) fn build_and_insert_linear_gradient(
        &mut self,
        platform: &PlatformBackend,
        host_pic: u32,
        p1: (i32, i32),
        p2: (i32, i32),
        stops: &[crate::kms::vk::gradient::Stop],
    ) -> Result<(), RenderError> {
        let inner = self.inner.as_mut().ok_or(RenderError::NoVk)?;
        let pool = platform
            .ops_command_pool_handle()
            .ok_or(RenderError::NoVk)?;
        let gradient = crate::kms::vk::gradient::GradientPicture::new_linear(
            inner.vk.clone(),
            pool,
            p1,
            p2,
            stops,
        )
        .map_err(|e| match e {
            crate::kms::vk::gradient::GradientError::Vk(r) => RenderError::Vk(r),
            crate::kms::vk::gradient::GradientError::NoMemoryType => {
                RenderError::Vk(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY)
            }
        })?;
        inner
            .picture_paint
            .insert(host_pic, PicturePaintState::Gradient(gradient));
        Ok(())
    }

    /// Stage 3f.13: radial-gradient companion of
    /// [`build_and_insert_linear_gradient`]. Sizes the LUT image
    /// at `RADIAL_SIDE × RADIAL_SIDE` and renders the two-circle
    /// radial CPU-side, then uploads.
    ///
    /// # Errors
    ///
    /// Returns `NoVk` on the test fixture; `Vk` on allocation
    /// failure.
    pub(crate) fn build_and_insert_radial_gradient(
        &mut self,
        platform: &PlatformBackend,
        host_pic: u32,
        inner_circle: (i32, i32, i32),
        outer_circle: (i32, i32, i32),
        stops: &[crate::kms::vk::gradient::Stop],
    ) -> Result<(), RenderError> {
        let inner = self.inner.as_mut().ok_or(RenderError::NoVk)?;
        let pool = platform
            .ops_command_pool_handle()
            .ok_or(RenderError::NoVk)?;
        let gradient = crate::kms::vk::gradient::GradientPicture::new_radial(
            inner.vk.clone(),
            pool,
            inner_circle,
            outer_circle,
            stops,
        )
        .map_err(|e| match e {
            crate::kms::vk::gradient::GradientError::Vk(r) => RenderError::Vk(r),
            crate::kms::vk::gradient::GradientError::NoMemoryType => {
                RenderError::Vk(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY)
            }
        })?;
        inner
            .picture_paint
            .insert(host_pic, PicturePaintState::Gradient(gradient));
        Ok(())
    }

    /// Stage 3b test helper: how many picture-paint entries are
    /// currently tracked. Used to assert that
    /// `render_free_picture` drops its slot.
    #[cfg(test)]
    pub(crate) fn picture_paint_len(&self) -> usize {
        self.inner.as_ref().map_or(0, |i| i.picture_paint.len())
    }

    /// Stage 3c: how many cached drawable views the engine
    /// currently holds. Test-only — used to assert eviction on
    /// drawable retire. Also exposed to integration tests via
    /// `KmsBackendV2::drawable_view_cache_len` — not gated on
    /// `cfg(test)` because `tests/` integration crates compile
    /// against the regular lib build, not the `--cfg test` one.
    pub(crate) fn drawable_view_cache_len(&self) -> usize {
        self.inner
            .as_ref()
            .map_or(0, |i| i.drawable_view_cache.len())
    }

    /// Stage 3c: whether the lazy-built RENDER pipeline cache has
    /// been constructed. Test-only — used to assert the lazy
    /// build trigger.
    #[cfg(test)]
    pub(crate) fn render_pipelines_built(&self) -> bool {
        self.inner
            .as_ref()
            .is_some_and(|i| i.render_pipelines.is_some())
    }

    /// Stage 3c: lazy-initialize RENDER paint assets — pipeline
    /// cache + 1×1 SolidFill / SolidMask / WhiteMask scratches +
    /// `DstReadback`. Idempotent. Called by `render_composite`
    /// and `render_fill_rectangles` on first paint; v1 builds
    /// these eagerly at backend construction, but the v2 engine
    /// is constructed before its first composite request so
    /// paying the cost on first use (typically warmup) is fine.
    ///
    /// The `white_mask_image` requires a one-shot clear-to-white
    /// CB to seed its texel; the recorded clear synchronously
    /// drains via `run_one_shot_op` so the texel is present
    /// before the first sample.
    ///
    /// # Errors
    ///
    /// - `NoVk` on the stub engine.
    /// - `Vk(...)` for any underlying Vk failure during pipeline
    ///   cache / scratch image / readback construction or the
    ///   one-shot white-clear submit.
    pub(crate) fn ensure_render_assets(
        &mut self,
        platform: &PlatformBackend,
    ) -> Result<(), RenderError> {
        use crate::kms::vk::render_pipeline::record_solid_color_clear;

        let Some(inner) = self.inner.as_mut() else {
            return Err(RenderError::NoVk);
        };
        if platform.renderer_failed {
            return Err(RenderError::RendererFailed);
        }

        if inner.render_pipelines.is_none() {
            let cache = RenderPipelineCache::new(Arc::clone(&inner.vk)).map_err(|e| {
                log::error!("v2 ensure_render_assets: RenderPipelineCache::new failed: {e:?}");
                RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
            })?;
            inner.render_pipelines = Some(cache);
        }
        // B.2 fix (vkdebug VUID-vkCmdDraw-None-09600): the per-op
        // `record_solid_color_clear` emits a barrier with
        // `old_layout = solid.current_layout()`. Validation tracks
        // layouts across CB boundaries: if the image is still in
        // UNDEFINED globally (never transitioned via any submitted CB),
        // the expectation that the barrier consumes "the layout
        // recorded at descriptor-write time" fails when the descriptor
        // declares SHADER_READ_ONLY. The white-mask path below already
        // seeded its image to SHADER_READ_ONLY via a one-shot clear;
        // mirror that for solid_src/solid_mask. Cost: two extra
        // synchronous submits at engine init.
        let pool_for_init_clears = platform.ops_command_pool_handle().ok_or_else(|| {
            log::error!("v2 ensure_render_assets: no ops_command_pool for solid-image init clears");
            RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
        })?;
        if inner.solid_src_image.is_none() {
            let mut s = SolidColorImage::new(Arc::clone(&inner.vk)).map_err(|e| {
                log::error!("v2 ensure_render_assets: solid_src SolidColorImage failed: {e:?}");
                RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
            })?;
            crate::kms::vk::ops::run_one_shot_op(&inner.vk, pool_for_init_clears, |vk, cb| {
                record_solid_color_clear(vk, cb, &mut s, [0.0, 0.0, 0.0, 0.0]);
                Ok(())
            })
            .map_err(|e| {
                log::error!("v2 ensure_render_assets: solid_src init-clear submit failed: {e:?}");
                RenderError::Vk(e)
            })?;
            log::info!(
                "v2 ensure_render_assets: solid_src_image image={:?} view={:?}",
                s.image(),
                s.image_view(),
            );
            inner.solid_src_image = Some(s);
        }
        if inner.solid_mask_image.is_none() {
            let mut s = SolidColorImage::new(Arc::clone(&inner.vk)).map_err(|e| {
                log::error!("v2 ensure_render_assets: solid_mask SolidColorImage failed: {e:?}");
                RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
            })?;
            crate::kms::vk::ops::run_one_shot_op(&inner.vk, pool_for_init_clears, |vk, cb| {
                record_solid_color_clear(vk, cb, &mut s, [0.0, 0.0, 0.0, 0.0]);
                Ok(())
            })
            .map_err(|e| {
                log::error!("v2 ensure_render_assets: solid_mask init-clear submit failed: {e:?}");
                RenderError::Vk(e)
            })?;
            log::info!(
                "v2 ensure_render_assets: solid_mask_image image={:?} view={:?}",
                s.image(),
                s.image_view(),
            );
            inner.solid_mask_image = Some(s);
        }
        if inner.white_mask_image.is_none() {
            let mut s = SolidColorImage::new(Arc::clone(&inner.vk)).map_err(|e| {
                log::error!("v2 ensure_render_assets: white_mask SolidColorImage failed: {e:?}");
                RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
            })?;
            crate::kms::vk::ops::run_one_shot_op(&inner.vk, pool_for_init_clears, |vk, cb| {
                record_solid_color_clear(vk, cb, &mut s, [1.0, 1.0, 1.0, 1.0]);
                Ok(())
            })
            .map_err(|e| {
                log::error!("v2 ensure_render_assets: white-clear submit failed: {e:?}");
                RenderError::Vk(e)
            })?;
            log::info!(
                "v2 ensure_render_assets: white_mask_image image={:?} view={:?}",
                s.image(),
                s.image_view(),
            );
            inner.white_mask_image = Some(s);
        }
        if inner.dst_readback.is_none() {
            inner.dst_readback = Some(DstReadback::new(Arc::clone(&inner.vk)));
        }
        if inner.src_alias_readback.is_none() {
            inner.src_alias_readback = Some(DstReadback::new(Arc::clone(&inner.vk)));
        }
        Ok(())
    }

    /// Stage 3e.2: lazy-init trap pipeline + mask scratch. Idempotent.
    /// Called by `render_traps_or_tris` on first use. The mask
    /// scratch starts at the default extent and grows via
    /// `ensure_image_size_returning_old` per call; the pipeline is
    /// built once at the standard R8_UNORM mask format.
    ///
    /// # Errors
    ///
    /// - `NoVk` on the stub engine.
    /// - `Vk(...)` for pipeline / scratch construction failure.
    fn ensure_trap_assets(&mut self, platform: &PlatformBackend) -> Result<(), RenderError> {
        use crate::kms::vk::{mask_scratch::MaskScratch, trap_pipeline::TrapPipeline};
        let Some(inner) = self.inner.as_mut() else {
            return Err(RenderError::NoVk);
        };
        if platform.renderer_failed {
            return Err(RenderError::RendererFailed);
        }
        if inner.trap_pipeline.is_none() {
            let p =
                TrapPipeline::new(Arc::clone(&inner.vk), vk::Format::R8_UNORM).map_err(|e| {
                    log::error!("v2 ensure_trap_assets: TrapPipeline::new failed: {e:?}");
                    RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
                })?;
            inner.trap_pipeline = Some(p);
        }
        if inner.mask_scratch.is_none() {
            let s = MaskScratch::new(Arc::clone(&inner.vk)).map_err(|e| {
                log::error!("v2 ensure_trap_assets: MaskScratch::new failed: {e:?}");
                RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
            })?;
            inner.mask_scratch = Some(s);
        }
        Ok(())
    }

    /// Stage 3c: invalidate any cached drawable views referencing
    /// `id`. Called by `KmsBackendV2` after a drawable has actually
    /// retired (storage destroyed); evicting earlier would leave
    /// dangling Vk handles since `vk::ImageView`'s underlying image
    /// is gone.
    pub(crate) fn notify_drawable_retired(&mut self, id: DrawableId) {
        self.invalidate_drawable_views(id);
    }

    /// Drop (and `vkDestroyImageView`) every cached drawable view keyed
    /// on `id`. Shared body of [`Self::notify_drawable_retired`] (called
    /// when a drawable's storage is destroyed) and the GLX-TFP promotion
    /// path (called after `Storage::adopt_exportable` swaps the backing
    /// image — the cache keys on `DrawableId` only and never re-checks
    /// the `VkImage` handle, so a swap without invalidation would keep
    /// sampling the OLD image).
    pub(crate) fn invalidate_drawable_views(&mut self, id: DrawableId) {
        let Some(inner) = self.inner.as_mut() else {
            return;
        };
        let device = &inner.vk.device;
        inner.drawable_view_cache.retain(|(d, _, _), cached| {
            if *d != id {
                return true;
            }
            unsafe {
                device.destroy_image_view(cached.view, None);
            }
            false
        });
    }

    /// GLX-TFP (Task 1.2): permanently migrate a server-owned pixmap
    /// onto dma-buf-exportable storage (glamor's model). Idempotent —
    /// returns early if the drawable is already exportable (DRI3 import
    /// or a prior promotion).
    ///
    /// Steps: (a) allocate an exportable image; (b)+(c) copy the old
    /// content into it and block until the copy completes; (d) build
    /// fresh sample + attachment views over the new image; (e) swap the
    /// `Storage` handles; (f) invalidate the drawable's cached views
    /// (the cache keys on `DrawableId` and never re-checks the
    /// `VkImage`, so a swap without this keeps sampling the old image);
    /// (g) retire the old handles once their guarding fence signals.
    ///
    /// # Errors
    ///
    /// `NoVk` (stub engine), `UnknownDrawable`, or any propagated
    /// `vk::Result` from allocation / view creation / the blocking copy.
    pub(crate) fn promote_drawable_exportable(
        &mut self,
        platform: &mut PlatformBackend,
        store: &mut DrawableStore,
        id: DrawableId,
    ) -> Result<(), RenderError> {
        if self.inner.is_none() {
            return Err(RenderError::NoVk);
        }
        // Idempotency check first — avoid the flush cost if already done.
        {
            let d = store.get(id).ok_or(RenderError::UnknownDrawable(id))?;
            if d.storage.is_exportable() {
                return Ok(());
            }
        }

        // Submit-boundary (codex review): any open frame / parked submit
        // group may hold CBs that captured cached views of the OLD image
        // but have NOT been submitted to the queue yet. The copy below
        // relies on same-queue submission ordering to read finalized
        // old-image content, and `retire_image_after` later destroys the
        // old handles + invalidated views once `last_render_ticket`
        // signals. Both are only sound if every prior user of the old
        // image is already submitted. Close the open frame and flush the
        // submit group here to establish that boundary (mirrors
        // `get_image`'s SyncWait close before its readback). After this,
        // `last_render_ticket` names the newest in-flight ticket that
        // touched the drawable.
        self.close_open_frame(store, platform, super::frame_builder::CloseReason::SyncWait)?;
        self.flush_submit_group(
            store,
            platform,
            super::submit_group::FlushReason::SyncBoundary,
        )?;

        // Metadata read (post-close: current_layout reflects any
        // transition the frame close recorded).
        let (extent, format, depth, old_layout, old_image) = {
            let d = store.get(id).ok_or(RenderError::UnknownDrawable(id))?;
            let s = &d.storage;
            (s.extent, s.format, s.depth, s.current_layout, s.image)
        };

        let vk = platform.vk().ok_or(RenderError::NoVk)?.clone();

        // (a) allocate exportable target.
        let exp =
            crate::kms::vk::target::allocate_exportable(&vk, extent.width, extent.height, format)?;

        // (b)+(c) copy old content → new image, block until complete.
        self.copy_image_blocking(platform, old_image, old_layout, exp.image, extent)?;
        let new_layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;

        // (d) build new views (same depth-aware swizzle the store uses).
        let sample_view = PlatformBackend::build_sample_view(&vk, exp.image, format, depth)?;
        let image_view = match PlatformBackend::build_attachment_view(&vk, exp.image, format) {
            Ok(v) => v,
            Err(e) => {
                unsafe { vk.device.destroy_image_view(sample_view, None) };
                return Err(e.into());
            }
        };

        // (e) swap storage, taking ownership of the exportable image's
        //     raw handles. The drawable existence check happens BEFORE
        //     `into_raw_parts` so an absent drawable can't leak the four
        //     raw handles (`into_raw_parts` disarms `ExportableImage`'s
        //     Drop, so a bail-out after it would orphan image/memory and
        //     the two views). Unreachable in the single-threaded engine,
        //     but cheap to make leak-proof.
        let retired = {
            let d = store.get_mut(id).ok_or(RenderError::UnknownDrawable(id))?;
            let (exp_image, exp_memory, exp_stride, exp_size, exp_modifier) = exp.into_raw_parts();
            d.storage.adopt_exportable(
                exp_image,
                exp_memory,
                sample_view,
                image_view,
                new_layout,
                exp_stride,
                exp_size,
                exp_modifier,
            )
        };

        // (f) invalidate the view cache for this DrawableId.
        self.invalidate_drawable_views(id);

        // (g) retire old handles once the old image's last render fence
        //     signals (clone the ticket; None → retire eagerly).
        let guard = store.get(id).and_then(|d| d.last_render_ticket.clone());
        self.retire_image_after(retired, guard);
        Ok(())
    }

    /// GLX-TFP (Task 1.2): blocking old→new image copy used by the
    /// promotion path. Records, on a dedicated one-shot CB + fence
    /// (`run_one_shot_op` waits for the fence before returning):
    ///   - `src`: `src_layout` → `TRANSFER_SRC_OPTIMAL`
    ///   - `dst`: `UNDEFINED` → `TRANSFER_DST_OPTIMAL`
    ///   - `vkCmdCopyImage` (full `extent`, COLOR, 1 mip / 1 layer)
    ///   - `dst`: `TRANSFER_DST_OPTIMAL` → `SHADER_READ_ONLY_OPTIMAL`
    ///
    /// Promotion is rare (once per pixmap, on first GLX bind), so a
    /// dedicated fence wait is acceptable.
    ///
    /// # Errors
    ///
    /// `NoVk`, or any propagated `vk::Result` from CB recording / submit
    /// / fence wait.
    fn copy_image_blocking(
        &mut self,
        platform: &PlatformBackend,
        src: vk::Image,
        src_layout: vk::ImageLayout,
        dst: vk::Image,
        extent: vk::Extent2D,
    ) -> Result<(), RenderError> {
        let inner = self.inner.as_ref().ok_or(RenderError::NoVk)?;
        let vk = Arc::clone(&inner.vk);
        let pool = platform
            .ops_command_pool_handle()
            .ok_or(RenderError::NoVk)?;

        crate::kms::vk::ops::run_one_shot_op(&vk, pool, |vk, cb| {
            let full_range = vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .level_count(1)
                .layer_count(1);

            let pre = [
                // src → TRANSFER_SRC_OPTIMAL
                vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                    .src_access_mask(vk::AccessFlags2::MEMORY_WRITE)
                    .dst_stage_mask(vk::PipelineStageFlags2::COPY)
                    .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
                    .old_layout(src_layout)
                    .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                    .image(src)
                    .subresource_range(full_range),
                // dst (UNDEFINED) → TRANSFER_DST_OPTIMAL
                vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                    .src_access_mask(vk::AccessFlags2::empty())
                    .dst_stage_mask(vk::PipelineStageFlags2::COPY)
                    .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                    .old_layout(vk::ImageLayout::UNDEFINED)
                    .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                    .image(dst)
                    .subresource_range(full_range),
            ];
            let dep = vk::DependencyInfo::default().image_memory_barriers(&pre);
            unsafe { vk.device.cmd_pipeline_barrier2(cb, &dep) };

            let layers = vk::ImageSubresourceLayers::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .layer_count(1);
            let region = [vk::ImageCopy::default()
                .src_subresource(layers)
                .dst_subresource(layers)
                .extent(vk::Extent3D {
                    width: extent.width,
                    height: extent.height,
                    depth: 1,
                })];
            unsafe {
                vk.device.cmd_copy_image(
                    cb,
                    src,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    dst,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &region,
                );
            }

            let post = [vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::COPY)
                .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
                .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .image(dst)
                .subresource_range(full_range)];
            let dep = vk::DependencyInfo::default().image_memory_barriers(&post);
            unsafe { vk.device.cmd_pipeline_barrier2(cb, &dep) };
            Ok(())
        })?;
        Ok(())
    }

    // ── Op: fill_rect / fill_rect_batch ─────────────────────────

    /// Fill `rect` in `target`'s storage with `color` (RGBA float).
    /// Convenience wrapper around [`Self::fill_rect_batch`] for the
    /// single-rect call sites (create_pixmap zero-fill, bg_pixel
    /// init, image_text background, etc.).
    ///
    /// # Errors
    ///
    /// - `NoVk`, `UnknownDrawable`, `RendererFailed`, or any
    ///   propagated `vk::Result` from CB allocation / submit.
    pub(crate) fn fill_rect(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        target: DrawableId,
        rect: vk::Rect2D,
        color: [f32; 4],
    ) -> Result<(), RenderError> {
        // Phase B.3 (N4): one-line delegate — fill_rect_batch carries the
        // new frame-builder body. The old close_open_frame_for_non_ported_op
        // call is DELETED; fill_rect now extends the open frame instead of
        // closing it.
        self.fill_rect_batch(store, platform, target, color, &[rect])
    }

    /// Fill every rect in `rects` on `target` with `color`, accumulating
    /// ONE `RecordedFillRect` into the open frame (Phase B.3 N4 — the
    /// entire rect slice is ONE op, not split per-rect). `fill_rect` is
    /// a one-line delegate here with N=1.
    ///
    /// Body order per N9: empty-input fast-path → `renderer_failed` →
    /// `flush_render_batch` → preflight (clamp+filter) → open frame if
    /// not open → first_touch + ticket-touch + damage →
    /// `push_op_and_set_layouts` with `(target, SHADER_READ_ONLY_OPTIMAL)`.
    ///
    /// Zero-sized rects are filtered up-front; if the slice contains
    /// only empties (or is empty), the call short-circuits without
    /// touching the frame.
    ///
    /// # Errors
    ///
    /// - `NoVk`, `UnknownDrawable`, `RendererFailed`, or any
    ///   propagated `vk::Result` from CB allocation (on frame open).
    pub(crate) fn fill_rect_batch(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        target: DrawableId,
        color: [f32; 4],
        rects: &[vk::Rect2D],
    ) -> Result<(), RenderError> {
        // Phase B.3 (N9): empty-input fast-path — BEFORE flush_render_batch.
        if rects.is_empty() {
            return Ok(());
        }
        // Phase B.3 (N9): renderer_failed check before any open-frame mutation.
        if platform.renderer_failed {
            return Err(RenderError::RendererFailed);
        }
        // Phase B.3 (N9): flush pending_render_batch at entry. May close an
        // open frame (chronological X11 ordering with pre-existing batches).
        // NO flush_cow_batch — that helper is deleted in Task 4.
        self.flush_render_batch(store, platform)?;

        // Preflight: read target metadata WITHOUT mutating the frame.
        let Some(inner) = self.inner.as_mut() else {
            return Err(RenderError::NoVk);
        };
        let Some(drawable) = store.get(target) else {
            return Err(RenderError::UnknownDrawable(target));
        };
        let extent = drawable.storage.extent;
        let image_view = drawable.storage.image_view;
        let format = drawable.storage.format;
        let dst_pre_layout = inner.current_layout_for_drawable(store, target);
        let prior_dst_ticket = drawable.last_render_ticket.clone();

        // Clamp + drop empties up front. Doing this before any frame
        // mutation means an all-empty batch short-circuits cleanly.
        let clamped: Vec<vk::Rect2D> = rects
            .iter()
            .map(|r| clamp_rect(*r, extent))
            .filter(|r| r.extent.width != 0 && r.extent.height != 0)
            .collect();
        if clamped.is_empty() {
            return Ok(());
        }

        // Open the frame if not already open (mirror copy_area / put_image
        // pattern at engine.rs:5278-5287).
        if !inner.frame_builder.is_open() {
            let _ = inner;
            let ticket = platform.submit_group_ticket_or_open()?;
            let inner = self.inner.as_mut().expect("inner");
            inner.acquire_generation = inner.acquire_generation.saturating_add(1);
            let frame_generation = inner.acquire_generation;
            inner.frame_builder.open_for_paint(ticket, frame_generation);
        }
        let inner = self.inner.as_mut().expect("inner");
        let frame_ticket = inner
            .frame_builder
            .open
            .as_ref()
            .expect("just opened")
            .ticket
            .clone();

        // Prelude: first_touch + ticket-touch + damage.
        {
            let open = inner.frame_builder.open.as_mut().expect("open");
            open.touched.first_touch(target, prior_dst_ticket);
            open.layouts.first_touch_drawable(target, dst_pre_layout);
        }
        store.touch_render_fence(target, frame_ticket.clone());
        for r in &clamped {
            store.damage(target, *r);
        }

        // Phase B.3 (N4): ONE RecordedFillRect per call carrying the entire
        // clamped rect slice. Splitting per-rect would be new behavior.
        let payload = Box::new(super::frame_builder::RecordedFillRect {
            dst_id: target,
            dst_image_view: image_view,
            dst_extent: extent,
            dst_format: format,
            dst_old_layout: dst_pre_layout,
            color,
            rects: clamped,
        });
        {
            let open = inner.frame_builder.open.as_mut().expect("open");
            open.push_op_and_set_layouts(
                super::frame_builder::RecordedOp::FillRect(payload),
                &[(target, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)],
            );
        }
        Ok(())
    }

    // ── Op: logic_fill (Stage 3f.2) ─────────────────────────────

    /// Lazy-build a `LogicFillPipelineCache` for `color_format`.
    /// Each cache instance is bound to a single attachment format
    /// at construction; we shard by format so a session that paints
    /// to both BGRA8 and R8 dst formats only pays per-format pipeline
    /// compile cost. The inner cache further keys by `(function,
    /// opaque_alpha)` so all 16 X11 GC functions × {opaque, ARGB}
    /// share one pipeline-layout.
    fn ensure_logic_fill_cache(
        &mut self,
        platform: &PlatformBackend,
        color_format: vk::Format,
    ) -> Result<(), RenderError> {
        use crate::kms::vk::logic_fill_pipeline::LogicFillPipelineCache;
        let Some(inner) = self.inner.as_mut() else {
            return Err(RenderError::NoVk);
        };
        if platform.renderer_failed {
            return Err(RenderError::RendererFailed);
        }
        if inner.logic_fill_caches.contains_key(&color_format) {
            return Ok(());
        }
        let cache =
            LogicFillPipelineCache::new(Arc::clone(&inner.vk), color_format).map_err(|e| {
                log::error!(
                    "v2 ensure_logic_fill_cache: LogicFillPipelineCache::new failed: {e:?}"
                );
                RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
            })?;
        inner.logic_fill_caches.insert(color_format, cache);
        Ok(())
    }

    /// Solid-fill `rects` in `target` through a `VkLogicOp` pipeline
    /// matching `function`. Ports v1's `try_vk_fill_with_function`
    /// non-`GXcopy` path into a v2-shape per-op CB.
    ///
    /// `opaque_alpha = true` is the depth-24 (server-owned α) path:
    /// the pipeline's color blend write mask drops alpha so the
    /// `VkLogicOp` only mutates RGB and the destination's existing
    /// alpha byte is left intact (L1 server-α invariant). `false` is
    /// the depth-32 ARGB path — LogicOp applies to all four channels
    /// per X11 semantics.
    ///
    /// `fg` is the X11 wire pixel value (top byte alpha for depth 32,
    /// ignored for depth 24). The recorder unpacks it identically to
    /// v1's `try_vk_fill_with_function`. `GXclear`-class functions
    /// (Clear / Set / Invert / etc.) ignore `fg` semantically; the
    /// fragment shader still receives it but `VkLogicOp` overrides
    /// the output.
    ///
    /// # Errors
    ///
    /// `UnknownDrawable` if `target` is missing; `NoVk` on the stub
    /// engine; `Vk` for any underlying Vulkan failure.
    pub(crate) fn logic_fill(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        target: DrawableId,
        function: yserver_core::backend::GcFunction,
        opaque_alpha: bool,
        fg: u32,
        rects: &[Rectangle16],
    ) -> Result<(), RenderError> {
        use yserver_core::backend::GcFunction;

        // N9 order: empty-input fast-paths → renderer_failed →
        // flush_render_batch → preflight → cache ensure → open →
        // prelude → push.
        if rects.is_empty() {
            return Ok(());
        }
        if matches!(function, GcFunction::NoOp) {
            return Ok(());
        }
        if platform.renderer_failed {
            return Err(RenderError::RendererFailed);
        }
        self.flush_render_batch(store, platform)?;

        // Preflight: read format via shared borrow; build pipeline cache
        // for the dst format if not already present (preserve the
        // `ensure_logic_fill_cache` helper verbatim per N6).
        let format = {
            let d = store
                .get(target)
                .ok_or(RenderError::UnknownDrawable(target))?;
            d.storage.format
        };
        self.ensure_logic_fill_cache(platform, format)?;

        let Some(inner) = self.inner.as_mut() else {
            return Err(RenderError::NoVk);
        };
        let Some(drawable) = store.get(target) else {
            return Err(RenderError::UnknownDrawable(target));
        };
        let extent = drawable.storage.extent;
        let depth = drawable.depth;
        let image_view = drawable.storage.image_view;
        let dst_pre_layout = inner.current_layout_for_drawable(store, target);
        let prior_dst_ticket = drawable.last_render_ticket.clone();

        // Unpack the X11 wire pixel (preserve legacy at engine.rs:2546-2560).
        // R8_UNORM dst (depth 1/8) takes fg in color[0]; BGRA8 dst uses the
        // same server-alpha policy as solid fills: depth-32 preserves the wire
        // alpha byte, server-owned-alpha depths force opaque.
        let color = decode_x11_pixel_for_storage(fg, depth, format);

        // Clamp rects to dst extent + drop empties (preserve legacy
        // filter_map at engine.rs:2562-2588).
        let vk_rects: Vec<vk::Rect2D> = rects
            .iter()
            .filter_map(|r| {
                let x0 = i32::from(r.x).max(0);
                let y0 = i32::from(r.y).max(0);
                let x1 = (i32::from(r.x).saturating_add(i32::from(r.width)))
                    .min(i32::try_from(extent.width).unwrap_or(i32::MAX));
                let y1 = (i32::from(r.y).saturating_add(i32::from(r.height)))
                    .min(i32::try_from(extent.height).unwrap_or(i32::MAX));
                if x1 <= x0 || y1 <= y0 {
                    return None;
                }
                Some(vk::Rect2D {
                    offset: vk::Offset2D { x: x0, y: y0 },
                    extent: vk::Extent2D {
                        width: (x1 - x0) as u32,
                        height: (y1 - y0) as u32,
                    },
                })
            })
            .collect();
        if vk_rects.is_empty() {
            return Ok(());
        }

        // Open frame if not already open (same pattern as fill_rect_batch).
        if !inner.frame_builder.is_open() {
            let _ = inner;
            let ticket = platform.submit_group_ticket_or_open()?;
            let inner = self.inner.as_mut().expect("inner");
            inner.acquire_generation = inner.acquire_generation.saturating_add(1);
            let frame_generation = inner.acquire_generation;
            inner.frame_builder.open_for_paint(ticket, frame_generation);
        }
        let inner = self.inner.as_mut().expect("inner");
        let frame_ticket = inner
            .frame_builder
            .open
            .as_ref()
            .expect("just opened")
            .ticket
            .clone();

        // Prelude: first_touch + first_touch_drawable + touch_render_fence
        // + per-rect damage.
        {
            let open = inner.frame_builder.open.as_mut().expect("open");
            open.touched.first_touch(target, prior_dst_ticket);
            open.layouts.first_touch_drawable(target, dst_pre_layout);
        }
        store.touch_render_fence(target, frame_ticket.clone());
        for r in &vk_rects {
            store.damage(target, *r);
        }

        // Build RecordedLogicFill payload and append to the open frame.
        let payload = Box::new(super::frame_builder::RecordedLogicFill {
            dst_id: target,
            dst_image_view: image_view,
            dst_extent: extent,
            dst_format: format,
            dst_old_layout: dst_pre_layout,
            logic_mode: function,
            opaque_alpha,
            color,
            rects: vk_rects,
        });
        {
            let open = inner.frame_builder.open.as_mut().expect("open");
            open.push_op_and_set_layouts(
                super::frame_builder::RecordedOp::LogicFill(payload),
                &[(target, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)],
            );
        }
        Ok(())
    }

    // ── Op: copy_area (Stage 2d) ────────────────────────────────

    /// Copy `src_rect` from `src` into `dst` at `dst_pos`. The
    /// disjoint case is a straight `vkCmdCopyImage`. When
    /// `src == dst`, a same-image overlap is detected and routed
    /// through a scratch-image via `vkCmdCopyImage` twice (per
    /// Stage 2 plan §"copy_area" subcase). Stage 2's slow scratch
    /// path is acceptable — apps that hit it (xterm scroll
    /// without compositor) need glyphs to be relevant anyway,
    /// landing in Stage 3.
    ///
    /// # Errors
    ///
    /// `UnknownDrawable` if either id is missing; `Vk` for
    /// any Vk failure; `NoVk` on the stub engine.
    pub(crate) fn copy_area(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        src: DrawableId,
        dst: DrawableId,
        src_rect: vk::Rect2D,
        dst_pos: vk::Offset2D,
    ) -> Result<(), RenderError> {
        // Phase B.3 (N9): empty-input fast-path FIRST — before any flush.
        if src_rect.extent.width == 0 || src_rect.extent.height == 0 {
            return Ok(());
        }
        // Phase B.3 (N9): renderer_failed check before any open-frame mutation.
        if platform.renderer_failed {
            return Err(RenderError::RendererFailed);
        }
        // Phase B.3 (N9): flush pending_render_batch at entry. May close an
        // open frame (chronological X11 ordering with pre-existing batches).
        self.flush_render_batch(store, platform)?;

        // Preflight: read src + dst metadata + format check WITHOUT mutating
        // anything in the open frame. inner borrow is scoped.
        let Some(inner) = self.inner.as_mut() else {
            return Err(RenderError::NoVk);
        };
        let (src_image, src_extent, src_format) = {
            let d = store.get(src).ok_or(RenderError::UnknownDrawable(src))?;
            (d.storage.image, d.storage.extent, d.storage.format)
        };
        let (dst_image, dst_extent, dst_format) = {
            let d = store.get(dst).ok_or(RenderError::UnknownDrawable(dst))?;
            (d.storage.image, d.storage.extent, d.storage.format)
        };
        if src_format != dst_format {
            return Err(RenderError::UnsupportedDepth(0));
        }

        // Clamp src_rect to src extent.
        let src_rect = clamp_rect(src_rect, src_extent);
        // Project to dst: compute the dst rect (clamped to dst extent).
        // Preserve legacy arithmetic VERBATIM from the pre-B.3 body —
        // these expressions handle X11 wire negative offsets correctly.
        let dst_pos_clamped = vk::Offset2D {
            x: dst_pos.x.max(0),
            y: dst_pos.y.max(0),
        };
        let copy_w = u32::try_from(
            (i32::from_le_bytes(i32::to_le_bytes(dst_pos.x))
                + i32::try_from(src_rect.extent.width).unwrap_or(0))
            .min(i32::try_from(dst_extent.width).unwrap_or(i32::MAX))
                - dst_pos_clamped.x,
        )
        .unwrap_or(0)
        .min(src_rect.extent.width);
        let copy_h = u32::try_from(
            (i32::from_le_bytes(i32::to_le_bytes(dst_pos.y))
                + i32::try_from(src_rect.extent.height).unwrap_or(0))
            .min(i32::try_from(dst_extent.height).unwrap_or(i32::MAX))
                - dst_pos_clamped.y,
        )
        .unwrap_or(0)
        .min(src_rect.extent.height);
        if copy_w == 0 || copy_h == 0 {
            return Ok(());
        }
        let dst_rect = vk::Rect2D {
            offset: dst_pos_clamped,
            extent: vk::Extent2D {
                width: copy_w,
                height: copy_h,
            },
        };

        // Phase B.3 (N8): allocate self-overlap scratch FIRST, BEFORE any
        // open-frame state mutation. Allocation failure returns Err with the
        // frame untouched (no rollback needed).
        let self_overlap_scratch: Option<ScratchImage> = if src == dst {
            Some(allocate_scratch_image(
                &inner.vk.clone(),
                platform,
                copy_w,
                copy_h,
                src_format,
            )?)
        } else {
            None
        };

        // Open the frame if not already open. Phase B.2 Mechanism 2: bump
        // acquire_generation at open + capture on OpenFrame. Mirror of
        // composite_glyphs_via_frame_builder at engine.rs:5315-5323.
        if !inner.frame_builder.is_open() {
            // Release the inner borrow before calling the platform method
            // (which doesn't need it). Same `let _ = inner` pattern as line 5318.
            let _ = inner;
            let ticket = platform.submit_group_ticket_or_open()?;
            let inner = self.inner.as_mut().expect("inner");
            inner.acquire_generation = inner.acquire_generation.saturating_add(1);
            let frame_generation = inner.acquire_generation;
            inner.frame_builder.open_for_paint(ticket, frame_generation);
        }
        let inner = self.inner.as_mut().expect("inner");
        let frame_ticket = inner
            .frame_builder
            .open
            .as_ref()
            .expect("just opened")
            .ticket
            .clone();

        // Prelude state: first-touch + layout overlay for BOTH dst and src
        // (per N1's single-terminal layout + ticket-touch discipline).
        let dst_pre_layout = inner.current_layout_for_drawable(store, dst);
        let src_pre_layout = if src == dst {
            dst_pre_layout
        } else {
            inner.current_layout_for_drawable(store, src)
        };
        let prior_dst_ticket = store.get(dst).and_then(|d| d.last_render_ticket.clone());
        let prior_src_ticket = if src == dst {
            prior_dst_ticket.clone()
        } else {
            store.get(src).and_then(|d| d.last_render_ticket.clone())
        };
        {
            let open = inner.frame_builder.open.as_mut().expect("open");
            open.touched.first_touch(dst, prior_dst_ticket);
            open.layouts.first_touch_drawable(dst, dst_pre_layout);
            if src != dst {
                open.touched.first_touch(src, prior_src_ticket);
                open.layouts.first_touch_drawable(src, src_pre_layout);
            }
        }
        store.touch_render_fence(dst, frame_ticket.clone());
        if src != dst {
            store.touch_render_fence(src, frame_ticket.clone());
        }
        store.damage(dst, dst_rect);

        // Phase B.3 (N1 + N8): append the op + set BOTH dst and src overlays
        // to SHADER_READ_ONLY_OPTIMAL (single-terminal-layout rule). For
        // self-overlap (src == dst), only one entry needed (idempotent).
        let payload = Box::new(super::frame_builder::RecordedCopyArea {
            dst_id: dst,
            src_id: src,
            src_rect,
            dst_rect,
            src_format,
            src_extent,
            dst_extent,
            src_image,
            dst_image,
            src_old_layout: src_pre_layout,
            dst_old_layout: dst_pre_layout,
            self_overlap_scratch,
        });
        let layout_updates: &[(DrawableId, vk::ImageLayout)] = if src == dst {
            &[(dst, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)]
        } else {
            &[
                (dst, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL),
                (src, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL),
            ]
        };
        {
            let open = inner.frame_builder.open.as_mut().expect("open");
            open.push_op_and_set_layouts(
                super::frame_builder::RecordedOp::CopyArea(payload),
                layout_updates,
            );
        }
        Ok(())
    }

    // ── Op: cow_copy_area (Stage 5 Task 3 POC) ──────────────────

    /// Phase B.3 (N3, N9, N10): coalescing variant of [`Self::copy_area`]
    /// for the Composite Overlay Window. Per N3, the cow is a regular
    /// [`DrawableId`] registered in the store like any other drawable;
    /// this function forwards to [`Self::copy_area`] directly.
    ///
    /// Same-image overlap (`src == cow_id`) is defended with an explicit
    /// error (legacy invariant at engine.rs:3109-3111 preserved).
    ///
    /// The frame-builder's per-frame collapse (multiple ops in one
    /// submitted CB) provides COW coalescing.
    ///
    /// # Errors
    ///
    /// `UnknownDrawable` if `cow_id` or `src` is missing;
    /// `RendererFailed` if the renderer has already failed; `Vk`
    /// for any Vk failure; `NoVk` on the stub engine.
    pub(crate) fn cow_copy_area(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        cow_id: DrawableId,
        src: DrawableId,
        src_rect: vk::Rect2D,
        dst_pos: vk::Offset2D,
    ) -> Result<(), RenderError> {
        // N9: empty-input fast-path FIRST.
        if src_rect.extent.width == 0 || src_rect.extent.height == 0 {
            return Ok(());
        }
        if platform.renderer_failed {
            return Err(RenderError::RendererFailed);
        }
        // N9: flush pending_render_batch at entry.
        self.flush_render_batch(store, platform)?;

        // Sanity: same-image overlap not handled on the cow path
        // (legacy invariant at engine.rs:3109-3111). The regular
        // copy_area's self-overlap scratch path would handle it, but
        // cow workloads never have src == cow_id in practice.
        if src == cow_id {
            return Err(RenderError::UnsupportedDepth(0));
        }

        // Per N3: cow_id is a regular DrawableId — forward to copy_area.
        // The frame builder's per-frame collapse provides coalescing.
        self.copy_area(store, platform, src, cow_id, src_rect, dst_pos)
    }

    /// Phase B.3 (N10): attach a PRESENT-completion entry to the open frame
    /// if and only if the frame has an op that WRITES to `cow_id`. Returns
    /// `Err(entry)` if no open frame exists or the frame doesn't write to
    /// `cow_id` (predicate is `RecordedOp::dst_id() == Some(cow_id)`, NOT
    /// `touched` — touched includes sampled-only references that would
    /// attach completions to frames that never wrote the cow).
    pub(crate) fn attach_cow_present_completion(
        &mut self,
        cow_id: DrawableId,
        entry: PendingPresentEntry,
    ) -> Result<(), PendingPresentEntry> {
        let Some(inner) = self.inner.as_mut() else {
            return Err(entry);
        };
        let Some(open) = inner.frame_builder.open.as_mut() else {
            return Err(entry);
        };
        // N10 predicate: writes, NOT just touched.
        let writes_to_cow = open.ops.iter().any(|op| op.dst_id() == Some(cow_id));
        if !writes_to_cow {
            return Err(entry);
        }
        open.pending_present_completions.push(entry);
        Ok(())
    }

    pub(crate) fn drain_present_batches(&mut self) -> Vec<PendingPresentBatch> {
        let Some(inner) = self.inner.as_mut() else {
            return Vec::new();
        };
        std::mem::take(&mut inner.pending_present_batches)
    }

    // ── Op: render-composite batched path (Stage 5 Task 3) ──────

    /// Try to append the call to an in-flight [`PendingRenderBatch`]
    /// or open a new one. Returns `Ok(Some(stats))` when the call
    /// is batch-eligible AND was successfully appended; the
    /// returned `CompositeStats.deferred_to_batch == true` so the
    /// backend caller suppresses its per-call telemetry / submit-
    /// trace event. Returns `Ok(None)` if the call is NOT
    /// eligible — caller must flush any pending render batch and
    /// fall through to the regular per-call render_composite body.
    ///
    /// Eligibility predicate (conservative, mirrors design):
    /// - `src` and `mask` are `ResolvedSource::Drawable(id)` OR
    ///   `mask == None`. No Solid (would write scratch), no
    ///   Gradient (would carry per-call `axis_projection`).
    /// - `op < 13` (no `dst_readback` path; ops Disjoint/Conjoint
    ///   need a dst snapshot per call which can't share a batch).
    /// - Not self-aliasing: `src.id != dst_id` AND `mask.id != dst_id`.
    /// - Pipeline + descriptor must be identical to the pending
    ///   batch's (encoded into [`RenderBatchKey`] equality).
    ///
    /// # Errors
    ///
    /// `NoVk` on the stub engine; `RendererFailed` on a poisoned
    /// renderer; `Vk` for CB allocation / descriptor / pipeline
    /// failures on the first append.
    #[allow(
        clippy::too_many_arguments,
        reason = "Mirrors render_composite signature"
    )]
    pub(crate) fn try_append_render_batch(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        op: u8,
        src: ResolvedSource,
        mask: ResolvedSource,
        dst_id: DrawableId,
        rects: &[crate::kms::vk::ops::render::CompositeRect],
        clip_rects: Option<&[Rectangle16]>,
        src_repeat: Repeat,
        mask_repeat: Repeat,
        src_transform: Option<PictTransform>,
        mask_transform: Option<PictTransform>,
        mask_component_alpha: bool,
        src_pict_format: u32,
        mask_pict_format: u32,
        dst_pict_format: u32,
    ) -> Result<Option<CompositeStats>, RenderError> {
        // Predicate gate 1 — sources.
        let src_id = match src {
            ResolvedSource::Drawable(id) if id != dst_id => id,
            _ => return Ok(None),
        };
        let mask_id_opt: Option<DrawableId> = match mask {
            ResolvedSource::Drawable(id) if id != dst_id => Some(id),
            ResolvedSource::None => None,
            _ => return Ok(None),
        };
        // Predicate gate 2 — op needs no dst readback.
        use crate::kms::vk::render_pipeline::StdPictOp;
        let Some(std_op) = StdPictOp::from_u8(op) else {
            return Ok(None);
        };
        if std_op.needs_dst_readback() {
            return Ok(None);
        }
        // Predicate gate 3 — rects non-empty (else nothing to batch).
        if rects.is_empty() {
            return Ok(None);
        }

        // Key constraint is now minimal — only fields that affect
        // pipeline binding + render-pass attachments. Everything
        // else (src/mask views, transforms, scissors, repeats,
        // pict_formats) is re-encoded per-append.
        let new_key = RenderBatchKey {
            dst: dst_id,
            op,
            dst_pict_format,
            mask_component_alpha,
        };

        // Key-mismatch branch: flush, then re-call to open fresh.
        let need_flush_for_key_change = self
            .inner
            .as_ref()
            .and_then(|i| i.pending_render_batch.as_ref())
            .is_some_and(|b| b.key != new_key);
        if need_flush_for_key_change {
            self.flush_render_batch(store, platform)?;
        }

        // Lazy-init RENDER assets (mirrors the unbatched path).
        self.ensure_render_assets(platform)?;
        let inner = self.inner.as_mut().ok_or(RenderError::NoVk)?;
        if platform.renderer_failed {
            return Err(RenderError::RendererFailed);
        }

        // Resolve dst metadata.
        let (dst_image, dst_view, dst_extent, dst_format, dst_depth) = {
            let d = store
                .get(dst_id)
                .ok_or(RenderError::UnknownDrawable(dst_id))?;
            (
                d.storage.image,
                d.storage.image_view,
                d.storage.extent,
                d.storage.format,
                d.depth,
            )
        };
        if dst_extent.width == 0 || dst_extent.height == 0 {
            return Ok(None);
        }
        if !matches!(
            dst_format,
            vk::Format::B8G8R8A8_UNORM | vk::Format::R8_UNORM
        ) {
            return Ok(None);
        }
        let dst_has_alpha = dst_has_alpha_for_pict_format(dst_format, dst_depth, dst_pict_format);

        // Resolve src view + extent (drawable_view_cache lookup).
        let src_info =
            drawable_for_render_view(store, src_id).ok_or(RenderError::UnknownDrawable(src_id))?;
        let src_class =
            swizzle_class_for_pict_format(src_info.format, src_info.depth, src_pict_format);
        let src_sampler = sampler_config_for_repeat(src_repeat);
        let src_view = ensure_drawable_view(
            &inner.vk,
            &mut inner.drawable_view_cache,
            src_id,
            src_info.image,
            src_info.format,
            src_sampler,
            src_class,
        )?;
        let src_extent = src_info.extent;

        // Resolve mask view + extent.
        let white_mask_view = inner
            .white_mask_image
            .as_ref()
            .expect("ensured")
            .image_view();
        let (mask_view, mask_extent) = if let Some(mid) = mask_id_opt {
            let info =
                drawable_for_render_view(store, mid).ok_or(RenderError::UnknownDrawable(mid))?;
            let class = swizzle_class_for_pict_format(info.format, info.depth, mask_pict_format);
            let sampler = sampler_config_for_repeat(mask_repeat);
            let view = ensure_drawable_view(
                &inner.vk,
                &mut inner.drawable_view_cache,
                mid,
                info.image,
                info.format,
                sampler,
                class,
            )?;
            (view, info.extent)
        } else {
            (
                white_mask_view,
                vk::Extent2D {
                    width: 1,
                    height: 1,
                },
            )
        };

        // Pipeline lookup.
        let pipeline = inner
            .render_pipelines
            .as_mut()
            .expect("ensured")
            .get(std_op, dst_format, dst_has_alpha, mask_component_alpha)
            .map_err(|e| {
                log::warn!("v2 try_append_render_batch: pipeline build failed: {e:?}");
                RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
            })?;
        let pipeline_layout = inner
            .render_pipelines
            .as_ref()
            .expect("ensured")
            .pipeline_layout();

        // Build CompositeAttrs (force_opaque + repeat + transforms).
        let src_force_opaque = resolve_force_opaque_pict_format(store, &src, src_pict_format);
        let mask_force_opaque = resolve_force_opaque_pict_format(store, &mask, mask_pict_format);
        let user_src_xform =
            crate::kms::backend::pixman_transform_to_affine(src_transform.as_ref(), src_extent);
        let user_mask_xform =
            crate::kms::backend::pixman_transform_to_affine(mask_transform.as_ref(), mask_extent);
        let effective_src_repeat = crate::kms::backend::repeat_to_shader_const(src_repeat);
        let effective_mask_repeat = if mask_id_opt.is_some() {
            crate::kms::backend::repeat_to_shader_const(mask_repeat)
        } else {
            crate::kms::vk::render_pipeline::REPEAT_PAD
        };
        let attrs = crate::kms::vk::ops::render::CompositeAttrs {
            src_extent,
            mask_extent,
            src_repeat: effective_src_repeat,
            mask_repeat: effective_mask_repeat,
            src_force_opaque,
            mask_force_opaque,
            src_xform: user_src_xform,
            mask_xform: user_mask_xform,
        };

        // Build clip scissor list (same clamping as unbatched path).
        let clip_scissors = build_render_clip_scissors(clip_rects, dst_extent);
        if clip_scissors.is_empty() {
            return Ok(Some(CompositeStats {
                deferred_to_batch: true,
                ..CompositeStats::default()
            }));
        }

        // Allocate THIS call's descriptor set (binds this
        // append's src + mask views). With the relaxed predicate,
        // every append gets its own descriptor — pipeline + dst
        // are shared across the batch but the per-draw inputs
        // are not.
        inner.acquire_generation += 1;
        let generation = inner.acquire_generation;
        let descriptor_set = inner
            .render_pipelines
            .as_ref()
            .expect("ensured")
            .allocate_descriptor_for_views_into_ring(
                &mut inner.descriptor_pool_ring,
                generation,
                src_view,
                mask_view,
                white_mask_view, // dummy dst_readback (no readback in batched path)
            )?;

        // Branch A: open a fresh batch (no pending).
        let is_open = inner.pending_render_batch.is_some();
        if !is_open {
            let (cb, ticket) = begin_op_cb(inner, platform)?;
            // Use an adapter so `record_render_composite_open` can
            // update the dst's tracked layout.
            let mut adapter = {
                let d = store.get_mut(dst_id).expect("checked");
                StorageCompositeTarget {
                    extent: dst_extent,
                    image: dst_image,
                    image_view: dst_view,
                    current_layout: d.storage.current_layout,
                }
            };
            crate::kms::vk::ops::render::record_render_composite_open(
                &inner.vk,
                cb,
                &mut adapter,
                pipeline,
            )?;
            // record_render_composite_open does NOT mutate the
            // tracked layout (that happens at _close). Update the
            // adapter's snapshot back into Drawable.storage now so
            // intermediate observers (none expected in this path)
            // see COLOR_ATTACHMENT_OPTIMAL between open and close.
            {
                let d = store.get_mut(dst_id).expect("checked");
                d.storage.current_layout = vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL;
            }
            // First-append draws (binds this call's descriptor set).
            crate::kms::vk::ops::render::record_render_composite_draws(
                &inner.vk,
                cb,
                pipeline_layout,
                descriptor_set,
                dst_extent,
                &attrs,
                rects,
                &clip_scissors,
            );
            // Accumulate damage.
            let mut dst_damage = Vec::with_capacity(rects.len());
            for cr in rects {
                let rect = vk::Rect2D {
                    offset: vk::Offset2D {
                        x: cr.dst_x,
                        y: cr.dst_y,
                    },
                    extent: vk::Extent2D {
                        width: cr.width,
                        height: cr.height,
                    },
                };
                dst_damage.push(clamp_rect(rect, dst_extent));
            }
            let accumulated_draws =
                u32::try_from(rects.len() * clip_scissors.len()).unwrap_or(u32::MAX);
            let mut touched = HashSet::new();
            touched.insert(src_id);
            if let Some(mid) = mask_id_opt {
                touched.insert(mid);
            }
            // Stage 5 Task 3 fix (UAF on Rembrandt iGPU, 2026-05-22):
            // touch every drawable the batch's CB now references
            // (dst + src + mask) with the batch ticket. Pre-fix
            // this only happened in `flush_render_batch`, leaving
            // a window where an intervening FreePixmap(src) before
            // flush would destroy the VkImage while the batch CB
            // still samples it.
            store.touch_render_fence(dst_id, ticket.clone());
            store.touch_render_fence(src_id, ticket.clone());
            if let Some(mid) = mask_id_opt {
                store.touch_render_fence(mid, ticket.clone());
            }
            inner.pending_render_batch = Some(PendingRenderBatch {
                cb,
                ticket,
                key: new_key,
                dst_damage,
                touched_drawables: touched,
                any_mask: mask_id_opt.is_some(),
                accumulated_draws,
                coalesced_count: 1,
            });
            return Ok(Some(CompositeStats {
                recorded_draws: accumulated_draws,
                deferred_to_batch: true,
                ..CompositeStats::default()
            }));
        }

        // Branch B: append to the existing batch (key matched by
        // the early check; pipeline still bound from open).
        // record_render_composite_draws will bind THIS call's
        // descriptor set inside the open render pass.
        let batch_cb = inner
            .pending_render_batch
            .as_ref()
            .expect("pending batch present in append branch")
            .cb;
        crate::kms::vk::ops::render::record_render_composite_draws(
            &inner.vk,
            batch_cb,
            pipeline_layout,
            descriptor_set,
            dst_extent,
            &attrs,
            rects,
            &clip_scissors,
        );
        // Update batch state.
        let added_draws = u32::try_from(rects.len() * clip_scissors.len()).unwrap_or(u32::MAX);
        let batch = inner
            .pending_render_batch
            .as_mut()
            .expect("pending batch present");
        batch.accumulated_draws = batch.accumulated_draws.saturating_add(added_draws);
        batch.coalesced_count = batch.coalesced_count.saturating_add(1);
        batch.touched_drawables.insert(src_id);
        if let Some(mid) = mask_id_opt {
            batch.touched_drawables.insert(mid);
            batch.any_mask = true;
        }
        // Stage 5 Task 3 fix: touch the new drawables the append
        // just added to `touched_drawables` with the batch ticket
        // (see open branch above for rationale). Dst's ticket is
        // already set from open; appending doesn't change it.
        let batch_ticket = batch.ticket.clone();
        store.touch_render_fence(src_id, batch_ticket.clone());
        if let Some(mid) = mask_id_opt {
            store.touch_render_fence(mid, batch_ticket);
        }
        for cr in rects {
            let rect = vk::Rect2D {
                offset: vk::Offset2D {
                    x: cr.dst_x,
                    y: cr.dst_y,
                },
                extent: vk::Extent2D {
                    width: cr.width,
                    height: cr.height,
                },
            };
            batch.dst_damage.push(clamp_rect(rect, dst_extent));
        }
        Ok(Some(CompositeStats {
            recorded_draws: batch.accumulated_draws,
            deferred_to_batch: true,
            ..CompositeStats::default()
        }))
    }

    /// Flush the pending render batch (if any). Records
    /// `cmd_end_rendering` + exit layout transition, ends + submits
    /// the CB, clones the fence ticket onto every drawable touched
    /// by the batch (dst + src + optional mask), applies
    /// accumulated damage, pushes a `SubmittedOp` + one
    /// `RenderFlushRecord` for backend drain.
    ///
    /// Returns `Some(coalesced_count)` if a batch was flushed,
    /// `None` if there was nothing pending. Caller uses the
    /// count for telemetry (`record_render_batch_flushed`).
    pub(crate) fn flush_render_batch(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
    ) -> Result<Option<u32>, RenderError> {
        let Some(inner) = self.inner.as_mut() else {
            return Ok(None);
        };
        let Some(batch) = inner.pending_render_batch.take() else {
            return Ok(None);
        };
        if platform.renderer_failed {
            log::debug!(
                "v2 flush_render_batch: renderer_failed; dropping batch \
                 (coalesced {} composites)",
                batch.coalesced_count,
            );
            return Ok(None);
        }

        // Resolve dst metadata (image + extent + tracked layout).
        let (dst_image, dst_view, dst_extent) = {
            let d = store
                .get(batch.key.dst)
                .ok_or(RenderError::UnknownDrawable(batch.key.dst))?;
            (d.storage.image, d.storage.image_view, d.storage.extent)
        };

        // Close the render pass + transition dst back to
        // SHADER_READ_ONLY.
        let mut adapter = StorageCompositeTarget {
            extent: dst_extent,
            image: dst_image,
            image_view: dst_view,
            current_layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        };
        crate::kms::vk::ops::render::record_render_composite_close(
            &inner.vk,
            batch.cb,
            &mut adapter,
        );
        {
            let d = store.get_mut(batch.key.dst).expect("checked");
            d.storage.current_layout = adapter.current_layout;
        }

        // End + submit (append to group).
        end_and_submit_op(inner, platform, batch.cb, &batch.ticket)?;

        // CPU bookkeeping.
        store.touch_render_fence(batch.key.dst, batch.ticket.clone());
        for tid in &batch.touched_drawables {
            store.touch_render_fence(*tid, batch.ticket.clone());
        }
        for rect in &batch.dst_damage {
            store.damage(batch.key.dst, *rect);
        }
        inner.acquire_generation += 1;
        let generation = inner.acquire_generation;
        let coalesced_count = batch.coalesced_count;
        inner.pending_group_ops.push(SubmittedOp {
            cb: batch.cb,
            ticket: batch.ticket,
            staging: None,
            scratch: Vec::new(),
            atlas_ticket: None,
            generation,
            retired_resources: Vec::new(),
        });
        inner.render_flush_records.push(RenderFlushRecord {
            dst: batch.key.dst,
            op: batch.key.op,
            has_mask: batch.any_mask,
            coalesced_count,
        });
        // `inner` borrow released. Auto-flush for render_batch (no semaphore path).
        self.maybe_auto_flush_submit_group(store, platform)?;
        Ok(Some(coalesced_count))
    }

    /// Drain the queue of render-batch flush records. Backend
    /// calls this once per `maybe_composite` tick.
    pub(crate) fn drain_render_flush_records(&mut self) -> Vec<RenderFlushRecord> {
        let Some(inner) = self.inner.as_mut() else {
            return Vec::new();
        };
        std::mem::take(&mut inner.render_flush_records)
    }

    // ── Op: put_image ───────────────────────────────────────────

    /// Upload `src_bytes` (interpreted per `src_depth`) into
    /// `target` at `dst_pos`. Stage 2c supports depths 1, 8, 24,
    /// 32 with the byte layouts the X11 dispatcher emits (see
    /// the inline conversion table). Per-op staging buffer; no
    /// arena coalescing yet.
    ///
    /// # Errors
    ///
    /// - `UnsupportedDepth` if `src_depth` isn't 1/8/24/32.
    /// - `TruncatedSource` if `src_bytes` is shorter than the
    ///   row stride × height the depth implies.
    /// - `Vk(...)` for any Vk failure (CB / buffer / submit).
    pub(crate) fn put_image(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        target: DrawableId,
        dst_pos: vk::Offset2D,
        src_extent: vk::Extent2D,
        src_bytes: &[u8],
        src_depth: u8,
    ) -> Result<(), RenderError> {
        // Phase B.3 (N9): empty-input fast-path FIRST — before renderer_failed
        // and flush_render_batch.
        if src_extent.width == 0 || src_extent.height == 0 {
            return Ok(());
        }
        // Phase B.3 (N9): renderer_failed check before any open-frame mutation.
        if platform.renderer_failed {
            return Err(RenderError::RendererFailed);
        }
        // Phase B.3 (N9): flush pending_render_batch at entry. May close an
        // open frame (chronological X11 ordering with pre-existing batches).
        // No flush_cow_batch — that helper is deleted in Task 4.
        self.flush_render_batch(store, platform)?;

        let Some(inner) = self.inner.as_mut() else {
            return Err(RenderError::NoVk);
        };
        let Some(drawable) = store.get(target) else {
            return Err(RenderError::UnknownDrawable(target));
        };

        // Stage 2c-supported depths only. Anything else is logged
        // upstream and routes to the gap path; we surface the
        // type-level reject so the backend wrapper can dedup-log.
        let dst_bpp: u32 = match src_depth {
            1 | 4 | 8 => 1,
            24 | 32 => 4,
            _ => return Err(RenderError::UnsupportedDepth(src_depth)),
        };
        let dst_format = drawable.storage.format;
        // The store allocates storage by depth; format mismatch
        // here means the caller targeted a depth-mismatched
        // drawable. Treat as unsupported.
        let expected_format = if dst_bpp == 1 {
            vk::Format::R8_UNORM
        } else {
            vk::Format::B8G8R8A8_UNORM
        };
        if dst_format != expected_format {
            return Err(RenderError::UnsupportedDepth(src_depth));
        }

        let dst_extent = drawable.storage.extent;
        let dst_image = drawable.storage.image;
        let dst_pre_layout = inner.current_layout_for_drawable(store, target);
        let prior_dst_ticket = drawable.last_render_ticket.clone();

        // Clamp the put rect to the storage extent. Per Stage 2
        // plan, GC clipping is the backend wrapper's concern;
        // the engine only sees the dst-extent guard.
        let clipped = clamp_put_rect(dst_pos, src_extent, dst_extent);
        let Some((dst_rect, src_origin_in_input)) = clipped else {
            return Ok(());
        };
        let copy_w = dst_rect.extent.width;
        let copy_h = dst_rect.extent.height;
        let staging_size = u64::from(copy_w) * u64::from(copy_h) * u64::from(dst_bpp);
        if staging_size == 0 {
            return Ok(());
        }

        // Phase B.3 (N8-style ordering): allocate the staging buffer BEFORE
        // any open-frame mutation so an allocation failure leaves the frame
        // untouched (no rollback needed).
        let staging = Arc::new(StagingBuffer::new(inner.vk.clone(), staging_size.max(1))?);
        // Convert src_bytes → staging according to (depth, dst_format).
        let (sx, sy) = src_origin_in_input;
        unpack_to_staging(
            src_bytes,
            src_extent,
            sx,
            sy,
            copy_w,
            copy_h,
            src_depth,
            staging.mapped.as_ptr(),
        )?;

        // Open the frame if not already open. Phase B.2 Mechanism 2: bump
        // acquire_generation at open + capture on OpenFrame. (Same pattern as
        // composite_glyphs_via_frame_builder.)
        if !inner.frame_builder.is_open() {
            let _ = inner;
            let ticket = platform.submit_group_ticket_or_open()?;
            let inner = self.inner.as_mut().expect("inner");
            inner.acquire_generation = inner.acquire_generation.saturating_add(1);
            let frame_generation = inner.acquire_generation;
            inner.frame_builder.open_for_paint(ticket, frame_generation);
        }
        let inner = self.inner.as_mut().expect("inner");
        let frame_ticket = inner
            .frame_builder
            .open
            .as_ref()
            .expect("just opened")
            .ticket
            .clone();

        // Phase B.3 (N2): pin the staging Arc into the frame pin-set BEFORE
        // any `store` mutation (first_touch + damage happen after pinning so
        // that a pin-failure doesn't leave store state inconsistent).
        let staging_pin_idx = {
            let open = inner.frame_builder.open.as_mut().expect("open");
            open.touched.first_touch(target, prior_dst_ticket);
            open.layouts.first_touch_drawable(target, dst_pre_layout);
            open.pins.pin_staging(Arc::clone(&staging))
        };
        store.touch_render_fence(target, frame_ticket.clone());
        store.damage(target, dst_rect);

        // Phase B.3 (N1): push the op and set the terminal layout
        // SHADER_READ_ONLY_OPTIMAL for the dst.
        let payload = Box::new(super::frame_builder::RecordedPutImage {
            dst_id: target,
            dst_rect,
            dst_image,
            dst_extent,
            dst_old_layout: dst_pre_layout,
            staging_pin_idx,
        });
        {
            let open = inner.frame_builder.open.as_mut().expect("open");
            open.push_op_and_set_layouts(
                super::frame_builder::RecordedOp::PutImage(payload),
                &[(target, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)],
            );
        }
        Ok(())
    }

    // ── Op: get_image (synchronous) ─────────────────────────────

    /// Read `rect` from `src`'s storage. **Synchronous** — waits
    /// on the readback `FenceTicket` before returning. The only
    /// sync path on the v2 paint surface; protocol design makes
    /// `GetImage` an RPC, so a host wait is unavoidable.
    ///
    /// Returns bytes in **wire format** (see `pack_from_storage`):
    /// for depth-32/24, `rect_w * rect_h * 4` BGRA-order bytes
    /// (alpha undefined for depth-24). For depth-8, byte rows
    /// padded to 32 bits. For depth-1, bitmap rows padded to 32
    /// bits, LSBFirst bit order; storage is `R8` and each non-zero
    /// byte sets one bit. All layouts keep the total a multiple of
    /// 4, which `wrap_get_image_reply` relies on for the reply
    /// length field.
    ///
    /// # Errors
    ///
    /// - `UnsupportedDepth` for depths other than 1/8/24/32.
    /// - `Vk` for CB / buffer / submit / wait failures.
    pub(crate) fn get_image(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        src: DrawableId,
        rect: vk::Rect2D,
        out_depth: u8,
    ) -> Result<Vec<u8>, RenderError> {
        // get_image is a synchronous CPU readback — must see all
        // prior submits including any pending COW batch.
        self.flush_render_batch(store, platform)?;
        // Phase B.1 close trigger 2: close any open frame before the
        // readback's ticket.wait(). The frame's CB must submit before the
        // readback CB records; without this, the readback would race the
        // deferred frame.
        self.close_open_frame(store, platform, super::frame_builder::CloseReason::SyncWait)?;
        // Phase A: drain any buffered paint group BEFORE allocating the
        // readback CB. This ensures prior paint ops are queued/submitted
        // so the readback observes them. Distinct from the second
        // flush below — which signals the readback's own fence so
        // ticket.wait() observes a queued signal-op. Both are needed:
        // this one drains prior buffered paint; the second flushes the
        // readback CB itself.
        self.flush_submit_group(
            store,
            platform,
            super::submit_group::FlushReason::SyncBoundary,
        )
        .map_err(RenderError::Vk)?;
        let Some(inner) = self.inner.as_mut() else {
            return Err(RenderError::NoVk);
        };
        if platform.renderer_failed {
            return Err(RenderError::RendererFailed);
        }
        let Some(drawable) = store.get_mut(src) else {
            return Err(RenderError::UnknownDrawable(src));
        };
        let storage_bpp: u32 = match out_depth {
            1 | 4 | 8 => 1,
            24 | 32 => 4,
            _ => return Err(RenderError::UnsupportedDepth(out_depth)),
        };
        let extent = drawable.storage.extent;
        // Clamp the read rect to storage bounds.
        let clipped = clamp_rect(rect, extent);
        let copy_w = clipped.extent.width;
        let copy_h = clipped.extent.height;
        if copy_w == 0 || copy_h == 0 {
            return Ok(Vec::new());
        }
        let staging_size = u64::from(copy_w) * u64::from(copy_h) * u64::from(storage_bpp);
        let staging = Arc::new(StagingBuffer::new(inner.vk.clone(), staging_size.max(1))?);

        let (cb, ticket) = begin_op_cb(inner, platform)?;
        let device = &inner.vk.device;

        drawable.record_layout_transition(
            &inner.vk,
            cb,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            vk::PipelineStageFlags2::ALL_COMMANDS,
            vk::AccessFlags2::SHADER_SAMPLED_READ
                | vk::AccessFlags2::TRANSFER_WRITE
                | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            vk::PipelineStageFlags2::COPY,
            vk::AccessFlags2::TRANSFER_READ,
        );

        let region = [vk::BufferImageCopy::default()
            .buffer_offset(0)
            .buffer_row_length(0)
            .buffer_image_height(0)
            .image_subresource(
                vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .layer_count(1),
            )
            .image_offset(vk::Offset3D {
                x: clipped.offset.x,
                y: clipped.offset.y,
                z: 0,
            })
            .image_extent(vk::Extent3D {
                width: copy_w,
                height: copy_h,
                depth: 1,
            })];
        unsafe {
            device.cmd_copy_image_to_buffer(
                cb,
                drawable.storage.image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                staging.buffer,
                &region,
            );
        }

        drawable.record_layout_transition(
            &inner.vk,
            cb,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            vk::PipelineStageFlags2::COPY,
            vk::AccessFlags2::TRANSFER_READ,
            vk::PipelineStageFlags2::FRAGMENT_SHADER,
            vk::AccessFlags2::SHADER_SAMPLED_READ,
        );

        end_and_submit_op(inner, platform, cb, &ticket)?;
        store.touch_render_fence(src, ticket.clone());
        // `inner` borrow released before flush so self.flush_submit_group
        // can take &mut self.
        let _ = inner;

        // Phase A: end_and_submit_op now only appends to the SubmitGroup.
        // Drive the explicit flush so the fence has a queued signal-op
        // before we wait on it.
        self.flush_submit_group(
            store,
            platform,
            super::submit_group::FlushReason::SyncBoundary,
        )
        .map_err(RenderError::Vk)?;
        let Some(inner) = self.inner.as_mut() else {
            return Err(RenderError::NoVk);
        };

        // Sync wait — off the hot path by protocol design.
        ticket.wait(&inner.vk)?;

        // Pack storage bytes into wire format.
        let raw_size = (u64::from(copy_w) * u64::from(copy_h) * u64::from(storage_bpp)) as usize;
        // SAFETY: staging is HOST_COHERENT, mapped for `staging.size`
        // bytes (≥ raw_size), and the fence above signalled, so
        // the GPU has completed all writes.
        let raw: &[u8] = unsafe { std::slice::from_raw_parts(staging.mapped.as_ptr(), raw_size) };
        let out = pack_from_storage(raw, copy_w, copy_h, out_depth)?;

        // `get_image` is the ONLY exception to the
        // `pending_group_ops`-on-paint-op rule. We push direct to
        // `submitted` because the fence is already signaled (we waited
        // on it above) and `staging.mapped` was read BEFORE we could
        // have moved staging into `pending_group_ops` (lifetime
        // requirement). `poll_retired` retires this op on the next tick.
        inner.acquire_generation += 1;
        let generation = inner.acquire_generation;
        inner.submitted.push_back(SubmittedOp {
            cb,
            ticket,
            staging: Some(staging),
            scratch: Vec::new(),
            atlas_ticket: None,
            generation,
            retired_resources: Vec::new(),
        });

        Ok(out)
    }

    // ── Op: image_text (Stage 3a) ───────────────────────────────

    /// One glyph the caller hands to [`RenderEngine::image_text`].
    /// CPU-side pre-rasterised by FreeType so the engine doesn't
    /// touch FreeType state. `pixels` is row-major, tightly packed
    /// (no row padding) — width × height alpha bytes.
    ///
    /// The pen-left/pen-top offsets are applied to `dst_x` /
    /// `dst_y` by the caller, so the engine just packs the glyph
    /// and queues a draw at the supplied destination coords.
    /// Stage 3a: drive a single text run against `target`'s
    /// storage. CPU-side glyph rasterisation is the caller's
    /// concern (KmsBackendV2 wraps the v1 FreeType path); the
    /// engine takes the resulting [`PreparedGlyph`] slice, interns
    /// each into the atlas, and records one TextPipeline draw
    /// covering the whole run.
    ///
    /// `font_xid` keys the glyph cache so the same codepoint
    /// rendered at two different font sizes ends up at two atlas
    /// slots. `foreground_rgba` is the GC foreground in [0..1].
    /// Damage is recorded on the target at the union of glyph
    /// bounding boxes.
    ///
    /// Returns telemetry counts the caller feeds to the v2 backend
    /// telemetry sink: how many distinct atlas interns happened
    /// (= miss count this run), how many glyph uploads were
    /// submitted (= same as interns today; collapses if later
    /// coalesced), and how many glyphs were dropped due to
    /// atlas-full.
    ///
    /// # Errors
    ///
    /// - `NoVk` on the stub engine.
    /// - `UnknownDrawable` when `target` isn't in `store`.
    /// - `Vk(...)` for any CB / submit failure. Best-effort: an
    ///   upload that fails partway is logged and the affected
    ///   glyph is dropped; only catastrophic failures (text-run
    ///   CB allocation, atlas init) propagate.
    pub(crate) fn image_text(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        target: DrawableId,
        font_xid: u32,
        foreground_rgba: [f32; 4],
        rendered: &[PreparedGlyph],
    ) -> Result<ImageTextStats, RenderError> {
        // Phase B.3 Task 14: image_text body rewrite — frame-builder path.
        // Body order per N9: empty-input → renderer_failed → flush_render_batch
        // → preflight (format gate N7 LOAD-BEARING) → lazy-init → open frame
        // → first_touch + atlas snapshot → glyph loop → push ImageText.

        // (0) Empty-input fast-path.
        let mut stats = ImageTextStats::default();
        if rendered.is_empty() {
            return Ok(stats);
        }

        // (1) renderer_failed check.
        if platform.renderer_failed {
            return Err(RenderError::RendererFailed);
        }

        // (2) Flush pre-existing render batch before opening the frame.
        // NO flush_cow_batch — that helper is deleted in Task 4.
        self.flush_render_batch(store, platform)?;

        // (3) Preflight: store lookup + TARGET-FORMAT GATE (N7 LOAD-BEARING).
        // Gate fires BEFORE any atlas first-touch / glyph upload / op append.
        // No rollback path needed because nothing has been recorded yet.
        let Some(inner) = self.inner.as_mut() else {
            return Err(RenderError::NoVk);
        };
        let (target_extent, target_format) = {
            let d = store
                .get(target)
                .ok_or(RenderError::UnknownDrawable(target))?;
            (d.storage.extent, d.storage.format)
        };
        if target_format != vk::Format::B8G8R8A8_UNORM {
            log::warn!(
                "v2 image_text (frame_builder): target xid={:?} has format {:?}; \
                 text pipeline only supports B8G8R8A8_UNORM — dropping run",
                store.get(target).map(|d| d.xid),
                target_format,
            );
            return Ok(stats);
        }

        // (4) Lazy-init V2GlyphAtlas + TextPipeline (preserve
        //     engine.rs:4531-4553 verbatim in spirit).
        if inner.glyph_atlas.is_none() {
            match V2GlyphAtlas::new(Arc::clone(&inner.vk)) {
                Ok(a) => inner.glyph_atlas = Some(a),
                Err(e) => {
                    log::error!("v2 image_text (frame_builder): V2GlyphAtlas::new failed: {e:?}");
                    return Err(RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED));
                }
            }
        }
        if inner.text_pipeline.is_none() {
            let atlas_view = inner.glyph_atlas.as_ref().expect("just built").image_view();
            match TextPipeline::new(
                Arc::clone(&inner.vk),
                vk::Format::B8G8R8A8_UNORM,
                atlas_view,
            ) {
                Ok(p) => inner.text_pipeline = Some(p),
                Err(e) => {
                    log::error!("v2 image_text (frame_builder): TextPipeline::new failed: {e:?}");
                    return Err(RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED));
                }
            }
        }

        // (5) Open frame if not already open.
        if !inner.frame_builder.is_open() {
            let _ = inner;
            let ticket = platform.submit_group_ticket_or_open()?;
            let inner = self.inner.as_mut().expect("inner");
            inner.acquire_generation = inner.acquire_generation.saturating_add(1);
            let frame_generation = inner.acquire_generation;
            inner.frame_builder.open_for_paint(ticket, frame_generation);
        }
        let inner = self.inner.as_mut().expect("inner");

        // (6) first_touch dst + ticket-touch dst + first_touch_drawable.
        let frame_ticket = inner
            .frame_builder
            .open
            .as_ref()
            .expect("just opened")
            .ticket
            .clone();
        let prior_dst_ticket = store.get(target).and_then(|d| d.last_render_ticket.clone());
        let dst_pre_frame_layout = inner.current_layout_for_drawable(store, target);
        {
            let open = inner.frame_builder.open.as_mut().expect("just opened");
            open.touched.first_touch(target, prior_dst_ticket);
            open.layouts
                .first_touch_drawable(target, dst_pre_frame_layout);
        }
        store.touch_render_fence(target, frame_ticket.clone());

        // (7) N7 atlas transactional discipline (LOAD-BEARING): snapshot
        //     atlas_prev_ticket + atlas layout on the FIRST atlas-touching op
        //     in this frame (mirror composite_glyphs_via_frame_builder
        //     engine.rs:5644-5663).
        {
            let atlas_pre_ticket: Option<FenceTicket> = inner
                .glyph_atlas
                .as_ref()
                .and_then(|a| a.last_render_ticket().cloned());
            let atlas_pre_layout: vk::ImageLayout = inner
                .glyph_atlas
                .as_ref()
                .map(super::glyph_atlas::V2GlyphAtlas::current_layout)
                .unwrap_or(vk::ImageLayout::UNDEFINED);
            let open = inner.frame_builder.open.as_mut().expect("open");
            if open.atlas_prev_ticket_snapshot.is_none() {
                open.atlas_prev_ticket_snapshot = Some(atlas_pre_ticket);
                open.layouts.first_touch_atlas(atlas_pre_layout);
            }
        }

        // (8) Per-glyph walk: lookup → on miss, pack + allocate staging
        //     buffer + pin via open.pins.pin_staging + push
        //     RecordedOp::GlyphUpload (NOT push_op_and_set_layouts because
        //     GlyphUpload.dst_id() is None — no layout updates).
        let ceiling = inner.frame_builder.max_pinned_resources_per_frame();
        let pending_pins_before_call = inner
            .frame_builder
            .open
            .as_ref()
            .map(|o| o.pins.len())
            .unwrap_or(0);

        let mut glyphs_to_draw: Vec<super::frame_builder::RecordedTextGlyph> =
            Vec::with_capacity(rendered.len());
        let mut new_uploads: Vec<(GlyphKey, AtlasEntry, Arc<StagingBuffer>)> = Vec::new();
        let mut new_zero_inserts: Vec<(GlyphKey, AtlasEntry)> = Vec::new();
        let mut damage_min_x = i32::MAX;
        let mut damage_min_y = i32::MAX;
        let mut damage_max_x = i32::MIN;
        let mut damage_max_y = i32::MIN;

        for g in rendered {
            let key = GlyphKey {
                font_xid,
                codepoint: g.codepoint,
            };
            #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
            let w_u = g.w as u32;
            #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
            let h_u = g.h as u32;

            // (a) Committed atlas hit?
            let committed_hit = inner.glyph_atlas.as_ref().expect("init").lookup(key);
            // (b) Pending-insert hit in the open frame?
            let pending_hit = inner.frame_builder.open.as_ref().and_then(|o| {
                o.pending_glyph_inserts
                    .entries
                    .iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, e)| *e)
            });
            // (c) New-uploads dedupe (same call earlier)?
            let dedupe_hit = new_uploads
                .iter()
                .find(|(k, _, _)| *k == key)
                .map(|(_, e, _)| *e);

            let entry = if let Some(hit) = committed_hit.or(pending_hit).or(dedupe_hit) {
                hit
            } else {
                // Zero-area glyph (space): cache degenerate entry; no
                // atlas slot consumed.
                if w_u == 0 || h_u == 0 {
                    let e = AtlasEntry {
                        atlas_x: 0,
                        atlas_y: 0,
                        w: 0,
                        h: 0,
                        pen_left: 0,
                        pen_top: 0,
                    };
                    new_zero_inserts.push((key, e));
                    continue;
                }
                // Pin-ceiling enforcement: check BEFORE pack() so dropped
                // glyphs don't leak atlas slots (mirror B.1 pattern).
                if new_uploads.len() + 1 + pending_pins_before_call > ceiling {
                    stats.glyphs_dropped += 1;
                    continue;
                }
                // Pre-validate pixels length BEFORE pack().
                let copy_len = (w_u as usize) * (h_u as usize);
                if g.pixels.len() < copy_len {
                    log::warn!(
                        "v2 image_text (frame_builder): glyph pixels {} < {} expected; \
                         dropping pre-pack",
                        g.pixels.len(),
                        copy_len,
                    );
                    stats.glyphs_dropped += 1;
                    continue;
                }
                let Some((atlas_x, atlas_y)) =
                    inner.glyph_atlas.as_mut().expect("init").pack(w_u, h_u)
                else {
                    inner.glyph_atlas.as_mut().expect("init").note_full_once();
                    stats.glyphs_dropped += 1;
                    continue;
                };
                stats.atlas_interns += 1;
                let upload_bytes = u64::from(w_u) * u64::from(h_u);
                let staging = Arc::new(StagingBuffer::new(
                    Arc::clone(&inner.vk),
                    upload_bytes.max(1),
                )?);
                let src_slice = &g.pixels[..copy_len];
                // SAFETY: staging is HOST_COHERENT, mapped for at least
                // `upload_bytes` bytes; `src_slice.len() == copy_len`.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        src_slice.as_ptr(),
                        staging.mapped.as_ptr(),
                        copy_len,
                    );
                }
                let new_entry = AtlasEntry {
                    atlas_x,
                    atlas_y,
                    w: w_u,
                    h: h_u,
                    pen_left: 0,
                    pen_top: 0,
                };
                new_uploads.push((key, new_entry, staging));
                stats.glyph_uploads += 1;
                new_entry
            };

            if entry.w == 0 || entry.h == 0 {
                continue;
            }
            // Project glyph bbox into damage tracker.
            damage_min_x = damage_min_x.min(g.dst_x);
            damage_min_y = damage_min_y.min(g.dst_y);
            #[allow(clippy::cast_possible_wrap)]
            let max_x = g.dst_x.saturating_add(entry.w as i32);
            #[allow(clippy::cast_possible_wrap)]
            let max_y = g.dst_y.saturating_add(entry.h as i32);
            damage_max_x = damage_max_x.max(max_x);
            damage_max_y = damage_max_y.max(max_y);
            glyphs_to_draw.push(super::frame_builder::RecordedTextGlyph {
                atlas_x: entry.atlas_x,
                atlas_y: entry.atlas_y,
                w: entry.w,
                h: entry.h,
                dst_x: g.dst_x,
                dst_y: g.dst_y,
            });
        }

        // Commit new uploads + zero-inserts into the open frame.
        {
            let open = inner.frame_builder.open.as_mut().expect("open");
            for (key, entry, staging) in new_uploads.drain(..) {
                let staging_pin_idx = open.pins.pin_staging(Arc::clone(&staging));
                open.ops.push(super::frame_builder::RecordedOp::GlyphUpload(
                    super::frame_builder::RecordedGlyphUpload {
                        staging_pin_idx,
                        atlas_x: entry.atlas_x,
                        atlas_y: entry.atlas_y,
                        w: entry.w,
                        h: entry.h,
                        insert_key: key,
                        insert_entry: entry,
                    },
                ));
                open.pending_glyph_inserts.push(key, entry);
                open.glyph_uploads_in_frame = open.glyph_uploads_in_frame.saturating_add(1);
            }
            for (key, entry) in new_zero_inserts.drain(..) {
                open.pending_glyph_inserts.push(key, entry);
            }
        }

        // (9) If glyphs_to_draw is empty after processing → return Ok(stats).
        if glyphs_to_draw.is_empty() {
            return Ok(stats);
        }

        // (10) Damage: union of glyph dst-bboxes (append-time mutation, same
        //      as composite_glyphs_via_frame_builder). Frame failure does NOT
        //      roll back damage — the DamageNotify was already sent.
        if damage_max_x > damage_min_x && damage_max_y > damage_min_y {
            let dx = damage_min_x.max(0);
            let dy = damage_min_y.max(0);
            let w = u32::try_from(damage_max_x - dx).unwrap_or(0);
            let h = u32::try_from(damage_max_y - dy).unwrap_or(0);
            if w > 0 && h > 0 {
                store.damage(
                    target,
                    clamp_rect(
                        vk::Rect2D {
                            offset: vk::Offset2D { x: dx, y: dy },
                            extent: vk::Extent2D {
                                width: w,
                                height: h,
                            },
                        },
                        target_extent,
                    ),
                );
            }
        }

        // (11) Append RecordedOp::ImageText via push_op_and_set_layouts with
        //      (target, SHADER_READ_ONLY_OPTIMAL).
        let inner = self.inner.as_mut().expect("inner");
        {
            let open = inner.frame_builder.open.as_mut().expect("open");
            open.push_op_and_set_layouts(
                super::frame_builder::RecordedOp::ImageText(Box::new(
                    super::frame_builder::RecordedImageText {
                        dst_id: target,
                        dst_extent: target_extent,
                        dst_old_layout: dst_pre_frame_layout,
                        foreground_rgba,
                        glyphs: glyphs_to_draw,
                    },
                )),
                &[(target, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)],
            );
        }

        Ok(stats)
    }

    // ── Op: composite_glyphs (Stage 3d) ─────────────────────────

    /// Record a RENDER `CompositeGlyphs` against `dst`. Backend
    /// wrapper (`KmsBackendV2::render_composite_glyphs`) is
    /// responsible for: (a) gating on `op == Over` + SolidFill
    /// source (plan §3d "v1-parity scope"), (b) parsing the
    /// `items` glyph-element stream including the inline `0xFF 0
    /// mask_fmt new_gs` glyphset-change form, (c) looking up
    /// each glyph from `KmsCore.glyphsets`, (d) host-side A1→A8
    /// expansion. By the time we reach the engine, each input is a
    /// dense A8 bitmap + a dst position + a glyphset xid that
    /// keys it in the engine's atlas.
    ///
    /// `foreground_rgba` is the SolidFill source's premultiplied
    /// colour (the text pipeline shader multiplies it by the
    /// sampled atlas alpha — same blend state as 3a's image_text).
    ///
    /// `clip_rects` is the dst picture's clip set, already
    /// pre-shifted by the picture's `clip_x` / `clip_y` origin
    /// (Stage 3b). `None` paints the full dst; passing an empty
    /// slice paints nothing. Per plan §4, the engine emits one
    /// `cmd_set_scissor` + glyph-draw batch per clip rect — this
    /// is the v1-bug-fix: v1's `try_vk_render_composite_glyphs`
    /// reads the dst picture clip but ignores it
    /// (`kms::backend.rs:5313`).
    ///
    /// # Errors
    ///
    /// - `NoVk` on the stub engine.
    /// - `UnknownDrawable` if `dst_id` is missing.
    /// - `Vk(...)` for any CB / submit failure. Atlas-upload
    ///   failures drop the affected glyph and bump
    ///   `stats.glyphs_dropped`; only catastrophic failures (CB
    ///   alloc, draw-record) propagate.
    /// - `RendererFailed` if `platform.renderer_failed`.
    pub(crate) fn composite_glyphs(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        dst_id: DrawableId,
        foreground_rgba: [f32; 4],
        glyphs: &[CompositeGlyphInput<'_>],
        clip_rects: Option<&[Rectangle16]>,
    ) -> Result<ImageTextStats, RenderError> {
        // FrameBuilder-routed unconditionally. The pre-B.1 per-op-
        // submit legacy body and its `YSERVER_FRAME_BUILDER` kill-
        // switch were removed 2026-06-04: the off-path had bit-rotted
        // (close_open_frame asserted at startup) and there is no
        // non-frame-builder path anymore.
        self.composite_glyphs_via_frame_builder(
            store,
            platform,
            dst_id,
            foreground_rgba,
            glyphs,
            clip_rects,
        )
    }

    /// Phase B.1 Task 15: FrameBuilder-routed composite_glyphs.
    /// Defers per-glyph upload submits + the final draw submit into a
    /// single open frame; the frame closes via M2/M3/timeout/sync_wait/
    /// shutdown and submits all recorded ops as ONE `vkQueueSubmit2`.
    ///
    /// Codex-round walkthroughs preserved here:
    /// - R1 finding 2: flush cow/render batches FIRST so any
    ///   pre-existing batch CBs land chronologically before the
    ///   frame's draws.
    /// - R1 finding 3: snapshot dst pre_frame_layout in the
    ///   `FrameLayoutTable` overlay so rollback_pre_submit can write
    ///   it back on close failure.
    /// - R3 finding 2: count UNIQUE prospective misses in a pre-pass
    ///   to avoid premature close+reopen on a call with repeated
    ///   uncached keys.
    /// - R3 finding 2a: after close+reopen, recompute
    ///   pending_pins_before_call (pins reset to zero on reopen).
    /// - R4: pin-ceiling per-glyph check BEFORE `pack()` so dropped
    ///   glyphs don't leak shelf slots.
    /// - R5: pre-validate pixel length BEFORE `pack()` so malformed
    ///   input doesn't leak a slot either.
    /// - Damage mutation at append time — spec § "Damage accumulation"
    ///   mandates it (the client's request was already accepted).
    fn composite_glyphs_via_frame_builder(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        dst_id: DrawableId,
        foreground_rgba: [f32; 4],
        glyphs: &[CompositeGlyphInput<'_>],
        clip_rects: Option<&[Rectangle16]>,
    ) -> Result<ImageTextStats, RenderError> {
        let mut stats = ImageTextStats::default();
        if glyphs.is_empty() {
            return Ok(stats);
        }

        // (0) Flush pre-existing cow/render batches before opening the
        //     frame. Codex R1 finding 2: a pre-opened cow batch's CBs
        //     must submit BEFORE the frame's draws (chronological X11
        //     order). With M2 wired on every non-ported paint op,
        //     batches normally close before a frame opens — but the
        //     frame stays OPEN across composite_glyphs calls, so a
        //     sequence like `cow_copy_area → composite_glyphs` would
        //     see the cow batch pending; flush it here defensively.
        self.flush_render_batch(store, platform)?;

        // (1) Resolve dst format gating — identical to legacy.
        let inner = match self.inner.as_mut() {
            Some(i) => i,
            None => return Err(RenderError::NoVk),
        };
        if platform.renderer_failed {
            return Err(RenderError::RendererFailed);
        }
        let (dst_extent, dst_format) = {
            let d = store
                .get(dst_id)
                .ok_or(RenderError::UnknownDrawable(dst_id))?;
            (d.storage.extent, d.storage.format)
        };
        if dst_format != vk::Format::B8G8R8A8_UNORM {
            log::warn!(
                "v2 composite_glyphs (frame_builder): dst xid={:?} has format {:?}; \
                 text pipeline only supports B8G8R8A8_UNORM — dropping run",
                store.get(dst_id).map(|d| d.xid),
                dst_format,
            );
            return Ok(stats);
        }

        // (2) Lazy-init atlas + text pipeline — identical to legacy.
        if inner.glyph_atlas.is_none() {
            match V2GlyphAtlas::new(Arc::clone(&inner.vk)) {
                Ok(a) => inner.glyph_atlas = Some(a),
                Err(e) => {
                    log::error!(
                        "v2 composite_glyphs (frame_builder): V2GlyphAtlas::new failed: {e:?}"
                    );
                    return Err(RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED));
                }
            }
        }
        if inner.text_pipeline.is_none() {
            let atlas_view = inner.glyph_atlas.as_ref().expect("just built").image_view();
            match TextPipeline::new(
                Arc::clone(&inner.vk),
                vk::Format::B8G8R8A8_UNORM,
                atlas_view,
            ) {
                Ok(p) => inner.text_pipeline = Some(p),
                Err(e) => {
                    log::error!(
                        "v2 composite_glyphs (frame_builder): TextPipeline::new failed: {e:?}"
                    );
                    return Err(RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED));
                }
            }
        }

        // (3) Open the frame if not open. `submit_group_ticket_or_open`
        //     either returns the existing shared ticket (if a sibling
        //     op already opened the group) or opens a fresh one.
        //
        //     Phase B.2 Mechanism 2: bump `acquire_generation` once at
        //     open and capture the resulting value on the OpenFrame.
        //     Every descriptor acquisition during the open frame uses
        //     this captured value; the close-path SubmittedOp uses the
        //     same value.
        if !inner.frame_builder.is_open() {
            // Release the inner borrow before calling the platform
            // method which doesn't need it.
            let _ = inner;
            let ticket = platform.submit_group_ticket_or_open()?;
            let inner = self.inner.as_mut().expect("inner");
            inner.acquire_generation = inner.acquire_generation.saturating_add(1);
            let frame_generation = inner.acquire_generation;
            inner.frame_builder.open_for_paint(ticket, frame_generation);
        }
        let inner = self.inner.as_mut().expect("inner");

        // (4) Ticket-touch dst + snapshot prior ticket (first-touch
        //     only) + FIRST-TOUCH dst layout overlay (codex R1 finding
        //     3 fix — the overlay's pre_frame_layout is what
        //     `rollback_pre_submit` writes back on close-failure).
        let frame_ticket = inner
            .frame_builder
            .open
            .as_ref()
            .expect("just opened")
            .ticket
            .clone();
        let prior_dst_ticket = store.get(dst_id).and_then(|d| d.last_render_ticket.clone());
        let dst_pre_frame_layout = inner.current_layout_for_drawable(store, dst_id);
        {
            let open = inner.frame_builder.open.as_mut().expect("just opened");
            open.touched.first_touch(dst_id, prior_dst_ticket);
            open.layouts
                .first_touch_drawable(dst_id, dst_pre_frame_layout);
        }
        store.touch_render_fence(dst_id, frame_ticket.clone());

        // (5) Snapshot atlas prev ticket + atlas layout (first-touch
        //     only). The atlas snapshot is the rollback target if the
        //     close fails AFTER any upload op recorded; record_upload
        //     mutates `V2GlyphAtlas::current_layout` in place.
        {
            let atlas_pre_ticket: Option<FenceTicket> = inner
                .glyph_atlas
                .as_ref()
                .and_then(|a| a.last_render_ticket().cloned());
            let atlas_pre_layout: vk::ImageLayout = inner
                .glyph_atlas
                .as_ref()
                .map(super::glyph_atlas::V2GlyphAtlas::current_layout)
                .unwrap_or(vk::ImageLayout::UNDEFINED);
            let open = inner.frame_builder.open.as_mut().expect("open");
            if open.atlas_prev_ticket_snapshot.is_none() {
                open.atlas_prev_ticket_snapshot = Some(atlas_pre_ticket);
                open.layouts.first_touch_atlas(atlas_pre_layout);
            }
        }

        // (6a) PRE-PASS — count UNIQUE atlas misses without packing or
        //      allocating. Codex R3 finding 2: a call with repeated
        //      uncached keys would otherwise count N misses where one
        //      upload suffices, triggering premature close+reopens.
        //      Dedupe keys against (a) the committed atlas, (b) the
        //      frame's already-queued pending_glyph_inserts, (c)
        //      prior misses in THIS pre-pass.
        let pending_pins_before_call = inner
            .frame_builder
            .open
            .as_ref()
            .map(|o| o.pins.len())
            .unwrap_or(0);
        let ceiling = inner.frame_builder.max_pinned_resources_per_frame();
        let mut prospective_miss_keys: HashSet<GlyphKey> = HashSet::new();
        for g in glyphs {
            let key = GlyphKey {
                font_xid: g.gs_xid,
                codepoint: g.glyph_id,
            };
            if g.w == 0 || g.h == 0 {
                continue;
            }
            // (a) committed atlas hit?
            if inner
                .glyph_atlas
                .as_ref()
                .expect("init")
                .lookup(key)
                .is_some()
            {
                continue;
            }
            // (b) pending insert already queued in the open frame?
            let pending_hit = inner.frame_builder.open.as_ref().is_some_and(|o| {
                o.pending_glyph_inserts
                    .entries
                    .iter()
                    .any(|(k, _)| *k == key)
            });
            if pending_hit {
                continue;
            }
            // (c) duplicate within this call?
            prospective_miss_keys.insert(key);
        }
        let prospective_misses = prospective_miss_keys.len();
        let needs_close_reopen = pending_pins_before_call + prospective_misses > ceiling;
        if needs_close_reopen {
            // Force a close+reopen NOW (pre-allocation). Log the
            // ceiling hit once per process via note_pin_ceiling_hit_once.
            inner
                .frame_builder
                .note_pin_ceiling_hit_once(pending_pins_before_call + prospective_misses);
            // Release the inner borrow before calling close_open_frame
            // (which itself reborrows self). Conventional cue without
            // invoking `drop()` on a reference.
            let _ = inner;
            self.close_open_frame(
                store,
                platform,
                super::frame_builder::CloseReason::PinCeiling,
            )?;
            // Re-open a fresh frame. Phase B.2 Mechanism 2: bump
            // acquire_generation at open and capture the value on
            // the fresh OpenFrame (same shape as the initial open
            // above).
            let new_ticket = platform.submit_group_ticket_or_open()?;
            let inner = self.inner.as_mut().expect("inner");
            inner.acquire_generation = inner.acquire_generation.saturating_add(1);
            let frame_generation = inner.acquire_generation;
            inner
                .frame_builder
                .open_for_paint(new_ticket, frame_generation);
            let frame_ticket_reopened = inner
                .frame_builder
                .open
                .as_ref()
                .expect("just opened")
                .ticket
                .clone();
            let dst_pre_layout_reopened = inner.current_layout_for_drawable(store, dst_id);
            let atlas_pre_layout_reopened = inner
                .glyph_atlas
                .as_ref()
                .map(super::glyph_atlas::V2GlyphAtlas::current_layout)
                .unwrap_or(vk::ImageLayout::UNDEFINED);
            let atlas_pre_ticket_reopened = inner
                .glyph_atlas
                .as_ref()
                .and_then(|a| a.last_render_ticket().cloned());
            let prior_dst_reopened = store.get(dst_id).and_then(|d| d.last_render_ticket.clone());
            {
                let open = inner.frame_builder.open.as_mut().expect("open");
                open.touched.first_touch(dst_id, prior_dst_reopened);
                open.layouts
                    .first_touch_drawable(dst_id, dst_pre_layout_reopened);
                open.atlas_prev_ticket_snapshot = Some(atlas_pre_ticket_reopened);
                open.layouts.first_touch_atlas(atlas_pre_layout_reopened);
            }
            store.touch_render_fence(dst_id, frame_ticket_reopened);
            // If the SINGLE call still exceeds the ceiling — drop
            // excess glyphs. The spec accepts atlas-slot leakage in
            // the rare-failure regime; we extend that to "pathological
            // single call".
            if prospective_misses > ceiling {
                log::warn!(
                    "v2 composite_glyphs (frame_builder): single call requested {} \
                     atlas misses but per-frame ceiling is {}; will drop excess",
                    prospective_misses,
                    ceiling,
                );
            }
        }
        // Re-acquire `inner` for the per-glyph walk below. (Whether or
        // not we closed-and-reopened, the `inner` borrow was scoped.)
        let inner = self.inner.as_mut().expect("inner");
        // Recompute pending_pins_before_call AFTER any close+reopen.
        // On reopen, pins start at zero; without the recompute, the
        // per-glyph guard below would use the stale pre-close value
        // and prematurely drop glyphs (codex R3 finding 2a).
        let pending_pins_before_call = inner
            .frame_builder
            .open
            .as_ref()
            .map(|o| o.pins.len())
            .unwrap_or(0);

        // (6b) Per-glyph walk — actually allocate staging + pack atlas
        //      slots for each miss. Deduplicate against (a) committed
        //      atlas, (b) pending_glyph_inserts in the open frame,
        //      (c) new_uploads already collected in this walk. Stop
        //      allocating once the ceiling is hit (drop excess glyphs).
        let mut glyphs_to_draw: Vec<super::frame_builder::RecordedTextGlyph> =
            Vec::with_capacity(glyphs.len());
        let mut new_uploads: Vec<(GlyphKey, AtlasEntry, Arc<StagingBuffer>)> = Vec::new();
        let mut new_zero_inserts: Vec<(GlyphKey, AtlasEntry)> = Vec::new();
        let mut damage_min_x = i32::MAX;
        let mut damage_min_y = i32::MAX;
        let mut damage_max_x = i32::MIN;
        let mut damage_max_y = i32::MIN;
        for g in glyphs {
            let key = GlyphKey {
                font_xid: g.gs_xid,
                codepoint: g.glyph_id,
            };
            // (a) committed atlas hit?
            let committed_hit = inner.glyph_atlas.as_ref().expect("init").lookup(key);
            // (b) pending-insert hit in the open frame?
            let pending_hit = inner.frame_builder.open.as_ref().and_then(|o| {
                o.pending_glyph_inserts
                    .entries
                    .iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, e)| *e)
            });
            // (c) new-uploads dedupe (same call earlier)?
            let dedupe_hit = new_uploads
                .iter()
                .find(|(k, _, _)| *k == key)
                .map(|(_, e, _)| *e);
            let entry = if let Some(hit) = committed_hit.or(pending_hit).or(dedupe_hit) {
                hit
            } else {
                // Zero-size glyphs use a degenerate entry; no atlas
                // slot is consumed (the legacy path packs them anyway
                // but the returned slot is unused; we skip pack here
                // to avoid wasting one row on the packer).
                if g.w == 0 || g.h == 0 {
                    let e = AtlasEntry {
                        atlas_x: 0,
                        atlas_y: 0,
                        w: 0,
                        h: 0,
                        pen_left: 0,
                        pen_top: 0,
                    };
                    new_zero_inserts.push((key, e));
                    continue;
                }
                // Pin-ceiling enforcement: check BEFORE calling
                // `pack()` so dropped glyphs don't leak atlas slots
                // (codex R4: pack consumes a shelf advance regardless
                // of whether the glyph ends up uploaded).
                if new_uploads.len() + 1 + pending_pins_before_call > ceiling {
                    stats.glyphs_dropped += 1;
                    continue;
                }
                // Pre-validate pixels length BEFORE pack() to avoid
                // leaking a packed slot on malformed input (codex R5).
                let copy_len = (g.w as usize) * (g.h as usize);
                if g.pixels.len() < copy_len {
                    log::warn!(
                        "v2 composite_glyphs (frame_builder): glyph pixels {} < {}; \
                         dropping pre-pack",
                        g.pixels.len(),
                        copy_len,
                    );
                    stats.glyphs_dropped += 1;
                    continue;
                }
                let Some((atlas_x, atlas_y)) =
                    inner.glyph_atlas.as_mut().expect("init").pack(g.w, g.h)
                else {
                    inner.glyph_atlas.as_mut().expect("init").note_full_once();
                    stats.glyphs_dropped += 1;
                    continue;
                };
                stats.atlas_interns += 1;
                let upload_bytes = u64::from(g.w) * u64::from(g.h);
                let staging = Arc::new(StagingBuffer::new(
                    Arc::clone(&inner.vk),
                    upload_bytes.max(1),
                )?);
                let src_slice = &g.pixels[..copy_len];
                // SAFETY: staging is HOST_COHERENT, mapped for at
                // least `upload_bytes` bytes (clamped to 1 below);
                // `src_slice.len() == copy_len <= upload_bytes`.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        src_slice.as_ptr(),
                        staging.mapped.as_ptr(),
                        copy_len,
                    );
                }
                let new_entry = AtlasEntry {
                    atlas_x,
                    atlas_y,
                    w: g.w,
                    h: g.h,
                    pen_left: 0,
                    pen_top: 0,
                };
                new_uploads.push((key, new_entry, staging));
                stats.glyph_uploads += 1;
                new_entry
            };
            if entry.w == 0 || entry.h == 0 {
                continue;
            }
            damage_min_x = damage_min_x.min(g.dst_x);
            damage_min_y = damage_min_y.min(g.dst_y);
            #[allow(clippy::cast_possible_wrap)]
            let max_x = g.dst_x.saturating_add(entry.w as i32);
            #[allow(clippy::cast_possible_wrap)]
            let max_y = g.dst_y.saturating_add(entry.h as i32);
            damage_max_x = damage_max_x.max(max_x);
            damage_max_y = damage_max_y.max(max_y);
            glyphs_to_draw.push(super::frame_builder::RecordedTextGlyph {
                atlas_x: entry.atlas_x,
                atlas_y: entry.atlas_y,
                w: entry.w,
                h: entry.h,
                dst_x: g.dst_x,
                dst_y: g.dst_y,
            });
        }

        if glyphs_to_draw.is_empty() && new_uploads.is_empty() && new_zero_inserts.is_empty() {
            return Ok(stats);
        }

        // (6c) Commit new uploads + zero-inserts + glyph_uploads
        //      counter. Pin-ceiling enforcement happened in pre-pass +
        //      per-glyph drop above; we know
        //      new_uploads.len() ≤ ceiling - pending.
        {
            let open = inner.frame_builder.open.as_mut().expect("open");
            for (key, entry, staging) in new_uploads.drain(..) {
                let staging_pin_idx = open.pins.pin_staging(Arc::clone(&staging));
                open.ops.push(super::frame_builder::RecordedOp::GlyphUpload(
                    super::frame_builder::RecordedGlyphUpload {
                        staging_pin_idx,
                        atlas_x: entry.atlas_x,
                        atlas_y: entry.atlas_y,
                        w: entry.w,
                        h: entry.h,
                        insert_key: key,
                        insert_entry: entry,
                    },
                ));
                open.pending_glyph_inserts.push(key, entry);
                open.glyph_uploads_in_frame = open.glyph_uploads_in_frame.saturating_add(1);
            }
            for (key, entry) in new_zero_inserts.drain(..) {
                open.pending_glyph_inserts.push(key, entry);
            }
        }

        if glyphs_to_draw.is_empty() {
            return Ok(stats);
        }

        // (7) Build the clip scissor list — identical to legacy.
        let clip_scissors: Vec<vk::Rect2D> = match clip_rects {
            None => vec![vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: dst_extent,
            }],
            Some(cr) => {
                let mut out = Vec::with_capacity(cr.len());
                for r in cr {
                    if r.width == 0 || r.height == 0 {
                        continue;
                    }
                    let x0 = i32::from(r.x).max(0);
                    let y0 = i32::from(r.y).max(0);
                    let x1 = (i32::from(r.x) + i32::from(r.width))
                        .min(i32::try_from(dst_extent.width).unwrap_or(i32::MAX));
                    let y1 = (i32::from(r.y) + i32::from(r.height))
                        .min(i32::try_from(dst_extent.height).unwrap_or(i32::MAX));
                    if x1 <= x0 || y1 <= y0 {
                        continue;
                    }
                    out.push(vk::Rect2D {
                        offset: vk::Offset2D { x: x0, y: y0 },
                        extent: vk::Extent2D {
                            #[allow(clippy::cast_sign_loss)]
                            width: (x1 - x0) as u32,
                            #[allow(clippy::cast_sign_loss)]
                            height: (y1 - y0) as u32,
                        },
                    });
                }
                if out.is_empty() {
                    return Ok(stats);
                }
                out
            }
        };

        // (8) Append-time damage mutation. Spec § "Damage accumulation"
        //     mandates append-time mutation (the X11 client's request
        //     already happened the moment the server accepted it;
        //     DamageNotify fires on acceptance, before GPU work).
        //     Frame failure does NOT roll damage back — restoration
        //     would lose a DamageNotify the client has already been
        //     told about.
        if damage_max_x > damage_min_x && damage_max_y > damage_min_y {
            let dx = damage_min_x.max(0);
            let dy = damage_min_y.max(0);
            let w = u32::try_from(damage_max_x - dx).unwrap_or(0);
            let h = u32::try_from(damage_max_y - dy).unwrap_or(0);
            if w > 0 && h > 0 {
                store.damage(
                    dst_id,
                    clamp_rect(
                        vk::Rect2D {
                            offset: vk::Offset2D { x: dx, y: dy },
                            extent: vk::Extent2D {
                                width: w,
                                height: h,
                            },
                        },
                        dst_extent,
                    ),
                );
            }
        }

        // (9) Append the draw op. No damage_rect carried — damage was
        //     already mutated at append time above.
        inner
            .frame_builder
            .open
            .as_mut()
            .expect("open")
            .push_op_and_set_layouts(
                super::frame_builder::RecordedOp::CompositeGlyphs(
                    super::frame_builder::RecordedCompositeGlyphs {
                        dst_id,
                        dst_old_layout: dst_pre_frame_layout,
                        foreground_rgba,
                        glyphs: glyphs_to_draw,
                        clip_scissors,
                        damage_rect: None,
                    },
                ),
                &[(dst_id, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)],
            );

        // (10) Do NOT auto-close. Frame closes via M2 (next non-ported
        //      op), M3 (maybe_composite), timeout, sync_wait, or
        //      shutdown.
        Ok(stats)
    }

    // ── Op: render_composite (Stage 3c) ─────────────────────────

    /// Record a RENDER `Composite` against `dst`. `src` and `mask`
    /// are pre-resolved by the backend wrapper from the protocol
    /// `PictureRecord`. `rects` are pre-decoded composite quads
    /// in dst coords; `clip_rects` is the dst picture's clip set,
    /// already pre-shifted by the picture's `clip_x` / `clip_y`
    /// origin (Stage 3b's `set_picture_clip_rectangles` site does
    /// the shift). Passing `None` for `clip_rects` paints the
    /// full dst extent; passing an empty slice paints nothing.
    ///
    /// Stage 3c scope (per plan §3c):
    /// - Standard PictOps 0..=12 + Saturate (13) via fixed-function
    ///   blend; Disjoint (16..=27) + Conjoint (32..=43) via the
    ///   shader-side `dst_readback` blend.
    /// - Per-rect picture-clip scissoring — one draw call per
    ///   clip-rect intersection, **NOT** v1's union-bbox shortcut.
    /// - Self-aliasing (`src.drawable_id() == Some(dst_id)`):
    ///   handled via Stage 2d's [`allocate_scratch_image`] —
    ///   copy dst → scratch first, sample scratch_view.
    /// - Component-alpha pass through to the pipeline cache key.
    ///
    /// Deliberate v1 deviations / out-of-scope-for-3c gaps:
    /// - **Gradient sources**: gap log + bail (Stage 3e wires
    ///   gradient LUT build via `picture_paint`).
    /// - **Mask self-alias** (`mask.drawable_id() == Some(dst_id)`):
    ///   gap log + bail. Real apps don't hit this; if rendercheck
    ///   spots a case, fold into 3e alongside the gradient work.
    /// - **No ambient `current_clip` consultation** — RENDER ops
    ///   consult picture clip only (plan §4); the GC's
    ///   `current_clip` lives outside the engine call.
    ///
    /// # Errors
    ///
    /// - `NoVk` on the stub engine.
    /// - `UnknownDrawable` if `dst_id` is missing from `store`.
    /// - `Vk(...)` for any underlying pipeline / submit failure.
    /// - `RendererFailed` when `platform.renderer_failed`.
    ///
    /// Out-of-scope gating (unknown op, gradient source, mask
    /// self-alias, unsupported dst format) returns `Ok` with
    /// `recorded_draws = 0` — the op silently no-ops, matching
    /// v1's `try_vk_render_composite` shape.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render_composite(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        op: u8,
        src: ResolvedSource,
        mask: ResolvedSource,
        dst_id: DrawableId,
        rects: &[crate::kms::vk::ops::render::CompositeRect],
        clip_rects: Option<&[Rectangle16]>,
        src_repeat: Repeat,
        mask_repeat: Repeat,
        src_transform: Option<PictTransform>,
        mask_transform: Option<PictTransform>,
        mask_component_alpha: bool,
        src_pict_format: u32,
        mask_pict_format: u32,
        dst_pict_format: u32,
    ) -> Result<CompositeStats, RenderError> {
        // FrameBuilder-routed unconditionally. The pre-B.2 immediate-
        // submit legacy body and its
        // `YSERVER_FRAME_BUILDER_RENDER_COMPOSITE` kill-switch were
        // removed 2026-06-04 along with the main
        // `YSERVER_FRAME_BUILDER` gate: the off-paths had bit-rotted
        // and no non-frame-builder path exists anymore. No M2 close
        // here — this IS the frame builder; closing the open frame at
        // the top would defeat op collapse.
        self.render_composite_via_frame_builder(
            store,
            platform,
            op,
            src,
            mask,
            dst_id,
            rects,
            clip_rects,
            src_repeat,
            mask_repeat,
            src_transform,
            mask_transform,
            mask_component_alpha,
            src_pict_format,
            mask_pict_format,
            dst_pict_format,
        )
    }

    /// Phase B.2 Task 9: frame-builder composite path — prelude only.
    ///
    /// Implements Phase 9A (scratch peek + close-on-grow, NO state
    /// mutation yet) + Phase 9B (open frame + ticket-touch dst).
    /// Subsequent tasks (10-13) fill in src/mask resolution, scratch
    /// pinning, descriptor acquisition, op record, and emit.
    ///
    /// **Phase 9A — close-then-grow ordering is LOAD-BEARING** (USER-
    /// codex U-R10.F1). The grow must happen BEFORE any new frame
    /// opens. With no open frame at the time of `ensure_returning_old`,
    /// the engine's `adopt_retired_resource_for_gpu_retirement`
    /// helper falls through case (a) (open frame) and attaches the
    /// retired Box to `submitted.back` — the just-closed frame's
    /// `SubmittedOp` — so its `release(&vk)` rides the in-flight CB's
    /// fence rather than the about-to-open new frame's pin set.
    ///
    /// The dispatcher deliberately does NOT call the M2 close before
    /// invoking this: under sub-gate=ON this method IS the frame
    /// builder, so the open frame must remain open across consecutive
    /// composites.
    ///
    /// # Errors
    ///
    /// Same shape as [`Self::render_composite`].
    #[allow(clippy::too_many_arguments)]
    fn render_composite_via_frame_builder(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        op: u8,
        src: ResolvedSource,
        mask: ResolvedSource,
        dst_id: DrawableId,
        rects: &[crate::kms::vk::ops::render::CompositeRect],
        clip_rects: Option<&[Rectangle16]>,
        src_repeat: Repeat,
        mask_repeat: Repeat,
        src_transform: Option<PictTransform>,
        mask_transform: Option<PictTransform>,
        mask_component_alpha: bool,
        src_pict_format: u32,
        mask_pict_format: u32,
        dst_pict_format: u32,
    ) -> Result<CompositeStats, RenderError> {
        use crate::kms::vk::{ops::render as vk_render, render_pipeline::StdPictOp};

        let stats = CompositeStats::default();
        if rects.is_empty() {
            return Ok(stats);
        }

        // (0) Flush pre-existing cow/render batches so they submit
        //     under their own (per-op) ticket before this call opens a
        //     frame.
        self.flush_render_batch(store, platform)?;

        // (1) Lazy-init RENDER assets (pipelines, solid 1x1 images,
        //     scratch slots).
        self.ensure_render_assets(platform)?;

        // (2) PHASE 9A — scratch peek + close-on-grow. NO state
        //     mutation yet (beyond the assets ensure above which is
        //     idempotent + doesn't touch the open frame).
        //
        //     Resolve dst metadata. Scoped so the `&self.inner` borrow
        //     is released before any later `as_mut()` re-borrow.
        let (dst_image, dst_view, dst_extent, dst_format, dst_depth) = {
            let _inner = self.inner.as_ref().ok_or(RenderError::NoVk)?;
            if platform.renderer_failed {
                return Err(RenderError::RendererFailed);
            }
            let d = store
                .get(dst_id)
                .ok_or(RenderError::UnknownDrawable(dst_id))?;
            (
                d.storage.image,
                d.storage.image_view,
                d.storage.extent,
                d.storage.format,
                d.depth,
            )
        };
        if dst_extent.width == 0 || dst_extent.height == 0 {
            return Ok(stats);
        }
        if !matches!(
            dst_format,
            vk::Format::B8G8R8A8_UNORM | vk::Format::R8_UNORM
        ) {
            log::debug!(
                "v2 render_composite (frame_builder) gap: dst format \
                 {dst_format:?} not BGRA/R8 (dst id={dst_id:?})"
            );
            return Ok(stats);
        }
        let dst_has_alpha = dst_has_alpha_for_pict_format(dst_format, dst_depth, dst_pict_format);

        // Map the protocol op byte to the pipeline cache's enum.
        let Some(std_op) = StdPictOp::from_u8(op) else {
            log::debug!(
                "v2 render_composite (frame_builder) gap: unsupported op {op} \
                 (dst id={dst_id:?})"
            );
            return Ok(stats);
        };
        let needs_dst_readback = std_op.needs_dst_readback();
        let src_self_alias = matches!(src, ResolvedSource::Drawable(id) if id == dst_id);
        let mask_self_alias = matches!(mask, ResolvedSource::Drawable(id) if id == dst_id);
        let self_alias_used = src_self_alias || mask_self_alias;

        // (2a) PEEK growth. Both scratches (when needed) grow to
        //      (dst_format, dst_extent.width, dst_extent.height). If
        //      the slot is empty (`None`), `fits` defaults to false
        //      → grow.
        let need_grow_dst_rb = needs_dst_readback && {
            let inner = self.inner.as_ref().expect("inner");
            inner
                .dst_readback
                .as_ref()
                .map(|rb| !rb.fits(dst_format, dst_extent.width, dst_extent.height))
                .unwrap_or(true)
        };
        let need_grow_alias = self_alias_used && {
            let inner = self.inner.as_ref().expect("inner");
            inner
                .src_alias_readback
                .as_ref()
                .map(|rb| !rb.fits(dst_format, dst_extent.width, dst_extent.height))
                .unwrap_or(true)
        };

        // (2b) If growth would fire AND a frame is open with prior ops,
        //      close BEFORE touching anything for the current op.
        //      Pitfall 4 — guards record_copy_from at emit-time from
        //      writing into a scratch instance newer than the one the
        //      recorded views resolved against.
        if (need_grow_dst_rb || need_grow_alias) && {
            let inner = self.inner.as_ref().expect("inner");
            inner
                .frame_builder
                .open
                .as_ref()
                .is_some_and(|o| !o.ops.is_empty())
        } {
            self.close_open_frame(
                store,
                platform,
                super::frame_builder::CloseReason::ScratchGrow,
            )?;
        }

        // (2c) CRITICAL: grow + adopt BEFORE opening the new frame
        //      (USER-codex U-R10.F1). If we grew AFTER opening, the
        //      helper's case (a) would attach the retired Box to the
        //      NEW frame's pin set — a new-frame abort would then
        //      release Vk handles while the just-closed CB is still
        //      sampling them. With no open frame here, the helper
        //      falls through to case (b) and rides `submitted.back`'s
        //      fence (the just-closed frame's SubmittedOp).
        if need_grow_dst_rb {
            let retired = {
                let inner = self.inner.as_mut().expect("inner");
                inner
                    .dst_readback
                    .as_mut()
                    .expect("ensured")
                    .ensure_returning_old(dst_format, dst_extent.width, dst_extent.height)
                    .map_err(|e| {
                        log::warn!(
                            "v2 render_composite (frame_builder): dst_readback \
                             ensure failed: {e:?}"
                        );
                        RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
                    })?
            };
            let inner = self.inner.as_mut().expect("inner");
            inner.adopt_retired_resource_for_gpu_retirement(retired);
        }
        if need_grow_alias {
            let retired = {
                let inner = self.inner.as_mut().expect("inner");
                inner
                    .src_alias_readback
                    .as_mut()
                    .expect("ensured")
                    .ensure_returning_old(dst_format, dst_extent.width, dst_extent.height)
                    .map_err(|e| {
                        log::warn!(
                            "v2 render_composite (frame_builder): \
                             src_alias_readback ensure failed: {e:?}"
                        );
                        RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
                    })?
            };
            let inner = self.inner.as_mut().expect("inner");
            inner.adopt_retired_resource_for_gpu_retirement(retired);
        }

        // (3) PHASE 9B — open + ticket-touch dst. Scratch slots are
        //     now sized correctly; Task 10's view queries don't grow.
        //
        //     Phase B.2 Mechanism 2: bump `acquire_generation` once at
        //     open and capture the resulting value on the OpenFrame.
        let inner = self.inner.as_mut().expect("inner");
        if !inner.frame_builder.is_open() {
            // Release the inner borrow before calling the platform
            // method which doesn't need it.
            let _ = inner;
            let ticket = platform.submit_group_ticket_or_open()?;
            let inner = self.inner.as_mut().expect("inner");
            inner.acquire_generation = inner.acquire_generation.saturating_add(1);
            let frame_generation = inner.acquire_generation;
            inner.frame_builder.open_for_paint(ticket, frame_generation);
        }
        let inner = self.inner.as_mut().expect("inner");

        // (4) Ticket-touch dst + snapshot prior ticket + FIRST-TOUCH
        //     dst layout overlay (the overlay's pre_frame_layout is
        //     what `rollback_pre_submit` writes back on close-failure).
        let frame_ticket = inner
            .frame_builder
            .open
            .as_ref()
            .expect("just opened")
            .ticket
            .clone();
        let prior_dst_ticket = store.get(dst_id).and_then(|d| d.last_render_ticket.clone());
        let dst_pre_frame_layout = inner.current_layout_for_drawable(store, dst_id);
        {
            let open = inner.frame_builder.open.as_mut().expect("just opened");
            open.touched.first_touch(dst_id, prior_dst_ticket);
            open.layouts
                .first_touch_drawable(dst_id, dst_pre_frame_layout);
        }
        store.touch_render_fence(dst_id, frame_ticket.clone());

        // (5) Resolve solid scratch views directly — these are 1×1
        //     engine-owned `SolidColorImage`s that never grow, so no
        //     pin / no ticket-touch is needed (Pitfall 4b). Engine
        //     `Drop` destroys them at shutdown after all frames have
        //     closed.
        let inner = self.inner.as_ref().expect("inner");
        let solid_src_view = inner
            .solid_src_image
            .as_ref()
            .expect("ensured")
            .image_view();
        let solid_mask_view = inner
            .solid_mask_image
            .as_ref()
            .expect("ensured")
            .image_view();
        let white_mask_view = inner
            .white_mask_image
            .as_ref()
            .expect("ensured")
            .image_view();

        // (5b) Self-alias readback view (src or mask == dst). Phase
        //      9A already grew the scratch slot if needed; here we
        //      just query the view. `view()` takes `&mut self`
        //      because it may lazily build the no-alpha variant on
        //      first `dst_has_alpha=false` call against this scratch
        //      instance, so we re-borrow `inner` mutably.
        let src_alias_view = if self_alias_used {
            let inner = self.inner.as_ref().expect("inner");
            debug_assert!(
                inner.src_alias_readback.as_ref().is_some_and(|rb| rb.fits(
                    dst_format,
                    dst_extent.width,
                    dst_extent.height,
                )),
                "Phase 9A failed to grow src_alias_readback to required size",
            );
            let inner = self.inner.as_mut().expect("inner");
            match inner
                .src_alias_readback
                .as_mut()
                .expect("ensured")
                .view(dst_format, dst_has_alpha)
            {
                Ok(Some(v)) => Some(v),
                Ok(None) => {
                    log::warn!(
                        "v2 render_composite (frame_builder): \
                         src_alias_readback view None — skipping"
                    );
                    return Ok(stats);
                }
                Err(e) => {
                    log::warn!(
                        "v2 render_composite (frame_builder): \
                         src_alias_readback view build failed: {e:?}"
                    );
                    return Ok(stats);
                }
            }
        } else {
            None
        };

        // (6) dst_readback view when the op needs the shader-side
        //     blend (Disjoint/Conjoint). Phase 9A already grew if
        //     needed; same `&mut self` re-borrow as src_alias above.
        let dst_readback_view = if needs_dst_readback {
            let inner = self.inner.as_ref().expect("inner");
            debug_assert!(
                inner.dst_readback.as_ref().is_some_and(|rb| rb.fits(
                    dst_format,
                    dst_extent.width,
                    dst_extent.height,
                )),
                "Phase 9A failed to grow dst_readback to required size",
            );
            let inner = self.inner.as_mut().expect("inner");
            match inner
                .dst_readback
                .as_mut()
                .expect("ensured")
                .view(dst_format, dst_has_alpha)
            {
                Ok(Some(v)) => Some(v),
                Ok(None) => {
                    log::warn!(
                        "v2 render_composite (frame_builder): \
                         dst_readback view None — skipping"
                    );
                    return Ok(stats);
                }
                Err(e) => {
                    log::warn!(
                        "v2 render_composite (frame_builder): \
                         dst_readback view build failed: {e:?}"
                    );
                    return Ok(stats);
                }
            }
        } else {
            None
        };

        // (7) Resolve src view + extent + (optional) clear colour.
        //     Mirrors `render_composite_legacy` (Drawable / Solid /
        //     Gradient / None branches), with the addition of:
        //
        //     - per-Drawable `store.touch_render_fence` (frame-wide
        //       ticket pin),
        //     - per-Drawable `open.touched.first_touch` + layout
        //       `first_touch_drawable` snapshot for close-failure
        //       rollback.
        //
        //     dst was first-touched in step (4) above; we skip the
        //     touch when src/mask resolves to dst (self-alias case
        //     — the descriptor binding rides `src_alias_view`
        //     resolved in step 5b instead of the drawable view
        //     cache, so the cache lookup is skipped too).
        //
        //     Gradient sources resolve through `inner.picture_paint`
        //     which is engine-owned and CPU-immutable for the
        //     picture's lifetime (codex R3 finding 9). No ticket-
        //     touch / pin: the engine holds the LUT past frame
        //     close, and `picture_paint_remove` cannot run mid-paint.
        let mut src_clear_color: Option<[f32; 4]> = None;
        let mut mask_clear_color: Option<[f32; 4]> = None;
        let mut src_is_synthetic_1x1 = false;
        let mut mask_is_synthetic_1x1 = false;
        let mut src_picture_xform: Option<vk_render::AffineXform> = None;
        let mut mask_picture_xform: Option<vk_render::AffineXform> = None;

        let (src_view, src_extent) = if src_self_alias {
            // Self-alias: bind the alias scratch instead of dst's
            // drawable view. dst was already first-touched in
            // step (4); no additional touch here.
            (
                src_alias_view.expect("set when self_alias_used"),
                dst_extent,
            )
        } else {
            match src {
                ResolvedSource::Drawable(id) => {
                    // Snapshot prior + layout BEFORE first_touch so we
                    // capture the pre-frame state.
                    let prior = store.get(id).and_then(|d| d.last_render_ticket.clone());
                    let pre_layout = {
                        let inner = self.inner.as_ref().expect("inner");
                        inner.current_layout_for_drawable(store, id)
                    };
                    {
                        let inner = self.inner.as_mut().expect("inner");
                        let open = inner.frame_builder.open.as_mut().expect("just opened");
                        open.touched.first_touch(id, prior);
                        open.layouts.first_touch_drawable(id, pre_layout);
                    }
                    store.touch_render_fence(id, frame_ticket.clone());

                    let info = drawable_for_render_view(store, id)
                        .ok_or(RenderError::UnknownDrawable(id))?;
                    // Audit #4: pict_format-aware swizzle so an
                    // xRGB32 source on a depth-32 storage picks the
                    // BgraNoAlpha (force α=ONE) sample view.
                    let class =
                        swizzle_class_for_pict_format(info.format, info.depth, src_pict_format);
                    let sampler = sampler_config_for_repeat(src_repeat);
                    let inner = self.inner.as_mut().expect("inner");
                    let view = ensure_drawable_view(
                        &inner.vk,
                        &mut inner.drawable_view_cache,
                        id,
                        info.image,
                        info.format,
                        sampler,
                        class,
                    )?;
                    (view, info.extent)
                }
                ResolvedSource::Solid(color) => {
                    src_clear_color = Some(color);
                    src_is_synthetic_1x1 = true;
                    (
                        solid_src_view,
                        vk::Extent2D {
                            width: 1,
                            height: 1,
                        },
                    )
                }
                ResolvedSource::Gradient(xid) => {
                    let inner = self.inner.as_ref().expect("inner");
                    match inner.picture_paint.get(&xid) {
                        Some(PicturePaintState::Gradient(g)) => {
                            src_picture_xform = Some(g.axis_projection());
                            (g.image_view(), g.extent())
                        }
                        None => {
                            log::debug!(
                                "v2 render_composite (frame_builder) gap: \
                                 gradient picture 0x{xid:x} missing from \
                                 engine.picture_paint (LUT build likely failed)"
                            );
                            return Ok(stats);
                        }
                    }
                }
                ResolvedSource::None => {
                    log::debug!(
                        "v2 render_composite (frame_builder) gap: src is \
                         None (protocol requires src)"
                    );
                    return Ok(stats);
                }
            }
        };

        // (8) Resolve mask view + extent. Same shape as src.
        let (mask_view, mask_extent) = if mask_self_alias {
            (
                src_alias_view.expect("set when self_alias_used"),
                dst_extent,
            )
        } else {
            match mask {
                ResolvedSource::Drawable(id) => {
                    let prior = store.get(id).and_then(|d| d.last_render_ticket.clone());
                    let pre_layout = {
                        let inner = self.inner.as_ref().expect("inner");
                        inner.current_layout_for_drawable(store, id)
                    };
                    {
                        let inner = self.inner.as_mut().expect("inner");
                        let open = inner.frame_builder.open.as_mut().expect("just opened");
                        open.touched.first_touch(id, prior);
                        open.layouts.first_touch_drawable(id, pre_layout);
                    }
                    store.touch_render_fence(id, frame_ticket.clone());

                    let info = drawable_for_render_view(store, id)
                        .ok_or(RenderError::UnknownDrawable(id))?;
                    // Audit #4: same pict_format-aware swizzle as src.
                    let class =
                        swizzle_class_for_pict_format(info.format, info.depth, mask_pict_format);
                    let sampler = sampler_config_for_repeat(mask_repeat);
                    let inner = self.inner.as_mut().expect("inner");
                    let view = ensure_drawable_view(
                        &inner.vk,
                        &mut inner.drawable_view_cache,
                        id,
                        info.image,
                        info.format,
                        sampler,
                        class,
                    )?;
                    (view, info.extent)
                }
                ResolvedSource::Solid(color) => {
                    mask_clear_color = Some(color);
                    mask_is_synthetic_1x1 = true;
                    (
                        solid_mask_view,
                        vk::Extent2D {
                            width: 1,
                            height: 1,
                        },
                    )
                }
                ResolvedSource::Gradient(xid) => {
                    let inner = self.inner.as_ref().expect("inner");
                    match inner.picture_paint.get(&xid) {
                        Some(PicturePaintState::Gradient(g)) => {
                            mask_picture_xform = Some(g.axis_projection());
                            (g.image_view(), g.extent())
                        }
                        None => {
                            log::debug!(
                                "v2 render_composite (frame_builder) gap: \
                                 gradient mask picture 0x{xid:x} missing \
                                 from engine.picture_paint (LUT build likely \
                                 failed)"
                            );
                            return Ok(stats);
                        }
                    }
                }
                ResolvedSource::None => {
                    mask_is_synthetic_1x1 = true;
                    (
                        white_mask_view,
                        vk::Extent2D {
                            width: 1,
                            height: 1,
                        },
                    )
                }
            }
        };

        // (9) PHASE B.2 Task 11 Step 1: pipeline lookup + descriptor
        //     acquisition. The pipeline cache `get` takes `&mut self`
        //     (builds on cache-miss); release that borrow BEFORE
        //     reaching for `allocate_descriptor_for_views_into_ring`
        //     so the descriptor-pool-ring sibling borrow doesn't
        //     alias.
        let inner = self.inner.as_mut().expect("inner");
        let _pipeline_handle = inner
            .render_pipelines
            .as_mut()
            .expect("ensured")
            .get(std_op, dst_format, dst_has_alpha, mask_component_alpha)
            .map_err(|e| {
                log::warn!(
                    "v2 render_composite (frame_builder): pipeline build failed \
                     for op {op}: {e:?}"
                );
                RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
            })?;
        // Mechanism 2: every descriptor acquisition during the open
        // frame uses the captured `frame_generation`. Read it via the
        // OpenFrame (set at open time, not re-bumped per op).
        let frame_generation = inner
            .frame_builder
            .open
            .as_ref()
            .expect("just opened")
            .frame_generation;
        let src_for_descriptor = src_alias_view.unwrap_or(src_view);
        let mask_for_descriptor = mask_view;
        // Pitfall (Task 12 audit): when `!needs_dst_readback`, binding 2
        // (`dst_tex`) is bound but never sampled (the Disjoint/Conjoint
        // shader path is the only consumer). Match legacy's
        // `dst_readback_view.unwrap_or(white_mask_view)` shape here —
        // `white_mask_view` is engine-owned, sized 1×1, and always in
        // `SHADER_READ_ONLY_OPTIMAL` (transitioned once at backend
        // init), so it satisfies the descriptor write's declared image
        // layout. Earlier drafts used `dst_view` which is in
        // `COLOR_ATTACHMENT_OPTIMAL` between open / close — a latent
        // VUID-Vkpipeline-image-layout-mismatch waiting for validation
        // layers to trip on it.
        let dst_for_descriptor = dst_readback_view.unwrap_or(white_mask_view);
        let descriptor_set = inner
            .render_pipelines
            .as_ref()
            .expect("ensured")
            .allocate_descriptor_for_views_into_ring(
                &mut inner.descriptor_pool_ring,
                frame_generation,
                src_for_descriptor,
                mask_for_descriptor,
                dst_for_descriptor,
            )
            .map_err(RenderError::Vk)?;

        // (10) Step 2: resolve dst_old_layout via the overlay
        //      accessor. Pitfall 5 — for the 2nd op-in-frame, the
        //      overlay reflects op 1's post-op layout
        //      (SHADER_READ_ONLY_OPTIMAL); reading
        //      `store.get(dst_id).storage.current_layout` directly
        //      would return the STALE pre-frame value because
        //      storage is intentionally not mutated during recording.
        let inner = self.inner.as_ref().expect("inner");
        let dst_old_layout = inner.current_layout_for_drawable(store, dst_id);

        // (11) Step 3: build the replay-ready CompositeAttrs via the
        //      shared helper extracted from `_legacy`. The payload
        //      records this verbatim; close-time replay feeds it to
        //      `record_render_composite_draws` unchanged.
        let attrs = build_render_composite_attrs(
            store,
            &src,
            &mask,
            src_pict_format,
            mask_pict_format,
            src_extent,
            mask_extent,
            src_repeat,
            mask_repeat,
            src_is_synthetic_1x1,
            mask_is_synthetic_1x1,
            src_picture_xform,
            mask_picture_xform,
            src_transform.as_ref(),
            mask_transform.as_ref(),
        );

        // (12) Step 4: append RecordedOp::RenderComposite via the
        //      atomicity helper. Pitfall 6 / codex round 4 finding 3 —
        //      `push_op_and_set_layouts` is the ONLY path that mutates
        //      ops + overlay in tandem. The overlay update is ONE write
        //      per op, to the POST-op layout the recorder's close-
        //      transition will leave dst at (SHADER_READ_ONLY_OPTIMAL).
        //      No intermediate COLOR_ATTACHMENT_OPTIMAL write — that's
        //      an in-CB transient never observable across ops.
        let recorded = super::frame_builder::RecordedRenderComposite {
            op,
            dst_id,
            dst_image,
            dst_view,
            dst_extent,
            dst_format,
            dst_has_alpha,
            dst_old_layout,
            src_view,
            mask_view,
            src_alias_view,
            dst_readback_view,
            attrs,
            src_clear_color,
            mask_clear_color,
            mask_component_alpha,
            needs_dst_readback,
            rects: rects.to_vec().into_boxed_slice(),
            clip_rects: clip_rects.map(|r| r.to_vec().into_boxed_slice()),
            descriptor_set,
        };
        {
            let inner = self.inner.as_mut().expect("inner");
            let open = inner.frame_builder.open.as_mut().expect("just opened");
            open.push_op_and_set_layouts(
                super::frame_builder::RecordedOp::RenderComposite(Box::new(recorded)),
                &[(dst_id, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)],
            );
        }

        // (13) Step 5: damage bookkeeping + recorded-draws stat.
        //      Damage is committed AT APPEND TIME (matches `_legacy`
        //      shape) so subsequent damage queries from non-paint
        //      paths see the union eagerly; close-on-failure rolls
        //      damage back via the layout-overlay-rollback that
        //      `close_open_frame` performs on the touched set.
        let mut stats = stats;
        stats.recorded_draws = u32::try_from(rects.len()).unwrap_or(u32::MAX);
        for cr in rects {
            #[allow(clippy::cast_possible_wrap)]
            let rect = vk::Rect2D {
                offset: vk::Offset2D {
                    x: cr.dst_x,
                    y: cr.dst_y,
                },
                extent: vk::Extent2D {
                    width: cr.width,
                    height: cr.height,
                },
            };
            store.damage(dst_id, clamp_rect(rect, dst_extent));
        }

        Ok(stats)
    }

    // ── Op: render_fill_rectangles (Stage 3c) ───────────────────

    /// X RENDER `FillRectangles`: paint `rects` with a single
    /// premultiplied colour using PictOp `op`. Per plan §3c
    /// "Scope", this is `render_composite(op, SolidFill(color),
    /// NoMask, dst, ...)` — one composite with N rects.
    ///
    /// # Errors
    ///
    /// Same shape as [`render_composite`].
    pub(crate) fn render_fill_rectangles(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        op: u8,
        color: [f32; 4],
        dst_id: DrawableId,
        rects: &[crate::kms::vk::ops::render::CompositeRect],
        clip_rects: Option<&[Rectangle16]>,
    ) -> Result<CompositeStats, RenderError> {
        // Phase B Invariant M2: no wrapper-level
        // `close_open_frame_for_non_ported_op` here — this wrapper
        // delegates to `render_composite`, which IS the frame builder
        // (must NOT close). A wrapper-level close here would defeat
        // the collapse of two `render_fill_rectangles` calls into one
        // frame.
        self.render_composite(
            store,
            platform,
            op,
            ResolvedSource::Solid(color),
            ResolvedSource::None,
            dst_id,
            rects,
            clip_rects,
            Repeat::Pad,
            Repeat::Pad,
            None,
            None,
            false,
            // Audit #4: Solid src has no Picture context — depth
            // heuristic fallback is fine, force-opaque is false
            // anyway for non-Drawable sources.
            0,
            0,
            0,
        )
    }

    // ── Op: render_traps_or_tris (Stage 3e.2) ───────────────────

    /// GPU-rasterized RENDER `Trapezoids` / `Triangles`. Backend
    /// wrapper decodes the wire stream, applies `(x_off, y_off)`,
    /// computes the bounding box, and packs per-instance vertex
    /// data; the engine method takes those pre-cooked inputs and
    /// drives a two-stage CB: first the trap pipeline rasterizes
    /// analytic edge coverage into an R8 [`MaskScratch`] image,
    /// then the standard render pipeline composites `src ⊗ mask`
    /// into `dst`. Mirrors v1's `try_vk_render_traps_or_tris`
    /// (kms/backend.rs:4500) port — same trap pipeline + mask
    /// scratch infrastructure, adapted for v2's per-op CB shape.
    ///
    /// `bbox` is `(x, y, w, h)` in pixel coords (already clamped
    /// to non-negative by the wrapper). `prim_kind` selects which
    /// sibling pipeline to bind (trap edges vs triangle edges).
    ///
    /// Out-of-scope gating (unknown op, gradient src — Stage 3e
    /// gradient support hasn't landed yet, mask self-alias,
    /// unsupported dst format, src self-alias) returns
    /// `Ok(stats)` with `recorded_draws = 0` — same shape as
    /// `render_composite`. Source self-alias bails with a gap log
    /// (would need scratch routing à la 3c.3; rare in real-world
    /// trap workloads).
    ///
    /// # Errors
    ///
    /// - `NoVk` on the stub engine.
    /// - `UnknownDrawable` if `dst_id` is missing.
    /// - `Vk(...)` for pipeline / scratch / CB failures.
    /// - `RendererFailed` if `platform.renderer_failed`.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render_traps_or_tris(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        op: u8,
        src: ResolvedSource,
        dst_id: DrawableId,
        prim_kind: TrapPrimKind,
        instance_data: &[u8],
        instance_count: u32,
        bbox: (i32, i32, u32, u32),
        clip_rects: Option<&[Rectangle16]>,
        src_repeat: Repeat,
        src_transform: Option<PictTransform>,
        // Client xSrc/ySrc source-sampling origin, already shifted by
        // the caller's dst redirect / x_off delta (xSrc-dx, ySrc-dy).
        // Recorded into the payload; the emit folds in bbox for the
        // non-full-dst branch. Fixes RENDER Trapezoids/Triangles
        // dropping xSrc/ySrc (GTK CSD shadow blur-mask sampling).
        src_origin_x: i32,
        src_origin_y: i32,
        // Audit #4 (2026-05-19) — src/dst PictFormat IDs. Mirrors the
        // render_composite wiring: pict_format-aware α swizzle on the
        // sample view, pict_format-aware dst_has_alpha for pipeline +
        // readback selection. 0 = no Picture context → legacy depth
        // heuristic (the codex round 2026-05-19 follow-up to audit
        // #4 closed the trap/tri path that was originally missed).
        src_pict_format: u32,
        dst_pict_format: u32,
    ) -> Result<CompositeStats, RenderError> {
        use crate::kms::vk::render_pipeline::StdPictOp;

        // Phase B.3 (N5 + N9): empty-input fast-path — BEFORE
        // flush_render_batch and any other state mutation.
        let mut stats = CompositeStats::default();
        if instance_count == 0 {
            return Ok(stats);
        }
        let (bbox_x, bbox_y, bbox_w, bbox_h) = bbox;
        if bbox_w == 0 || bbox_h == 0 {
            return Ok(stats);
        }

        // N9 order: renderer_failed check.
        {
            let _inner = self.inner.as_ref().ok_or(RenderError::NoVk)?;
            if platform.renderer_failed {
                return Err(RenderError::RendererFailed);
            }
        }

        // N9 order: flush_render_batch before any state mutation.
        self.flush_render_batch(store, platform)?;

        // Lazy-init RENDER + TRAP assets (idempotent, preserves legacy).
        self.ensure_render_assets(platform)?;
        self.ensure_trap_assets(platform)?;

        // Preflight: resolve dst metadata.
        let (dst_image, dst_view, dst_extent, dst_format, dst_depth) = {
            let _inner = self.inner.as_ref().ok_or(RenderError::NoVk)?;
            let d = store
                .get(dst_id)
                .ok_or(RenderError::UnknownDrawable(dst_id))?;
            (
                d.storage.image,
                d.storage.image_view,
                d.storage.extent,
                d.storage.format,
                d.depth,
            )
        };
        if dst_extent.width == 0 || dst_extent.height == 0 {
            return Ok(stats);
        }
        if !matches!(
            dst_format,
            vk::Format::B8G8R8A8_UNORM | vk::Format::R8_UNORM
        ) {
            log::debug!("v2 render_traps_or_tris gap: dst format {dst_format:?} unsupported");
            return Ok(stats);
        }
        // Audit #4 (2026-05-19): pict_format-aware dst alpha.
        let dst_has_alpha = dst_has_alpha_for_pict_format(dst_format, dst_depth, dst_pict_format);
        let Some(std_op) = StdPictOp::from_u8(op) else {
            log::debug!("v2 render_traps_or_tris gap: unsupported op {op}");
            return Ok(stats);
        };
        let needs_dst_readback = std_op.needs_dst_readback();

        // Self-alias gate (preserve legacy at engine.rs:7271-7274).
        if matches!(src, ResolvedSource::Drawable(id) if id == dst_id) {
            log::debug!("v2 render_traps_or_tris gap: src self-alias (out of scope for 3e.2)");
            return Ok(stats);
        }

        // Step 5 (N8-style ordering): allocate vertex StagingBuffer FIRST,
        // before any open-frame state mutation. Allocation failure leaves
        // the frame untouched.
        let instance_buf = {
            let inner = self.inner.as_mut().expect("inner");
            let needed = u64::try_from(instance_data.len()).unwrap_or(0).max(1);
            let buf = StagingBuffer::new_with_usage(
                Arc::clone(&inner.vk),
                needed,
                vk::BufferUsageFlags::VERTEX_BUFFER,
            )?;
            unsafe {
                std::ptr::copy_nonoverlapping(
                    instance_data.as_ptr(),
                    buf.mapped.as_ptr(),
                    instance_data.len(),
                );
            }
            buf
        };

        // Step 6 (Phase 9A for mask_scratch): peek + close-before-grow + grow
        // + adopt. Mirrors render_composite_via_frame_builder at engine.rs:6539-6557.
        let need_grow_mask = {
            let inner = self.inner.as_ref().expect("inner");
            inner
                .mask_scratch
                .as_ref()
                .map(|s| !s.fits(bbox_w, bbox_h))
                .unwrap_or(true)
        };
        if need_grow_mask
            && self
                .inner
                .as_ref()
                .expect("inner")
                .frame_builder
                .open
                .as_ref()
                .is_some_and(|o| !o.ops.is_empty())
        {
            self.close_open_frame(
                store,
                platform,
                super::frame_builder::CloseReason::ScratchGrow,
            )?;
        }
        if need_grow_mask {
            let retired = {
                let inner = self.inner.as_mut().expect("inner");
                inner
                    .mask_scratch
                    .as_mut()
                    .expect("ensured")
                    .ensure_image_size_returning_old(bbox_w, bbox_h)
                    .map_err(|e| {
                        log::warn!("v2 render_traps_or_tris: mask ensure_image_size: {e:?}");
                        RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
                    })?
            };
            let inner = self.inner.as_mut().expect("inner");
            inner.adopt_retired_resource_for_gpu_retirement(retired);
        }

        // Step 7 (Phase 9A for dst_readback): peek + close-before-grow + grow
        // + adopt when std_op.needs_dst_readback().
        if needs_dst_readback {
            let need_grow_rb = {
                let inner = self.inner.as_ref().expect("inner");
                inner
                    .dst_readback
                    .as_ref()
                    .map(|rb| !rb.fits(dst_format, dst_extent.width, dst_extent.height))
                    .unwrap_or(true)
            };
            if need_grow_rb
                && self
                    .inner
                    .as_ref()
                    .expect("inner")
                    .frame_builder
                    .open
                    .as_ref()
                    .is_some_and(|o| !o.ops.is_empty())
            {
                self.close_open_frame(
                    store,
                    platform,
                    super::frame_builder::CloseReason::ScratchGrow,
                )?;
            }
            if need_grow_rb {
                let retired = {
                    let inner = self.inner.as_mut().expect("inner");
                    inner
                        .dst_readback
                        .as_mut()
                        .expect("ensured")
                        .ensure_returning_old(dst_format, dst_extent.width, dst_extent.height)
                        .map_err(|e| {
                            log::warn!("v2 render_traps_or_tris: dst readback ensure: {e:?}");
                            RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
                        })?
                };
                let inner = self.inner.as_mut().expect("inner");
                inner.adopt_retired_resource_for_gpu_retirement(retired);
            }
        }

        // Step 8: resolve append-time-stable fields per N5.

        // src_kind: match ResolvedSource variants.
        let src_kind = {
            let inner = self.inner.as_ref().expect("inner");
            match src {
                ResolvedSource::Drawable(id) => {
                    let info = drawable_for_render_view(store, id)
                        .ok_or(RenderError::UnknownDrawable(id))?;
                    let swizzle_class =
                        swizzle_class_for_pict_format(info.format, info.depth, src_pict_format);
                    super::frame_builder::RecordedTrapSrcKind::Drawable { id, swizzle_class }
                }
                ResolvedSource::Solid(color) => {
                    super::frame_builder::RecordedTrapSrcKind::Solid(color)
                }
                ResolvedSource::Gradient(xid) => match inner.picture_paint.get(&xid) {
                    Some(PicturePaintState::Gradient(g)) => {
                        // B.3 hotfix 2: clone the Arc so the recorded op holds
                        // a strong ref past picture_paint_remove. The clone is
                        // moved to pins.retired_resources at close time so it
                        // survives until the GPU fence fires.
                        let picture = g.clone();
                        let intrinsic_axis_projection = picture.axis_projection();
                        super::frame_builder::RecordedTrapSrcKind::Gradient {
                            picture,
                            intrinsic_axis_projection,
                        }
                    }
                    None => {
                        log::debug!(
                            "v2 render_traps_or_tris gap: gradient picture 0x{xid:x} \
                             missing from engine.picture_paint (LUT build likely failed)"
                        );
                        return Ok(stats);
                    }
                },
                ResolvedSource::None => {
                    log::debug!("v2 render_traps_or_tris gap: src None");
                    return Ok(stats);
                }
            }
        };

        // src_extent: needed for CompositeAttrs at emit time.
        let src_extent = match &src_kind {
            super::frame_builder::RecordedTrapSrcKind::Drawable { id, .. } => {
                drawable_for_render_view(store, *id)
                    .map(|info| info.extent)
                    .unwrap_or(vk::Extent2D {
                        width: 1,
                        height: 1,
                    })
            }
            super::frame_builder::RecordedTrapSrcKind::Solid(_) => vk::Extent2D {
                width: 1,
                height: 1,
            },
            super::frame_builder::RecordedTrapSrcKind::Gradient { picture, .. } => {
                // B.3 hotfix 2: extent is on the Arc clone; no HashMap lookup.
                picture.extent()
            }
        };
        let src_is_synthetic_1x1 = matches!(
            src_kind,
            super::frame_builder::RecordedTrapSrcKind::Solid(_)
        );

        // src_repeat: pre-resolve via repeat_to_shader_const.
        // The payload stores it as u32 (matches the spec field type); cast
        // from the helper's i32 return — the shader constants are 0..=3 and
        // always non-negative so the cast is lossless.
        #[allow(clippy::cast_sign_loss)]
        let src_repeat_const = crate::kms::backend::repeat_to_shader_const(src_repeat) as u32;

        // src_force_opaque via pict_format-aware helper.
        let src_force_opaque = resolve_force_opaque_pict_format(store, &src, src_pict_format);

        // user_src_xform via pixman_transform_to_affine.
        let user_src_xform =
            crate::kms::backend::pixman_transform_to_affine(src_transform.as_ref(), src_extent);

        // needs_full_dst byte-pattern test (engine.rs:7472).
        let needs_full_dst = matches!(op, 0 | 1 | 5 | 6 | 7 | 10 | 13 | 16..=27 | 32..=43);
        let (render_dst_x, render_dst_y, render_w, render_h) = if needs_full_dst {
            (0, 0, dst_extent.width, dst_extent.height)
        } else {
            (bbox_x, bbox_y, bbox_w, bbox_h)
        };

        // clip_scissors: pre-clamped at append.
        let clip_scissors: Vec<vk::Rect2D> = match clip_rects {
            None => vec![vk::Rect2D {
                offset: vk::Offset2D {
                    x: render_dst_x,
                    y: render_dst_y,
                },
                extent: vk::Extent2D {
                    width: render_w,
                    height: render_h,
                },
            }],
            Some(cr) => {
                let mut out = Vec::with_capacity(cr.len());
                for r in cr {
                    if r.width == 0 || r.height == 0 {
                        continue;
                    }
                    let x0 = i32::from(r.x).max(0);
                    let y0 = i32::from(r.y).max(0);
                    let x1 = (i32::from(r.x) + i32::from(r.width))
                        .min(i32::try_from(dst_extent.width).unwrap_or(i32::MAX));
                    let y1 = (i32::from(r.y) + i32::from(r.height))
                        .min(i32::try_from(dst_extent.height).unwrap_or(i32::MAX));
                    if x1 <= x0 || y1 <= y0 {
                        continue;
                    }
                    out.push(vk::Rect2D {
                        offset: vk::Offset2D { x: x0, y: y0 },
                        extent: vk::Extent2D {
                            #[allow(clippy::cast_sign_loss)]
                            width: (x1 - x0) as u32,
                            #[allow(clippy::cast_sign_loss)]
                            height: (y1 - y0) as u32,
                        },
                    });
                }
                if out.is_empty() {
                    return Ok(stats);
                }
                out
            }
        };

        // Step 9: open frame if not open.
        {
            let inner = self.inner.as_mut().expect("inner");
            if !inner.frame_builder.is_open() {
                let _ = inner;
                let ticket = platform.submit_group_ticket_or_open()?;
                let inner = self.inner.as_mut().expect("inner");
                inner.acquire_generation = inner.acquire_generation.saturating_add(1);
                let frame_generation = inner.acquire_generation;
                inner.frame_builder.open_for_paint(ticket, frame_generation);
            }
        }

        // Snapshot frame ticket.
        let frame_ticket = {
            let inner = self.inner.as_ref().expect("inner");
            inner
                .frame_builder
                .open
                .as_ref()
                .expect("just opened")
                .ticket
                .clone()
        };

        // Step 10: pin vertex StagingBuffer.
        let vertex_pool_pin = {
            let inner = self.inner.as_mut().expect("inner");
            let open = inner.frame_builder.open.as_mut().expect("just opened");
            open.pins.pin_staging(Arc::new(instance_buf))
        };

        // Step 11 (codex round-9 CRITICAL): prelude state for ALL TOUCHED DRAWABLES.
        // dst: first_touch + first_touch_drawable + touch_render_fence.
        let prior_dst_ticket = store.get(dst_id).and_then(|d| d.last_render_ticket.clone());
        let dst_pre_layout = {
            let inner = self.inner.as_ref().expect("inner");
            inner.current_layout_for_drawable(store, dst_id)
        };
        {
            let inner = self.inner.as_mut().expect("inner");
            let open = inner.frame_builder.open.as_mut().expect("just opened");
            open.touched.first_touch(dst_id, prior_dst_ticket);
            open.layouts.first_touch_drawable(dst_id, dst_pre_layout);
        }
        store.touch_render_fence(dst_id, frame_ticket.clone());

        // src (only when Drawable): SAME three mutations on the src DrawableId.
        // Skipping these is a lifetime bug per codex round-9 CRITICAL.
        if let super::frame_builder::RecordedTrapSrcKind::Drawable { id: src_id, .. } = src_kind {
            let prior_src_ticket = store.get(src_id).and_then(|d| d.last_render_ticket.clone());
            let src_pre_layout = {
                let inner = self.inner.as_ref().expect("inner");
                inner.current_layout_for_drawable(store, src_id)
            };
            {
                let inner = self.inner.as_mut().expect("inner");
                let open = inner.frame_builder.open.as_mut().expect("just opened");
                open.touched.first_touch(src_id, prior_src_ticket);
                open.layouts.first_touch_drawable(src_id, src_pre_layout);
            }
            store.touch_render_fence(src_id, frame_ticket.clone());
        }

        // Damage bookkeeping (coarse, matches legacy shape).
        let dmg = vk::Rect2D {
            offset: vk::Offset2D {
                x: render_dst_x,
                y: render_dst_y,
            },
            extent: vk::Extent2D {
                width: render_w,
                height: render_h,
            },
        };
        store.damage(dst_id, clamp_rect(dmg, dst_extent));
        stats.recorded_draws = u32::try_from(clip_scissors.len()).unwrap_or(u32::MAX);
        stats.used_dst_readback = needs_dst_readback;

        // Step 12: push_op_and_set_layouts. layouts_to_set includes
        // (dst, SHADER_READ_ONLY_OPTIMAL) always, AND (src, SHADER_READ_ONLY_OPTIMAL)
        // when src is Drawable.
        let payload = Box::new(super::frame_builder::RecordedRenderTrapsOrTris {
            dst_id,
            dst_image,
            dst_view,
            dst_old_layout: dst_pre_layout,
            dst_extent,
            dst_format,
            dst_has_alpha,
            std_op,
            op_byte: op,
            src_kind,
            src_extent,
            src_is_synthetic_1x1,
            src_repeat: src_repeat_const,
            src_force_opaque,
            user_src_xform,
            src_origin_x,
            src_origin_y,
            prim_kind,
            bbox_x,
            bbox_y,
            bbox_w,
            bbox_h,
            instance_count,
            clip_scissors,
            vertex_pool_pin,
        });
        {
            let inner = self.inner.as_mut().expect("inner");
            let open = inner.frame_builder.open.as_mut().expect("just opened");
            // Build the layouts_to_set slice. dst always; src when Drawable.
            if let super::frame_builder::RecordedTrapSrcKind::Drawable { id: src_id, .. } =
                payload.src_kind
            {
                open.push_op_and_set_layouts(
                    super::frame_builder::RecordedOp::RenderTrapsOrTris(payload),
                    &[
                        (dst_id, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL),
                        (src_id, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL),
                    ],
                );
            } else {
                open.push_op_and_set_layouts(
                    super::frame_builder::RecordedOp::RenderTrapsOrTris(payload),
                    &[(dst_id, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)],
                );
            }
        }

        Ok(stats)
    }
}

// ────────────────────────────────────────────────────────────────
// Stage 3c support: source resolution + drawable view cache.
// ────────────────────────────────────────────────────────────────

/// Stage 3e.2: primitive kind for [`RenderEngine::render_traps_or_tris`].
/// Selects which sibling of the trap pipeline to bind. Pre-cooked
/// instance data + count are passed alongside; the kind only
/// affects pipeline selection.
#[derive(Debug, Clone, Copy)]
pub(crate) enum TrapPrimKind {
    Trapezoid,
    Triangle,
}

/// Picture source resolved against `KmsCore.pictures` by the
/// backend wrapper. The engine doesn't read protocol records
/// directly; the wrapper hands it one of these.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ResolvedSource {
    /// Picture wraps a drawable; the engine samples its storage.
    Drawable(DrawableId),
    /// `RenderCreateSolidFill` source: a single premultiplied
    /// RGBA colour. Pipeline samples from a 1×1 scratch cleared
    /// to this colour per call.
    Solid([f32; 4]),
    /// Gradient placeholder (linear / radial). Stage 3c bails;
    /// 3e wires LUT build through `RenderEngine.picture_paint`.
    Gradient(u32),
    /// No mask (only valid as `mask`). Bound to the engine's
    /// white-mask scratch so `mask.a == 1.0` makes the blend a
    /// no-op.
    None,
}

/// Telemetry surface for one [`RenderEngine::render_composite`]
/// or [`RenderEngine::render_fill_rectangles`] call. The wrapper
/// pushes these into the per-second / lifetime telemetry sinks.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct CompositeStats {
    /// Whether the op took the `Disjoint`/`Conjoint` shader-side
    /// `dst_readback` path. Used to wire the
    /// `disjoint_readback_count` telemetry counter.
    pub used_dst_readback: bool,
    /// Whether the op took the Stage 3c.3 self-alias path
    /// (`src.drawable_id() == dst_id`, or same for mask). Tests
    /// assert this surfaces the scratch route; v1 had no observable
    /// signal for this case (the bug it was hiding).
    pub used_src_alias_scratch: bool,
    /// Total `vkCmdDraw` calls issued (rects × clip-rect
    /// intersections). Used by the acceptance harness to assert
    /// per-rect-scissor splits.
    pub recorded_draws: u32,
    /// Stage 5 Task 3 (render-composite generalization): the call
    /// was appended to a pending [`PendingRenderBatch`] rather
    /// than submitted as its own CB. Backend callers should
    /// suppress per-call `paint_submits` + `trace_simple` when
    /// this is `true`; the flush-time drain emits the events
    /// instead.
    pub deferred_to_batch: bool,
}

/// Snapshot of a drawable's view-relevant metadata. Lives only
/// long enough to build a `vk::ImageView` against it.
struct DrawableViewInfo {
    image: vk::Image,
    extent: vk::Extent2D,
    format: vk::Format,
    depth: u8,
}

/// X11 Render PictFormat force-opaque resolver.
///
/// Per the X11 Render spec, a Picture whose format has
/// `alpha_mask == 0` (e.g. a depth-24 RGB visual: r8g8b8 or
/// x8r8g8b8) must yield samples with `α = 1.0` regardless of the
/// byte content of the underlying storage. v2 stores depth-24
/// pixmaps as `B8G8R8A8_UNORM` with the α byte as server-owned
/// padding — without this override, marco/compositing samples the
/// padding byte (often 0) and the operator collapses to no-op,
/// leaving widget windows invisible under a compositing WM.
///
/// ## Gate: `depth == 24` BGRA storage only
///
/// Stage 4d landed this as `depth == 24` rather than the broader
/// `depth < 32` because depth-8 (A8 alpha-only pictures) and
/// depth-1 (bitmap masks) carry meaningful α in the X11 Render
/// `PictFormat` — forcing `α = 1.0` on a depth-8 mask would
/// silently turn coverage masks into solid blocks. Picture-
/// format-driven resolution (looking up the actual `PictFormat`
/// attached to the source picture rather than the drawable
/// depth) is the cleaner long-term shape; `depth == 24` is the
/// load-bearing case marco-with-compositing depends on, so we
/// fix that first and broaden later if a non-depth-24 picture
/// format with `alpha_mask == 0` shows up in a real workload.
///
/// `Solid` carries its own α, `Gradient` LUTs are authored with
/// the right α from `RenderCreateGradient`, and `None` is the
/// synthetic white-mask path — none of those need the override.
fn resolve_force_opaque(store: &DrawableStore, src: &ResolvedSource) -> bool {
    match src {
        ResolvedSource::Drawable(id) => store.get(*id).is_some_and(|d| d.depth == 24),
        ResolvedSource::Solid(_) | ResolvedSource::Gradient(_) | ResolvedSource::None => false,
    }
}

/// Audit #4 (2026-05-19) — pict_format-aware force-opaque decision.
/// A picture with PictFormat declaring `alpha_mask = 0` (xRGB24 or
/// xRGB32) says "the storage's α byte is padding, not client-
/// meaningful." Engine must force α=1 regardless of storage depth.
///
/// `pict_format == 0` falls back to the legacy depth heuristic for
/// engine-internal callers that synthesize sources without a real
/// Picture (composite_glyphs/trapezoids backfills). Both helpers
/// coexist: the pict_format-aware path threads through the
/// `render_composite` call site; the older `resolve_force_opaque`
/// stays for the synthesized-source paths.
fn resolve_force_opaque_pict_format(
    store: &DrawableStore,
    src: &ResolvedSource,
    pict_format: u32,
) -> bool {
    use yserver_protocol::x11::{RENDER_FMT_RGB24, RENDER_FMT_XRGB32};
    match src {
        ResolvedSource::Drawable(id) => {
            if pict_format == RENDER_FMT_RGB24 || pict_format == RENDER_FMT_XRGB32 {
                return true;
            }
            store.get(*id).is_some_and(|d| d.depth == 24)
        }
        ResolvedSource::Solid(_) | ResolvedSource::Gradient(_) | ResolvedSource::None => false,
    }
}

/// Phase B.2 Task 11 (USER-codex U-R11.F1+F2 / U-R12.F2): shared
/// `CompositeAttrs` builder lifted out of `render_composite_legacy` so
/// `render_composite_via_frame_builder` records an attrs payload that
/// reproduces the legacy pre-call construction byte-for-byte. The
/// recorded payload is replayed at close-time by
/// `record_render_composite_draws` (via `record_render_composite_open_with_old_layout`
/// in B.2 Task 12) so any divergence here would alter pixel output
/// relative to the pre-frame-builder path.
///
/// Inputs mirror the per-call locals the legacy body has already
/// resolved (synthetic-1x1 flags, gradient picture transforms, user
/// pict-transforms, pict-format-aware force-opaque flags). The helper
/// does NOT pack repeat / force-opaque into `RenderPushConsts`-style
/// bits — `record_render_composite_draws` handles that at emit time.
#[allow(clippy::too_many_arguments)]
fn build_render_composite_attrs(
    store: &DrawableStore,
    src: &ResolvedSource,
    mask: &ResolvedSource,
    src_pict_format: u32,
    mask_pict_format: u32,
    src_extent: vk::Extent2D,
    mask_extent: vk::Extent2D,
    src_repeat: Repeat,
    mask_repeat: Repeat,
    src_is_synthetic_1x1: bool,
    mask_is_synthetic_1x1: bool,
    src_picture_xform: Option<crate::kms::vk::ops::render::AffineXform>,
    mask_picture_xform: Option<crate::kms::vk::ops::render::AffineXform>,
    src_transform: Option<&PictTransform>,
    mask_transform: Option<&PictTransform>,
) -> crate::kms::vk::ops::render::CompositeAttrs {
    // Synthetic 1×1 scratches use PAD so the single texel covers the
    // whole rect. Otherwise pass the bare shader repeat constant.
    let effective_src_repeat = if src_is_synthetic_1x1 {
        crate::kms::vk::render_pipeline::REPEAT_PAD
    } else {
        crate::kms::backend::repeat_to_shader_const(src_repeat)
    };
    let effective_mask_repeat = if mask_is_synthetic_1x1 {
        crate::kms::vk::render_pipeline::REPEAT_PAD
    } else {
        crate::kms::backend::repeat_to_shader_const(mask_repeat)
    };

    // Compose gradient picture's intrinsic xform with the user's
    // RenderSetPictureTransform — matches v1's `compose_affines(
    // intrinsic, user)` shape.
    let user_src_xform = crate::kms::backend::pixman_transform_to_affine(src_transform, src_extent);
    let user_mask_xform =
        crate::kms::backend::pixman_transform_to_affine(mask_transform, mask_extent);
    let combined_src_xform = match src_picture_xform {
        Some(intrinsic) => crate::kms::backend::compose_affines(intrinsic, user_src_xform),
        None => user_src_xform,
    };
    let combined_mask_xform = match mask_picture_xform {
        Some(intrinsic) => crate::kms::backend::compose_affines(intrinsic, user_mask_xform),
        None => user_mask_xform,
    };

    let src_force_opaque = resolve_force_opaque_pict_format(store, src, src_pict_format);
    let mask_force_opaque = resolve_force_opaque_pict_format(store, mask, mask_pict_format);

    crate::kms::vk::ops::render::CompositeAttrs {
        src_extent,
        mask_extent,
        src_repeat: effective_src_repeat,
        mask_repeat: effective_mask_repeat,
        src_force_opaque,
        mask_force_opaque,
        src_xform: combined_src_xform,
        mask_xform: combined_mask_xform,
    }
}

fn drawable_for_render_view(store: &DrawableStore, id: DrawableId) -> Option<DrawableViewInfo> {
    let d = store.get(id)?;
    Some(DrawableViewInfo {
        image: d.storage.image,
        extent: d.storage.extent,
        format: d.storage.format,
        depth: d.depth,
    })
}

fn sampler_config_for_repeat(r: Repeat) -> SamplerConfig {
    match r {
        Repeat::None => SamplerConfig::Clamp,
        Repeat::Normal => SamplerConfig::Repeat,
        Repeat::Pad => SamplerConfig::Pad,
        Repeat::Reflect => SamplerConfig::Reflect,
    }
}

/// Map the pre-resolved shader repeat constant (see
/// `crate::kms::backend::repeat_to_shader_const`) back to the matching
/// `SamplerConfig`. The deferred trap/tri emit stores the repeat as the
/// shader constant in its payload, so the src view's Vk sampler must be
/// derived from it — mirroring how every non-deferred composite path
/// derives the sampler from the picture's `Repeat`. Defaults to `Clamp`
/// (REPEAT_NONE) for any unrecognised value.
fn sampler_config_for_shader_repeat(c: u32) -> SamplerConfig {
    use crate::kms::vk::render_pipeline::{REPEAT_NORMAL, REPEAT_PAD, REPEAT_REFLECT};
    if c == REPEAT_NORMAL as u32 {
        SamplerConfig::Repeat
    } else if c == REPEAT_PAD as u32 {
        SamplerConfig::Pad
    } else if c == REPEAT_REFLECT as u32 {
        SamplerConfig::Reflect
    } else {
        SamplerConfig::Clamp
    }
}

fn swizzle_class_for(format: vk::Format, depth: u8) -> SwizzleClass {
    match (format, depth) {
        (vk::Format::R8_UNORM, _) => SwizzleClass::AlphaOnlyR8,
        (vk::Format::B8G8R8A8_UNORM, 24) => SwizzleClass::BgraNoAlpha,
        _ => SwizzleClass::RgbaIdent,
    }
}

/// Audit #4 (2026-05-19) — pict_format-aware destination
/// `has_alpha` decision. A Picture wrapping a depth-32 storage
/// with `RENDER_FMT_XRGB32` declares `alpha_mask = 0` — the dst
/// storage's α byte is padding, NOT a client-meaningful alpha
/// channel. The pipeline + readback selection must treat it as
/// "no alpha target" (same as depth-24), else post-composite reads
/// of the padding bytes leak through to subsequent samples as
/// partial transparency.
///
/// Pre-fix `dst_has_alpha = dst_depth == 32` unconditionally. Now
/// the picture's PictFormat takes precedence over storage depth
/// when known (xRGB24 / xRGB32 → no alpha; ARGB32 → has alpha);
/// `pict_format == 0` falls back to the depth+format heuristic
/// for engine-internal callers without picture context.
fn dst_has_alpha_for_pict_format(format: vk::Format, depth: u8, pict_format: u32) -> bool {
    use yserver_protocol::x11::{RENDER_FMT_ARGB32, RENDER_FMT_RGB24, RENDER_FMT_XRGB32};
    // R8_UNORM dst is an A8 mask — alpha-only by definition,
    // pict_format can't override that.
    if format == vk::Format::R8_UNORM {
        return true;
    }
    if pict_format == RENDER_FMT_RGB24 || pict_format == RENDER_FMT_XRGB32 {
        return false;
    }
    if pict_format == RENDER_FMT_ARGB32 {
        return true;
    }
    // Fallback: legacy depth heuristic.
    depth == 32
}

/// Audit #4 (2026-05-19) — pict_format-aware swizzle. A picture
/// with PictFormat declaring `alpha_mask = 0` (xRGB24 or xRGB32)
/// must bind a sample view whose α swizzle pins to ONE, regardless
/// of storage depth. Pre-fix the engine cached one view per
/// (drawable, sampler, swizzle) tuple where swizzle came from
/// storage-depth alone — so a depth-32 storage wrapped by an
/// xRGB32 picture got `RgbaIdent` (pass-through), and the storage's
/// α padding bytes (typically 0) leaked into the composite as
/// transparent.
///
/// `pict_format == 0` falls back to `swizzle_class_for` for
/// internal engine paths that don't carry a Picture identity
/// (composite_glyphs synthesized A8 masks, trapezoid traps).
fn swizzle_class_for_pict_format(format: vk::Format, depth: u8, pict_format: u32) -> SwizzleClass {
    use yserver_protocol::x11::{RENDER_FMT_RGB24, RENDER_FMT_XRGB32};
    // R8_UNORM is alpha-only by construction — pict_format can't
    // override that. Same for the legacy depth-24 BGRA8 case.
    if format == vk::Format::R8_UNORM {
        return SwizzleClass::AlphaOnlyR8;
    }
    if format == vk::Format::B8G8R8A8_UNORM {
        if pict_format == RENDER_FMT_RGB24 || pict_format == RENDER_FMT_XRGB32 {
            return SwizzleClass::BgraNoAlpha;
        }
        if depth == 24 {
            return SwizzleClass::BgraNoAlpha;
        }
    }
    SwizzleClass::RgbaIdent
}

/// Lookup/build a `vk::ImageView` for `id` with the given
/// (sampler, swizzle) classification. The cache key splits on
/// SamplerConfig so a Repeat=None vs Repeat=Pad sample of the
/// same drawable doesn't share — Stage 3c uses Nearest only, so
/// sampler is "address mode" rather than full sampler state.
/// Address mode actually lives in the pipeline cache's sampler
/// (one shared linear sampler) — the cache split is therefore
/// over-engineered for 3c but matches the plan's published
/// (DrawableId, SamplerConfig, SwizzleClass) key, leaving room
/// for Stage 5's per-address-mode sampler splits without a
/// cache-shape rewrite.
fn ensure_drawable_view(
    vk: &VkContext,
    cache: &mut HashMap<(DrawableId, SamplerConfig, SwizzleClass), CachedDrawableView>,
    id: DrawableId,
    image: vk::Image,
    format: vk::Format,
    sampler: SamplerConfig,
    class: SwizzleClass,
) -> Result<vk::ImageView, vk::Result> {
    let key = (id, sampler, class);
    if let Some(c) = cache.get(&key) {
        return Ok(c.view);
    }
    let components = match class {
        SwizzleClass::RgbaIdent => vk::ComponentMapping {
            r: vk::ComponentSwizzle::IDENTITY,
            g: vk::ComponentSwizzle::IDENTITY,
            b: vk::ComponentSwizzle::IDENTITY,
            a: vk::ComponentSwizzle::IDENTITY,
        },
        SwizzleClass::AlphaOnlyR8 => vk::ComponentMapping {
            r: vk::ComponentSwizzle::ZERO,
            g: vk::ComponentSwizzle::ZERO,
            b: vk::ComponentSwizzle::ZERO,
            a: vk::ComponentSwizzle::R,
        },
        SwizzleClass::BgraNoAlpha => vk::ComponentMapping {
            r: vk::ComponentSwizzle::IDENTITY,
            g: vk::ComponentSwizzle::IDENTITY,
            b: vk::ComponentSwizzle::IDENTITY,
            a: vk::ComponentSwizzle::ONE,
        },
    };
    let info = vk::ImageViewCreateInfo::default()
        .image(image)
        .view_type(vk::ImageViewType::TYPE_2D)
        .format(format)
        .components(components)
        .subresource_range(
            vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .level_count(1)
                .layer_count(1),
        );
    let view = unsafe { vk.device.create_image_view(&info, None)? };
    cache.insert(key, CachedDrawableView { view });
    Ok(view)
}

/// Adapter implementing [`CompositeTarget`] over a v2 `Drawable`'s
/// storage fields. Built per-call by `render_composite`; the
/// recorder mutates `current_layout` and the caller reflects it
/// back into the Drawable's storage on success.
struct StorageCompositeTarget {
    extent: vk::Extent2D,
    image: vk::Image,
    image_view: vk::ImageView,
    current_layout: vk::ImageLayout,
}

impl CompositeTarget for StorageCompositeTarget {
    fn vk_image(&self) -> vk::Image {
        self.image
    }
    fn vk_image_view(&self) -> vk::ImageView {
        self.image_view
    }
    fn extent(&self) -> vk::Extent2D {
        self.extent
    }
    fn current_layout(&self) -> vk::ImageLayout {
        self.current_layout
    }
    fn set_current_layout(&mut self, layout: vk::ImageLayout) {
        self.current_layout = layout;
    }
}

/// CPU-rasterised glyph the caller hands to
/// [`RenderEngine::image_text`]. Mirrors v1's `RenderedGlyph`
/// shape, but living in the v2 engine module so the public type
/// surface is self-contained. `pixels` is row-major tightly
/// packed, `w × h` alpha bytes (FreeType `BITMAP_GRAY`).
#[derive(Debug)]
pub(crate) struct PreparedGlyph {
    pub dst_x: i32,
    pub dst_y: i32,
    pub w: usize,
    pub h: usize,
    pub pixels: Vec<u8>,
    pub codepoint: u32,
}

/// Single glyph input to [`RenderEngine::composite_glyphs`]. The
/// backend wrapper resolves glyphset xid + glyph id via
/// `KmsCore.glyphsets`, expands A1 bitmaps host-side to dense A8,
/// and computes the per-glyph dst position from the items stream's
/// running pen + glyph metrics. Lifetimes: `pixels` is borrowed
/// from `KmsCore.glyphsets[gs_xid].glyphs[glyph_id].pixels` (or a
/// caller-owned A1→A8 scratch); the engine copies it into a per-
/// glyph `StagingBuffer` on intern, so the borrow only needs to
/// outlive the engine call itself.
pub(crate) struct CompositeGlyphInput<'a> {
    /// Glyphset xid the glyph came from (atlas key namespace).
    pub gs_xid: u32,
    /// Glyph id within the glyphset (atlas key codepoint).
    pub glyph_id: u32,
    /// Glyph width / height. 0×0 entries cache an empty entry and
    /// skip the upload (space glyphs after pen-only adjustment).
    pub w: u32,
    pub h: u32,
    /// Dense A8 bitmap, row-major `w × h`. Caller is responsible
    /// for A1→A8 expansion + ARGB32→A8 alpha-extract (the latter
    /// is done in `parse_add_glyphs` for storage; the former is
    /// per-call because v1 stores A1 raw and expands at draw time).
    pub pixels: &'a [u8],
    /// Dst-space top-left corner for the glyph quad.
    pub dst_x: i32,
    pub dst_y: i32,
}

/// Telemetry surface for one [`RenderEngine::image_text`] call.
/// Caller (KmsBackendV2) feeds these into the telemetry sink so
/// `atlas_intern/s`, `glyph_uploads/s`, and the lifetime
/// `glyphs_dropped_atlas_full` counter all stay accurate.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ImageTextStats {
    pub atlas_interns: u32,
    pub glyph_uploads: u32,
    pub glyphs_dropped: u32,
}

/// Per-picture GPU-side state. Stage 3b only carries an empty
/// placeholder variant; Stage 3c adds `Gradient(GradientPicture)`
/// (the v1-side LUT-image type) when the first `render_composite`
/// against a gradient picture lazy-builds it.
/// Note: `GradientPicture` carries raw Vk handles + an `Arc<VkContext>`
/// and doesn't implement `Debug`, so this enum stays Debug-free.
pub(crate) enum PicturePaintState {
    /// GPU-side state for a `LinearGradient` / `RadialGradient`
    /// picture record. The wrapped [`GradientPicture`] owns its
    /// image / view / memory; dropping it (via
    /// [`RenderEngine::picture_paint_remove`] on `RenderFreePicture`)
    /// destroys the Vk resources. Built eagerly at
    /// `render_create_linear_gradient` / `render_create_radial_gradient`
    /// time so the first `render_composite` against the gradient
    /// has the LUT ready.
    Gradient(crate::kms::vk::gradient::GradientPicture),
}

/// Adapter implementing [`TextRunTarget`] over a v2 `Drawable`'s
/// storage fields. Built by [`RenderEngine::image_text`]; layout
/// changes performed by the recorder are read back into the
/// Drawable's storage by the caller.
struct StorageTextTarget {
    extent: vk::Extent2D,
    image: vk::Image,
    image_view: vk::ImageView,
    current_layout: vk::ImageLayout,
}

impl TextRunTarget for StorageTextTarget {
    fn vk_image(&self) -> vk::Image {
        self.image
    }
    fn vk_image_view(&self) -> vk::ImageView {
        self.image_view
    }
    fn extent(&self) -> vk::Extent2D {
        self.extent
    }
    fn current_layout(&self) -> vk::ImageLayout {
        self.current_layout
    }
    fn set_current_layout(&mut self, layout: vk::ImageLayout) {
        self.current_layout = layout;
    }
}

// Stage 3c: `record_render_composite` takes the same minimal
// paint-target surface as `record_text_run`. Impl `CompositeTarget`
// on the same adapter so v2's RENDER paint sites can hand the
// recorder a borrow over a `Drawable`'s storage fields.
impl CompositeTarget for StorageTextTarget {
    fn vk_image(&self) -> vk::Image {
        self.image
    }
    fn vk_image_view(&self) -> vk::ImageView {
        self.image_view
    }
    fn extent(&self) -> vk::Extent2D {
        self.extent
    }
    fn current_layout(&self) -> vk::ImageLayout {
        self.current_layout
    }
    fn set_current_layout(&mut self, layout: vk::ImageLayout) {
        self.current_layout = layout;
    }
}

impl Drop for RenderEngine {
    fn drop(&mut self) {
        // Best-effort drain — any submitted ops that didn't go
        // through `drain_all` would leak CBs. The `Drop` here
        // can't access the platform's pool any more, but it can
        // wait on each fence so `StagingBuffer`'s drop is safe.
        if let Some(inner) = self.inner.as_mut() {
            // Collect VkContext clone up front so we can release
            // BatchResources without borrow conflicts against the
            // submitted/pending_frames iteration below.
            let vk = Arc::clone(&inner.vk);
            // Drain cached drawable views. `notify_drawable_retired`
            // is the runtime per-drawable-destroy invalidation hook
            // but currently nobody calls it (filed as a separate
            // known-issue), so at shutdown the entire cache is
            // resident and every cached `VkImageView` would leak.
            // VkImageView is destroyable independently of its image
            // (Vulkan spec), so even cache entries whose underlying
            // image has already been destroyed via the runtime path
            // are safe to destroy here.
            for (_, cached) in inner.drawable_view_cache.drain() {
                unsafe { vk.device.destroy_image_view(cached.view, None) };
            }
            for mut op in inner.submitted.drain(..) {
                let _ = op.ticket.wait(&vk);
                // Phase B.2 Mechanism 3: explicit release of any
                // retired BatchResources attached to this op. Drop
                // would LEAK the underlying Vk handles
                // (BatchResource::release is `self: Box<Self>` —
                // paint_batch.rs:147). Must run BEFORE moving
                // `op.staging` out (the iterator hands us a `mut op`
                // and `drain_retired_scratch` requires `&mut op`).
                for r in op.drain_retired_scratch() {
                    r.release(&vk);
                }
                // staging drops here.
                drop(op.staging);
                // CB handles leak — caller should have invoked
                // `drain_all` against a live platform pool. The
                // pool's own Drop destroys the pool, which
                // implicitly frees all its CBs (Vulkan spec).
                let _ = op.cb;
            }
            for mut record in inner.pending_frames.drain(..) {
                let _ = record.ticket.wait(&vk);
                // Phase B.2 Mechanism 3 (defensive): release any
                // retired BatchResources attached to the frame's
                // pin set. See submitted loop above for rationale.
                for r in record.pins.retired_resources.drain(..) {
                    r.release(&vk);
                }
                drop(record); // pins (Arcs) decrement here
            }
        }
    }
}

// ────────────────────────────────────────────────────────────────
// Helpers: CB lifecycle, byte conversion, rect clipping.
// ────────────────────────────────────────────────────────────────

/// Allocate a fresh primary CB from the platform's
/// `OpsCommandPool`, begin recording, and acquire a
/// `FenceTicket` from the platform's fence pool. Returns
/// `(cb, ticket)` ready to record into.
fn begin_op_cb(
    inner: &mut RenderEngineInner,
    platform: &mut PlatformBackend,
) -> Result<(vk::CommandBuffer, FenceTicket), RenderError> {
    let pool = platform
        .ops_command_pool_handle()
        .ok_or(RenderError::NoVk)?;
    let device = &inner.vk.device;
    let alloc_info = vk::CommandBufferAllocateInfo::default()
        .command_pool(pool)
        .level(vk::CommandBufferLevel::PRIMARY)
        .command_buffer_count(1);
    let cb = unsafe { device.allocate_command_buffers(&alloc_info)? }[0];
    let begin =
        vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
    if let Err(e) = unsafe { device.begin_command_buffer(cb, &begin) } {
        // SAFETY: cb was just allocated from `pool`, never submitted;
        // safe to free in Initial state.
        unsafe { device.free_command_buffers(pool, &[cb]) };
        return Err(e.into());
    }
    // Phase A: shared ticket comes from the open submit group. With
    // max_size = 1, the group auto-closes after one append; with
    // max_size > 1 (post-Task 4), N appends share the same ticket.
    let ticket = match platform.submit_group_ticket_or_open() {
        Ok(t) => t,
        Err(e) => {
            // SAFETY: cb was begun but never submitted; safe to
            // free in Recording state.
            unsafe { device.free_command_buffers(pool, &[cb]) };
            return Err(RenderError::Vk(e));
        }
    };
    Ok((cb, ticket))
}

/// End CB recording, submit on the graphics queue with the
/// ticket's fence, return `Ok` on accept. Same-queue submission
/// order with the I6a fence is what Stage 2 plan cross-cutting
/// §3 banks on for paint→compose ordering without
/// `vkQueueWaitIdle`.
fn end_and_submit_op(
    inner: &mut RenderEngineInner,
    platform: &mut PlatformBackend,
    cb: vk::CommandBuffer,
    ticket: &FenceTicket,
) -> Result<(), RenderError> {
    end_and_submit_op_with_signal(inner, platform, cb, ticket, None)
}

fn end_and_submit_op_with_signal(
    inner: &mut RenderEngineInner,
    platform: &mut PlatformBackend,
    cb: vk::CommandBuffer,
    ticket: &FenceTicket,
    completion_signal: Option<vk::Semaphore>,
) -> Result<(), RenderError> {
    let device = &inner.vk.device;
    unsafe { device.end_command_buffer(cb)? };
    platform.submit_paint_cb_with_semaphore(cb, ticket.fence(), completion_signal)?;
    let _ = device;
    Ok(())
}

/// Phase B.1 Task 12: replay a single `RecordedOp` into `cb`. Caller
/// holds `&mut inner` and `&mut store`; this function consumes the
/// recorder-side state captured at append-time and emits the GPU
/// commands necessary to honour it.
fn emit_recorded_op_into_cb(
    inner: &mut RenderEngineInner,
    store: &mut DrawableStore,
    cb: vk::CommandBuffer,
    pins: &super::frame_builder::FramePinSet,
    frame_generation: u64,
    op: &super::frame_builder::RecordedOp,
) -> Result<(), RenderError> {
    use super::frame_builder::RecordedOp as Op;
    match op {
        Op::GlyphUpload(up) => {
            let atlas = inner.glyph_atlas.as_mut().ok_or(RenderError::NoVk)?;
            let staging_buffer = pins.staging_buffers[up.staging_pin_idx.0 as usize].buffer;
            atlas.record_upload(cb, staging_buffer, up.atlas_x, up.atlas_y, up.w, up.h);
            Ok(())
        }
        Op::CompositeGlyphs(cg) => {
            let atlas_extent = inner
                .glyph_atlas
                .as_ref()
                .ok_or(RenderError::NoVk)?
                .extent();
            // Clone the Vk handle owner so the recorder call doesn't
            // alias `inner.text_pipeline` against `&inner.vk`.
            let vk = inner.vk.clone();
            let drawable = store
                .get_mut(cg.dst_id)
                .ok_or(RenderError::UnknownDrawable(cg.dst_id))?;
            let glyphs_view: Vec<crate::kms::vk::ops::text::TextGlyph> = cg
                .glyphs
                .iter()
                .map(|g| crate::kms::vk::ops::text::TextGlyph {
                    entry: super::glyph_atlas::AtlasEntry {
                        atlas_x: g.atlas_x,
                        atlas_y: g.atlas_y,
                        w: g.w,
                        h: g.h,
                        pen_left: 0,
                        pen_top: 0,
                    },
                    dst_x: g.dst_x,
                    dst_y: g.dst_y,
                })
                .collect();
            let mut adapter = StorageTextTarget {
                extent: drawable.storage.extent,
                image: drawable.storage.image,
                image_view: drawable.storage.image_view,
                current_layout: cg.dst_old_layout,
            };
            let pipeline = inner.text_pipeline.as_ref().ok_or(RenderError::NoVk)?;
            crate::kms::vk::ops::text::record_text_run_scissored(
                &vk,
                cb,
                &mut adapter,
                crate::kms::vk::ops::text::TextAtlas {
                    extent: atlas_extent,
                },
                pipeline,
                &glyphs_view,
                cg.foreground_rgba,
                &cg.clip_scissors,
            )?;
            // Pipeline borrow ends here; mutate storage now.
            drawable.storage.current_layout = adapter.current_layout;
            Ok(())
        }
        Op::LayoutTransition(lt) => {
            let drawable = store
                .get_mut(lt.drawable_id)
                .ok_or(RenderError::UnknownDrawable(lt.drawable_id))?;
            drawable.record_layout_transition(
                &inner.vk,
                cb,
                lt.target_layout,
                lt.src_stage,
                lt.src_access,
                lt.dst_stage,
                lt.dst_access,
            );
            Ok(())
        }
        Op::RenderComposite(rc) => emit_recorded_render_composite_into_cb(inner, cb, pins, rc),
        // Phase B.3 — CopyArea implemented in Task 2; stubs for later tasks.
        Op::CopyArea(ca) => emit_recorded_copy_area_into_cb(inner, cb, ca),
        Op::PutImage(pi) => emit_recorded_put_image_into_cb(inner, cb, pins, pi),
        Op::FillRect(fr) => emit_recorded_fill_rect_into_cb(inner, store, cb, fr),
        Op::LogicFill(lf) => emit_recorded_logic_fill_into_cb(inner, store, cb, lf),
        Op::ImageText(it) => emit_recorded_image_text_into_cb(inner, store, cb, it),
        Op::RenderTrapsOrTris(rt) => {
            emit_recorded_render_traps_or_tris_into_cb(inner, store, cb, pins, frame_generation, rt)
        }
    }
}

/// Phase B.2 Task 12: replay a deferred `RecordedRenderComposite`
/// into the frame's command buffer. Mirrors `render_composite_legacy`'s
/// CB-recording shape (lines ~6200-6280) BUT:
///
/// - takes the dst's old layout from the **recorded payload** rather
///   than `Drawable::storage.current_layout` (Pitfall 5 — the latter is
///   stale during deferred recording across multiple ops in one frame),
/// - operates against a [`RecordedCompositeTarget`] adapter that holds
///   the pre-resolved image / view / extent (no `&mut DrawableStore`
///   read; the descriptor + views were resolved at append-time and
///   pinned by the frame).
///
/// The barrier emission is **identical** to the legacy path: exactly
/// one `to_color` (open) + one `to_read` (close). No double-barrier,
/// no manual barrier outside the recorder helpers. See plan §Task 12
/// Step 4 + Pitfall 5+6.
fn emit_recorded_render_composite_into_cb(
    inner: &mut RenderEngineInner,
    cb: vk::CommandBuffer,
    _pins: &super::frame_builder::FramePinSet,
    rc: &super::frame_builder::RecordedRenderComposite,
) -> Result<(), RenderError> {
    use crate::kms::vk::{
        ops::render as vk_render,
        render_pipeline::{StdPictOp, record_solid_color_clear},
    };

    // (1) Synthetic 1×1 src/mask clears (`record_solid_color_clear`
    //     internally transitions the scratch to SHADER_READ_ONLY).
    //     Per Pitfall 4b, the engine-owned `solid_src_image` /
    //     `solid_mask_image` are never grown — the same `SolidColorImage`
    //     handles the descriptor write at op-append captured. The clear
    //     fires per-op at emit time, rewriting the 1×1 texel for THIS
    //     op's source colour.
    if let Some(c) = rc.src_clear_color {
        let solid = inner.solid_src_image.as_mut().expect(
            "solid_src_image: ensure_render_assets ran in render_composite_via_frame_builder",
        );
        record_solid_color_clear(&inner.vk, cb, solid, c);
    }
    if let Some(c) = rc.mask_clear_color {
        let solid = inner.solid_mask_image.as_mut().expect(
            "solid_mask_image: ensure_render_assets ran in render_composite_via_frame_builder",
        );
        record_solid_color_clear(&inner.vk, cb, solid, c);
    }

    // (2) Self-alias copy: dst → src_alias_readback scratch. Same as
    //     legacy `render_composite_legacy` lines ~6223-6230. The copy
    //     RESTORES dst's old layout after the transfer (per
    //     `DstReadback::record_copy_from`'s contract), so the subsequent
    //     `to_color` open barrier sees the same `dst_old_layout` it
    //     would have seen without the scratch path.
    //
    //     Pitfall 4: under B.2 grow semantics, the `src_alias_readback`
    //     here is the SAME `DstReadback` instance the op-append site
    //     resolved its view against. Growth-during-frame is handled by
    //     the via_fb path's "close + grow + adopt + reopen" sequence
    //     before this emit runs.
    if rc.src_alias_view.is_some() {
        let rb = inner.src_alias_readback.as_mut().expect(
            "src_alias_readback: ensured at op-append in render_composite_via_frame_builder",
        );
        rb.record_copy_from(
            cb,
            rc.dst_image,
            rc.dst_old_layout,
            rc.dst_format,
            rc.dst_extent,
        );
    }

    // (2b) Shader-side dst readback copy: Saturate and the
    //      Disjoint/Conjoint families bind binding 2 (`dst_tex`) and
    //      expect it to contain a snapshot of dst before this op. The
    //      append path only ensures/resolves the scratch view and writes
    //      the descriptor; the actual transition+copy must replay here,
    //      in command-buffer order, before the draw samples it.
    if rc.needs_dst_readback {
        let rb = inner
            .dst_readback
            .as_mut()
            .expect("dst_readback: ensured at op-append in render_composite_via_frame_builder");
        rb.record_copy_from(
            cb,
            rc.dst_image,
            rc.dst_old_layout,
            rc.dst_format,
            rc.dst_extent,
        );
    }

    // (3) Pipeline lookup. The cache `get` takes `&mut self`; the borrow
    //     is released before the open barrier emission so `&inner.vk`
    //     can be re-borrowed safely.
    let std_op = StdPictOp::from_u8(rc.op).expect("op validated at append in via_frame_builder");
    let pipeline = inner
        .render_pipelines
        .as_mut()
        .expect("render_pipelines: ensured at op-append")
        .get(
            std_op,
            rc.dst_format,
            rc.dst_has_alpha,
            rc.mask_component_alpha,
        )
        .map_err(|e| {
            log::warn!("emit_recorded_render_composite: pipeline get failed: {e:?}");
            RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
        })?;
    let pipeline_layout = inner
        .render_pipelines
        .as_ref()
        .expect("render_pipelines: ensured at op-append")
        .pipeline_layout();

    // (4) Open: emit the dst `to_color` barrier using the pre-resolved
    //     overlay-driven old layout. Pitfall 5 — `record_render_composite_open`
    //     reads `dst.current_layout()` which is `storage.current_layout`
    //     (stale across multi-op frames); the `_with_old_layout` overload
    //     takes `old_layout` explicitly and does NOT mutate the target.
    let target = RecordedCompositeTarget {
        image: rc.dst_image,
        view: rc.dst_view,
        extent: rc.dst_extent,
    };
    vk_render::record_render_composite_open_with_old_layout(
        &inner.vk,
        cb,
        &target,
        pipeline,
        rc.dst_old_layout,
    )
    .map_err(RenderError::Vk)?;

    // (5) Per-rect draws. clip_rects=None → single full-extent scissor
    //     (matches legacy `build_render_clip_scissors`'s None branch).
    //     The full-extent fallback's `vk::Rect2D` is locally owned so
    //     its borrow lifetime is the function scope.
    let full_extent_scissor;
    let clip_scissors: &[vk::Rect2D] = match rc.clip_rects.as_deref() {
        Some(cr) => {
            // Important distinction:
            // - `None` => no picture clip, paint everywhere
            // - `Some([])` => empty picture clip, paint nothing
            //
            // B.2 originally collapsed both into the same fallback,
            // which let replayed ops redraw whole-frame damage after
            // `SetPictureClipRectangles(n=0)`.
            let owned = build_render_clip_scissors(Some(cr), rc.dst_extent);
            if owned.is_empty() {
                let mut target = target;
                vk_render::record_render_composite_close(&inner.vk, cb, &mut target);
                return Ok(());
            }
            full_extent_scissor = owned;
            full_extent_scissor.as_slice()
        }
        None => {
            full_extent_scissor = vec![vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: rc.dst_extent,
            }];
            full_extent_scissor.as_slice()
        }
    };
    vk_render::record_render_composite_draws(
        &inner.vk,
        cb,
        pipeline_layout,
        rc.descriptor_set,
        rc.dst_extent,
        &rc.attrs,
        &rc.rects,
        clip_scissors,
    );

    // (6) Close: emit `cmd_end_rendering` + dst `to_read` barrier back to
    //     `SHADER_READ_ONLY_OPTIMAL`. The recorder calls
    //     `target.set_current_layout(SHADER_READ_ONLY_OPTIMAL)` —
    //     intentional no-op on `RecordedCompositeTarget` (Pitfall 4b
    //     audit); storage layout commit happens via
    //     `commit_close_success`'s overlay walk on submit success.
    let mut target = target;
    vk_render::record_render_composite_close(&inner.vk, cb, &mut target);

    Ok(())
}

/// Phase B.3 Task 2 (N1, N8): replay a deferred `RecordedCopyArea` into the
/// frame's command buffer. Mirrors the legacy `copy_area` barrier shapes
/// EXACTLY: self-overlap path mirrors engine.rs:2814-2918 (three-barrier
/// sequence); disjoint path mirrors engine.rs:2951-3045 (two-barrier
/// sequence). Terminal layout for BOTH src and dst is
/// `SHADER_READ_ONLY_OPTIMAL` (N1 single-terminal-layout rule).
///
/// The exact stage/access masks mirror the legacy paths: the producer mask
/// (src_access on pre-barriers) is `SHADER_SAMPLED_READ | TRANSFER_WRITE |
/// COLOR_ATTACHMENT_WRITE` to drain prior compose/fill/put-image writes on the
/// same image — a simpler `TRANSFER_WRITE only` mask would recreate the
/// B.2-class RAW hazard.
///
/// The `self_overlap_scratch` image in the payload is allocated by the
/// `copy_area` append path (N8 allocation-first) and owned by
/// `RecordedCopyArea::self_overlap_scratch` until the close-path scratch walk
/// moves it into `SubmittedOp::scratch`. This function READS the scratch
/// but does NOT mutate its ownership — `ca` is `&RecordedCopyArea` (not `&mut`).
fn emit_recorded_copy_area_into_cb(
    inner: &mut RenderEngineInner,
    cb: vk::CommandBuffer,
    ca: &super::frame_builder::RecordedCopyArea,
) -> Result<(), RenderError> {
    let device = &inner.vk.device;
    if let Some(scratch) = ca.self_overlap_scratch.as_ref() {
        // Self-overlap: mirror engine.rs:2814-2918's three-barrier sequence.
        // (1) src → TRANSFER_SRC_OPTIMAL (drains prior compose/fill/put-image writes).
        barrier_to_layout(
            device,
            cb,
            ca.src_image,
            ca.src_old_layout,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            vk::PipelineStageFlags2::ALL_COMMANDS,
            vk::AccessFlags2::SHADER_SAMPLED_READ
                | vk::AccessFlags2::TRANSFER_WRITE
                | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            vk::PipelineStageFlags2::COPY,
            vk::AccessFlags2::TRANSFER_READ,
        );
        // (2) scratch UNDEFINED → TRANSFER_DST_OPTIMAL.
        barrier_to_layout(
            device,
            cb,
            scratch.image,
            vk::ImageLayout::UNDEFINED,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::PipelineStageFlags2::TOP_OF_PIPE,
            vk::AccessFlags2::empty(),
            vk::PipelineStageFlags2::COPY,
            vk::AccessFlags2::TRANSFER_WRITE,
        );
        // Copy src_rect → scratch (at offset 0,0).
        let region1 = [vk::ImageCopy::default()
            .src_subresource(color_layers())
            .src_offset(vk::Offset3D {
                x: ca.src_rect.offset.x,
                y: ca.src_rect.offset.y,
                z: 0,
            })
            .dst_subresource(color_layers())
            .dst_offset(vk::Offset3D { x: 0, y: 0, z: 0 })
            .extent(vk::Extent3D {
                width: ca.dst_rect.extent.width,
                height: ca.dst_rect.extent.height,
                depth: 1,
            })];
        unsafe {
            device.cmd_copy_image(
                cb,
                ca.src_image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                scratch.image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &region1,
            );
        }
        // (3a) scratch TRANSFER_DST → TRANSFER_SRC.
        barrier_to_layout(
            device,
            cb,
            scratch.image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            vk::PipelineStageFlags2::COPY,
            vk::AccessFlags2::TRANSFER_WRITE,
            vk::PipelineStageFlags2::COPY,
            vk::AccessFlags2::TRANSFER_READ,
        );
        // (3b) src (== dst) TRANSFER_SRC → TRANSFER_DST.
        barrier_to_layout(
            device,
            cb,
            ca.src_image,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::PipelineStageFlags2::COPY,
            vk::AccessFlags2::TRANSFER_READ,
            vk::PipelineStageFlags2::COPY,
            vk::AccessFlags2::TRANSFER_WRITE,
        );
        // Copy scratch → src (== dst) at dst_rect.
        let region2 = [vk::ImageCopy::default()
            .src_subresource(color_layers())
            .src_offset(vk::Offset3D { x: 0, y: 0, z: 0 })
            .dst_subresource(color_layers())
            .dst_offset(vk::Offset3D {
                x: ca.dst_rect.offset.x,
                y: ca.dst_rect.offset.y,
                z: 0,
            })
            .extent(vk::Extent3D {
                width: ca.dst_rect.extent.width,
                height: ca.dst_rect.extent.height,
                depth: 1,
            })];
        unsafe {
            device.cmd_copy_image(
                cb,
                scratch.image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                ca.src_image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &region2,
            );
        }
        // (4) src (== dst) → SHADER_READ_ONLY_OPTIMAL (N1 terminal-layout rule).
        barrier_to_layout(
            device,
            cb,
            ca.src_image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            vk::PipelineStageFlags2::COPY,
            vk::AccessFlags2::TRANSFER_WRITE,
            vk::PipelineStageFlags2::FRAGMENT_SHADER,
            vk::AccessFlags2::SHADER_SAMPLED_READ,
        );
        return Ok(());
    }

    // Disjoint case: two-barrier pre-sequence + copy + two-barrier post-sequence.
    // Pre-barriers: src → TRANSFER_SRC, dst → TRANSFER_DST (exact N1 masks).
    barrier_to_layout(
        device,
        cb,
        ca.src_image,
        ca.src_old_layout,
        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
        vk::PipelineStageFlags2::ALL_COMMANDS,
        vk::AccessFlags2::SHADER_SAMPLED_READ
            | vk::AccessFlags2::TRANSFER_WRITE
            | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
        vk::PipelineStageFlags2::COPY,
        vk::AccessFlags2::TRANSFER_READ,
    );
    barrier_to_layout(
        device,
        cb,
        ca.dst_image,
        ca.dst_old_layout,
        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
        vk::PipelineStageFlags2::ALL_COMMANDS,
        vk::AccessFlags2::SHADER_SAMPLED_READ
            | vk::AccessFlags2::TRANSFER_WRITE
            | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
        vk::PipelineStageFlags2::COPY,
        vk::AccessFlags2::TRANSFER_WRITE,
    );
    let region = [vk::ImageCopy::default()
        .src_subresource(color_layers())
        .src_offset(vk::Offset3D {
            x: ca.src_rect.offset.x,
            y: ca.src_rect.offset.y,
            z: 0,
        })
        .dst_subresource(color_layers())
        .dst_offset(vk::Offset3D {
            x: ca.dst_rect.offset.x,
            y: ca.dst_rect.offset.y,
            z: 0,
        })
        .extent(vk::Extent3D {
            width: ca.dst_rect.extent.width,
            height: ca.dst_rect.extent.height,
            depth: 1,
        })];
    unsafe {
        device.cmd_copy_image(
            cb,
            ca.src_image,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            ca.dst_image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &region,
        );
    }
    // Post-barriers: BOTH src and dst → SHADER_READ_ONLY_OPTIMAL (N1).
    barrier_to_layout(
        device,
        cb,
        ca.src_image,
        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        vk::PipelineStageFlags2::COPY,
        vk::AccessFlags2::TRANSFER_READ,
        vk::PipelineStageFlags2::FRAGMENT_SHADER,
        vk::AccessFlags2::SHADER_SAMPLED_READ,
    );
    barrier_to_layout(
        device,
        cb,
        ca.dst_image,
        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        vk::PipelineStageFlags2::COPY,
        vk::AccessFlags2::TRANSFER_WRITE,
        vk::PipelineStageFlags2::FRAGMENT_SHADER,
        vk::AccessFlags2::SHADER_SAMPLED_READ,
    );
    Ok(())
}

/// Phase B.3 Task 6 (N1 + N2): replay a deferred `RecordedPutImage` into
/// the frame's command buffer. Staging buffer handle is read from the
/// frame pin-set (N2 — index pre-recorded at append time). Barrier shape
/// mirrors the legacy `put_image` body (engine.rs pre-B.3 lines ~3684-3731):
///
/// - Pre-barrier: dst `old_layout` → `TRANSFER_DST_OPTIMAL` with
///   `ALL_COMMANDS / SHADER_SAMPLED_READ | COLOR_ATTACHMENT_WRITE`
///   producer mask (N1 — drains prior compose/fill/paint writes).
/// - `cmd_copy_buffer_to_image` from the pinned staging buffer.
/// - Post-barrier: dst `TRANSFER_DST_OPTIMAL` → `SHADER_READ_ONLY_OPTIMAL`
///   (N1 terminal layout).
fn emit_recorded_put_image_into_cb(
    inner: &mut RenderEngineInner,
    cb: vk::CommandBuffer,
    pins: &super::frame_builder::FramePinSet,
    pi: &super::frame_builder::RecordedPutImage,
) -> Result<(), RenderError> {
    let device = &inner.vk.device;
    // N1 put_image pre-barrier (DST only — staging buffers have no layout).
    // Mirrors engine.rs:3684-3692 producer mask: SHADER_SAMPLED_READ |
    // COLOR_ATTACHMENT_WRITE drains any prior compose/fill/put-image writes
    // to this drawable.
    barrier_to_layout(
        device,
        cb,
        pi.dst_image,
        pi.dst_old_layout,
        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
        vk::PipelineStageFlags2::ALL_COMMANDS,
        vk::AccessFlags2::SHADER_SAMPLED_READ | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
        vk::PipelineStageFlags2::COPY,
        vk::AccessFlags2::TRANSFER_WRITE,
    );
    // N2: read the staging buffer handle from the frame pin-set.
    let staging_buffer = pins.staging_buffers[pi.staging_pin_idx.0 as usize].buffer;
    let region = [vk::BufferImageCopy::default()
        .buffer_offset(0)
        .buffer_row_length(0)
        .buffer_image_height(0)
        .image_subresource(
            vk::ImageSubresourceLayers::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .layer_count(1),
        )
        .image_offset(vk::Offset3D {
            x: pi.dst_rect.offset.x,
            y: pi.dst_rect.offset.y,
            z: 0,
        })
        .image_extent(vk::Extent3D {
            width: pi.dst_rect.extent.width,
            height: pi.dst_rect.extent.height,
            depth: 1,
        })];
    unsafe {
        device.cmd_copy_buffer_to_image(
            cb,
            staging_buffer,
            pi.dst_image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &region,
        );
    }
    // N1 post-barrier: dst → SHADER_READ_ONLY_OPTIMAL (terminal layout).
    // Mirrors engine.rs:3723-3731.
    barrier_to_layout(
        device,
        cb,
        pi.dst_image,
        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        vk::PipelineStageFlags2::COPY,
        vk::AccessFlags2::TRANSFER_WRITE,
        vk::PipelineStageFlags2::FRAGMENT_SHADER,
        vk::AccessFlags2::SHADER_SAMPLED_READ,
    );
    Ok(())
}

/// Phase B.3 Task 8: replay a deferred `RecordedFillRect` into the
/// frame's command buffer. Uses `cmd_clear_attachments` directly —
/// NO composite pipeline, NO descriptor (codex round-7 catch —
/// earlier drafts erroneously routed through composite).
///
/// `load_op = LOAD` is LOAD-BEARING per N4: outside-rect pixels must
/// be preserved. `DONT_CARE` would invalidate the entire render area.
///
/// Pre-barrier producer mask mirrors the legacy path at the old
/// engine.rs fill_rect_batch body: `ALL_COMMANDS /
/// SHADER_SAMPLED_READ | TRANSFER_WRITE | COLOR_ATTACHMENT_WRITE`
/// drains any prior compose reads / put_image writes / fill writes
/// on the same image.
fn emit_recorded_fill_rect_into_cb(
    inner: &mut RenderEngineInner,
    store: &DrawableStore,
    cb: vk::CommandBuffer,
    fr: &super::frame_builder::RecordedFillRect,
) -> Result<(), RenderError> {
    let device = &inner.vk.device;
    // Resolve the dst vk::Image at emit time — the payload carries
    // dst_id so we can look it up from the store. The storage image is
    // stable for the drawable's lifetime; no invalidation risk.
    let dst_image = store
        .get(fr.dst_id)
        .ok_or(RenderError::UnknownDrawable(fr.dst_id))?
        .storage
        .image;

    // FILL pre-barrier: drain prior writes / reads on dst, then
    // transition to COLOR_ATTACHMENT_OPTIMAL for the clear pass.
    // Mirror the legacy fill_rect_batch barrier at the old body.
    barrier_to_layout(
        device,
        cb,
        dst_image,
        fr.dst_old_layout,
        vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        vk::PipelineStageFlags2::ALL_COMMANDS,
        vk::AccessFlags2::SHADER_SAMPLED_READ
            | vk::AccessFlags2::TRANSFER_WRITE
            | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
        vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
        // LOAD-bearing render passes need attachment read access on open,
        // same as the compositor path in vk::ops::render.
        vk::AccessFlags2::COLOR_ATTACHMENT_READ | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
    );

    let render_area = vk::Rect2D {
        offset: vk::Offset2D::default(),
        extent: fr.dst_extent,
    };
    let color_attachment = [vk::RenderingAttachmentInfo::default()
        .image_view(fr.dst_image_view)
        .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .load_op(vk::AttachmentLoadOp::LOAD) // LOAD-BEARING per N4
        .store_op(vk::AttachmentStoreOp::STORE)];
    let rendering_info = vk::RenderingInfo::default()
        .render_area(render_area)
        .layer_count(1)
        .color_attachments(&color_attachment);
    let attachments = [vk::ClearAttachment::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .color_attachment(0)
        .clear_value(vk::ClearValue {
            color: vk::ClearColorValue { float32: fr.color },
        })];
    let clear_rects: Vec<vk::ClearRect> = fr
        .rects
        .iter()
        .map(|r| {
            vk::ClearRect::default()
                .rect(*r)
                .base_array_layer(0)
                .layer_count(1)
        })
        .collect();
    unsafe {
        device.cmd_begin_rendering(cb, &rendering_info);
        let viewport = [vk::Viewport {
            x: 0.0,
            y: 0.0,
            #[allow(clippy::cast_precision_loss)]
            width: fr.dst_extent.width as f32,
            #[allow(clippy::cast_precision_loss)]
            height: fr.dst_extent.height as f32,
            min_depth: 0.0,
            max_depth: 1.0,
        }];
        device.cmd_set_viewport(cb, 0, &viewport);
        let scissor = [render_area];
        device.cmd_set_scissor(cb, 0, &scissor);
        device.cmd_clear_attachments(cb, &attachments, &clear_rects);
        device.cmd_end_rendering(cb);
    }

    // Post-barrier: dst → SHADER_READ_ONLY_OPTIMAL (N1 terminal layout).
    barrier_to_layout(
        device,
        cb,
        dst_image,
        vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
        vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
        vk::PipelineStageFlags2::FRAGMENT_SHADER,
        vk::AccessFlags2::SHADER_SAMPLED_READ,
    );
    Ok(())
}

/// Phase B.3 Task 10: replay a `RecordedLogicFill` into the frame CB.
///
/// Mirrors engine.rs pre-B.3 `logic_fill` emit body (lines ~2593-2697):
/// - pipeline re-resolved FRESH via `inner.logic_fill_caches[dst_format]
///   .get(logic_mode, opaque_alpha)` (cache is engine-owned + stable per N6).
/// - Single `cmd_set_viewport` OUTSIDE the per-rect loop (N6 invariant).
/// - Push constants match legacy shape: dst_origin, dst_size, viewport,
///   _pad, fg_color.
fn emit_recorded_logic_fill_into_cb(
    inner: &mut RenderEngineInner,
    store: &DrawableStore,
    cb: vk::CommandBuffer,
    lf: &super::frame_builder::RecordedLogicFill,
) -> Result<(), RenderError> {
    use crate::kms::vk::logic_fill_pipeline::LogicFillPushConsts;
    let device = &inner.vk.device;
    let dst_image = store
        .get(lf.dst_id)
        .ok_or(RenderError::UnknownDrawable(lf.dst_id))?
        .storage
        .image;
    let cache = inner
        .logic_fill_caches
        .get_mut(&lf.dst_format)
        .ok_or(RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED))?;
    let pipeline = cache
        .get(lf.logic_mode, lf.opaque_alpha)
        .map_err(|_| RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED))?;
    let pipeline_layout = cache.pipeline_layout();

    // N6 pre-barrier: drain prior compose / put_image / paint writes via
    // ALL_COMMANDS producer mask (exact shape from legacy at
    // engine.rs:2649-2663).
    barrier_to_layout(
        device,
        cb,
        dst_image,
        lf.dst_old_layout,
        vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        vk::PipelineStageFlags2::ALL_COMMANDS,
        vk::AccessFlags2::SHADER_SAMPLED_READ
            | vk::AccessFlags2::TRANSFER_WRITE
            | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
        vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
        // LOAD-bearing render passes need attachment read access on open,
        // same as the compositor path in vk::ops::render.
        vk::AccessFlags2::COLOR_ATTACHMENT_READ | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
    );

    let render_area = vk::Rect2D {
        offset: vk::Offset2D::default(),
        extent: lf.dst_extent,
    };
    let color_attachment = [vk::RenderingAttachmentInfo::default()
        .image_view(lf.dst_image_view)
        .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .load_op(vk::AttachmentLoadOp::LOAD)
        .store_op(vk::AttachmentStoreOp::STORE)];
    let rendering_info = vk::RenderingInfo::default()
        .render_area(render_area)
        .layer_count(1)
        .color_attachments(&color_attachment);
    #[allow(clippy::cast_precision_loss)]
    let viewport = [vk::Viewport {
        x: 0.0,
        y: 0.0,
        width: lf.dst_extent.width as f32,
        height: lf.dst_extent.height as f32,
        min_depth: 0.0,
        max_depth: 1.0,
    }];
    #[allow(clippy::cast_precision_loss)]
    let dst_vp = [lf.dst_extent.width as f32, lf.dst_extent.height as f32];
    unsafe {
        device.cmd_begin_rendering(cb, &rendering_info);
        // N6: single cmd_set_viewport OUTSIDE the per-rect loop
        // (legacy at engine.rs:2702 sets it once before the for loop).
        device.cmd_set_viewport(cb, 0, &viewport);
        device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, pipeline);
        for r in &lf.rects {
            let scissor = [*r];
            device.cmd_set_scissor(cb, 0, &scissor);
            #[allow(clippy::cast_precision_loss)]
            let pc = LogicFillPushConsts {
                dst_origin: [r.offset.x as f32, r.offset.y as f32],
                dst_size: [r.extent.width as f32, r.extent.height as f32],
                viewport: dst_vp,
                _pad: [0.0, 0.0],
                fg_color: lf.color,
            };
            device.cmd_push_constants(
                cb,
                pipeline_layout,
                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                0,
                pc.as_bytes(),
            );
            device.cmd_draw(cb, 4, 1, 0, 0);
        }
        device.cmd_end_rendering(cb);
    }
    // Post-barrier: COLOR_ATTACHMENT_OPTIMAL → SHADER_READ_ONLY_OPTIMAL.
    barrier_to_layout(
        device,
        cb,
        dst_image,
        vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
        vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
        vk::PipelineStageFlags2::FRAGMENT_SHADER,
        vk::AccessFlags2::SHADER_SAMPLED_READ,
    );
    Ok(())
}

/// Phase B.3 Task 14 (N7): replay a deferred `RecordedImageText`
/// into the frame's command buffer. Mirrors the legacy
/// `record_text_run` call shape from the pre-B.3 `image_text` body
/// (engine.rs:4049-4070). The key difference from B.1's
/// `emit_recorded_op_into_cb`'s `CompositeGlyphs` arm:
/// - Uses `record_text_run` (single-run, NO clip scissors), NOT
///   `record_text_run_scissored` (which carries an X RENDER picture clip).
/// - Carries `dst_old_layout` from the recorded payload instead of the
///   live storage layout (Pitfall 5 — the live layout is stale during
///   deferred emit).
fn emit_recorded_image_text_into_cb(
    inner: &mut RenderEngineInner,
    store: &mut DrawableStore,
    cb: vk::CommandBuffer,
    it: &super::frame_builder::RecordedImageText,
) -> Result<(), RenderError> {
    let atlas_extent = inner
        .glyph_atlas
        .as_ref()
        .ok_or(RenderError::NoVk)?
        .extent();
    // Clone the Vk handle so the recorder call doesn't alias
    // `inner.text_pipeline` against `&inner.vk`.
    let vk = inner.vk.clone();
    let drawable = store
        .get_mut(it.dst_id)
        .ok_or(RenderError::UnknownDrawable(it.dst_id))?;
    let glyphs_view: Vec<crate::kms::vk::ops::text::TextGlyph> = it
        .glyphs
        .iter()
        .map(|g| crate::kms::vk::ops::text::TextGlyph {
            entry: super::glyph_atlas::AtlasEntry {
                atlas_x: g.atlas_x,
                atlas_y: g.atlas_y,
                w: g.w,
                h: g.h,
                pen_left: 0,
                pen_top: 0,
            },
            dst_x: g.dst_x,
            dst_y: g.dst_y,
        })
        .collect();
    // Build the StorageTextTarget adapter using the recorded
    // `dst_old_layout` (Pitfall 5 — the drawable's live
    // `current_layout` is stale during deferred emit; the overlay
    // has already been committed by push_op_and_set_layouts).
    let mut adapter = StorageTextTarget {
        extent: drawable.storage.extent,
        image: drawable.storage.image,
        image_view: drawable.storage.image_view,
        current_layout: it.dst_old_layout,
    };
    let pipeline = inner.text_pipeline.as_ref().ok_or(RenderError::NoVk)?;
    // image_text uses single-run record_text_run (no clip scissors),
    // distinct from composite_glyphs's record_text_run_scissored.
    crate::kms::vk::ops::text::record_text_run(
        &vk,
        cb,
        &mut adapter,
        crate::kms::vk::ops::text::TextAtlas {
            extent: atlas_extent,
        },
        pipeline,
        &glyphs_view,
        it.foreground_rgba,
    )?;
    // Propagate the adapter's tracked layout back into the drawable's
    // storage — record_text_run transitions to SHADER_READ_ONLY_OPTIMAL.
    drawable.storage.current_layout = adapter.current_layout;
    Ok(())
}

/// Phase B.3 Task 12 (N5): replay a deferred `RecordedRenderTrapsOrTris`
/// into the frame's command buffer. Mirrors the legacy
/// `render_traps_or_tris` two-stage CB (raster phase + composite phase).
///
/// All four resources NOT recorded per N5 are re-resolved FRESH:
/// - `engine.mask_scratch` (image, attachment_view, image_view, extent, current_layout).
/// - `engine.dst_readback` view when `std_op.needs_dst_readback()`.
/// - composite pipeline via `render_pipelines.get(std_op, …)`.
/// - descriptor set via `allocate_descriptor_for_views_into_ring` using
///   `open_frame.frame_generation` as the watermark.
///
/// Post-emit CPU writeback (N5 LOAD-BEARING, codex round-10):
/// `inner.mask_scratch.set_current_layout(SHADER_READ_ONLY_OPTIMAL)` after
/// the composite-close barrier — without this the NEXT trap op's pre-barrier
/// reads a stale old_layout (VUID-class bug).
/// Per-axis source sampling origin for the RENDER `Trapezoids`/
/// `Triangles` composite stage. `base` is the client `xSrc`/`ySrc`
/// already shifted by the caller's dst redirect / `x_off` delta. When
/// the op renders over the full dst (`needs_full_dst`) the coverage
/// mask carries the bbox offset, so the source aligns directly at
/// `base`; otherwise the composite renders at the bbox origin and the
/// source must add it back. Mirrors Xorg `miTrapezoids`, where the
/// source is sampled at `xSrc + dst_px`.
#[inline]
fn trap_composite_src_origin_axis(base: i32, bbox: i32, needs_full_dst: bool) -> i32 {
    if needs_full_dst { base } else { base + bbox }
}

#[allow(clippy::too_many_arguments)]
fn emit_recorded_render_traps_or_tris_into_cb(
    inner: &mut RenderEngineInner,
    store: &mut DrawableStore,
    cb: vk::CommandBuffer,
    pins: &super::frame_builder::FramePinSet,
    frame_generation: u64,
    rt: &super::frame_builder::RecordedRenderTrapsOrTris,
) -> Result<(), RenderError> {
    use crate::kms::vk::{
        ops::render as vk_render, render_pipeline::record_solid_color_clear,
        trap_pipeline::TrapDrawPushConsts,
    };

    // ── (a) Resolve src view FRESH from engine caches at emit time ──
    let solid_src_view = inner
        .solid_src_image
        .as_ref()
        .expect("solid_src_image: ensure_render_assets ran at append")
        .image_view();

    let src_view = match &rt.src_kind {
        super::frame_builder::RecordedTrapSrcKind::Drawable { id, swizzle_class } => {
            let info =
                drawable_for_render_view(store, *id).ok_or(RenderError::UnknownDrawable(*id))?;
            // Use the snapshot swizzle_class (append-time stable, per N5).
            // Mirror the non-deferred composite paths: derive the src
            // view's sampler from the picture's repeat mode (REPEAT_NONE
            // → clamp-to-border, not the previously hardcoded
            // clamp-to-edge). The in-shader `apply_repeat` already zeroes
            // out-of-bounds samples for REPEAT_NONE, so this is mostly
            // hygiene/consistency — but it removes a latent edge-texel
            // leak at the exact-`uv==1.0` boundary.
            let sampler = sampler_config_for_shader_repeat(rt.src_repeat);
            ensure_drawable_view(
                &inner.vk,
                &mut inner.drawable_view_cache,
                *id,
                info.image,
                info.format,
                sampler,
                *swizzle_class,
            )?
        }
        super::frame_builder::RecordedTrapSrcKind::Solid(color) => {
            // record_solid_color_clear writes the colour into the 1×1 scratch
            // BEFORE the trap raster phase; the view is the solid_src_view.
            let solid = inner
                .solid_src_image
                .as_mut()
                .expect("solid_src_image: ensure_render_assets ran at append");
            record_solid_color_clear(&inner.vk, cb, solid, *color);
            solid_src_view
        }
        super::frame_builder::RecordedTrapSrcKind::Gradient {
            picture,
            intrinsic_axis_projection: _,
        } => {
            // B.3 hotfix 2: the Arc clone guarantees liveness — no
            // picture_paint lookup, no None branch. The "missing at
            // emit" warn path is gone.
            picture.image_view()
        }
    };

    // ── (b) Resolve mask_scratch FRESH ──
    let mask_scratch = inner
        .mask_scratch
        .as_ref()
        .expect("mask_scratch: ensure_trap_assets ran at append");
    let mask_image = mask_scratch.image();
    let mask_attachment_view = mask_scratch.attachment_view();
    let mask_view = mask_scratch.image_view();
    let mask_extent = mask_scratch.extent();
    let mask_src_layout = mask_scratch.current_layout();

    // ── (c) dst_readback view FRESH when std_op.needs_dst_readback() ──
    let white_mask_view = inner
        .white_mask_image
        .as_ref()
        .expect("white_mask_image: ensure_render_assets ran at append")
        .image_view();
    let dst_readback_view = if rt.std_op.needs_dst_readback() {
        let rb = inner
            .dst_readback
            .as_mut()
            .expect("dst_readback: ensured at append when needs_dst_readback");
        match rb.view(rt.dst_format, rt.dst_has_alpha) {
            Ok(Some(v)) => v,
            Ok(None) | Err(_) => {
                log::warn!(
                    "emit_recorded_render_traps_or_tris: dst_readback view unavailable — skipping"
                );
                return Ok(());
            }
        }
    } else {
        white_mask_view
    };

    // ── Resolve composite pipeline FRESH ──
    let pipeline = inner
        .render_pipelines
        .as_mut()
        .expect("render_pipelines: ensured at append")
        .get(rt.std_op, rt.dst_format, rt.dst_has_alpha, false)
        .map_err(|e| {
            log::warn!("emit_recorded_render_traps_or_tris: pipeline build {e:?}");
            RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
        })?;
    let pipeline_layout = inner
        .render_pipelines
        .as_ref()
        .expect("render_pipelines: ensured at append")
        .pipeline_layout();

    // Allocate descriptor set FRESH via B.2 Mechanism 2 watermark.
    // `frame_generation` is threaded from the close path's local copy of
    // `open_frame.frame_generation` — inner.frame_builder.open is None by
    // the time the emit dispatch loop runs (take_open_for_close clears it).
    let descriptor_set = inner
        .render_pipelines
        .as_ref()
        .expect("render_pipelines: ensured at append")
        .allocate_descriptor_for_views_into_ring(
            &mut inner.descriptor_pool_ring,
            frame_generation,
            src_view,
            mask_view,
            dst_readback_view,
        )?;

    let device = &inner.vk.device;

    // ── (d) Trap raster phase — mirror engine.rs:7531-7647 ──
    let (prim_pipeline, prim_layout) = {
        let tp = inner
            .trap_pipeline
            .as_ref()
            .expect("trap_pipeline: ensured at append");
        let pipe = match rt.prim_kind {
            TrapPrimKind::Trapezoid => tp.trapezoid_pipeline(),
            TrapPrimKind::Triangle => tp.triangle_pipeline(),
        };
        (pipe, tp.pipeline_layout())
    };

    // Barrier: mask_scratch → COLOR_ATTACHMENT_OPTIMAL.
    let (mask_src_stage, mask_src_access) = match mask_src_layout {
        vk::ImageLayout::UNDEFINED => {
            (vk::PipelineStageFlags2::TOP_OF_PIPE, vk::AccessFlags2::NONE)
        }
        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL => (
            vk::PipelineStageFlags2::FRAGMENT_SHADER,
            vk::AccessFlags2::SHADER_SAMPLED_READ,
        ),
        _ => (
            vk::PipelineStageFlags2::ALL_COMMANDS,
            vk::AccessFlags2::SHADER_SAMPLED_READ,
        ),
    };
    let color_range = vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1);
    let to_attach = [vk::ImageMemoryBarrier2::default()
        .src_stage_mask(mask_src_stage)
        .src_access_mask(mask_src_access)
        .dst_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
        .dst_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
        .old_layout(mask_src_layout)
        .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .image(mask_image)
        .subresource_range(color_range)];
    let dep = vk::DependencyInfo::default().image_memory_barriers(&to_attach);
    unsafe { device.cmd_pipeline_barrier2(cb, &dep) };

    let bbox_render_area = vk::Rect2D {
        offset: vk::Offset2D::default(),
        extent: vk::Extent2D {
            width: rt.bbox_w,
            height: rt.bbox_h,
        },
    };
    // For ops that composite the coverage mask over the FULL dst
    // (`needs_full_dst`: op=Src and friends), the composite below samples
    // `mask_scratch` at texels OUTSIDE the trap bbox (the mask is offset
    // by `-bbox` and read across the whole destination). `mask_scratch`
    // is a persistent, power-of-two-grown, reused image (256² minimum),
    // so those out-of-bbox texels hold STALE coverage from a prior
    // trap-op. Clearing only the bbox region leaves that stale data,
    // which a full-dst composite reads as nonzero coverage → a spurious
    // alpha ridge (observed as a 2px alpha~=23 line at GTK CSD tooltip
    // box edges, where `solid_alpha(46) * stale_coverage(~50%) = 23`).
    // Clear the WHOLE scratch for these ops so out-of-bbox samples read
    // 0; the draw stays scissored to the bbox (`cmd_set_scissor` below),
    // so coverage is unchanged — only the previously-stale margin is
    // now zeroed. Non-full-dst ops render only within the bbox, never
    // sample the margin, and keep the cheaper bbox-only clear.
    let needs_full_dst_clear = matches!(
        rt.op_byte,
        0 | 1 | 5 | 6 | 7 | 10 | 13 | 16..=27 | 32..=43
    );
    let clear_render_area = if needs_full_dst_clear {
        vk::Rect2D {
            offset: vk::Offset2D::default(),
            extent: mask_extent,
        }
    } else {
        bbox_render_area
    };
    let clear = vk::ClearValue {
        color: vk::ClearColorValue {
            float32: [0.0, 0.0, 0.0, 0.0],
        },
    };
    let color_attachment = [vk::RenderingAttachmentInfo::default()
        .image_view(mask_attachment_view)
        .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .load_op(vk::AttachmentLoadOp::CLEAR)
        .store_op(vk::AttachmentStoreOp::STORE)
        .clear_value(clear)];
    let rendering_info = vk::RenderingInfo::default()
        .render_area(clear_render_area)
        .layer_count(1)
        .color_attachments(&color_attachment);

    // Bind vertex buffer from the pinned StagingBuffer (N5 / B.1 N2 pattern).
    let vertex_buf = pins.staging_buffers[rt.vertex_pool_pin.0 as usize].buffer;
    #[allow(clippy::cast_precision_loss)]
    let trap_pc = TrapDrawPushConsts {
        mask_extent: [mask_extent.width as f32, mask_extent.height as f32],
        bbox_origin_pixel: [rt.bbox_x as f32, rt.bbox_y as f32],
        bbox_size_pixel: [rt.bbox_w as f32, rt.bbox_h as f32],
        _pad: [0.0; 2],
    };
    #[allow(clippy::cast_precision_loss)]
    let trap_viewport = [vk::Viewport {
        x: 0.0,
        y: 0.0,
        width: mask_extent.width as f32,
        height: mask_extent.height as f32,
        min_depth: 0.0,
        max_depth: 1.0,
    }];
    unsafe {
        device.cmd_begin_rendering(cb, &rendering_info);
        device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, prim_pipeline);
        device.cmd_bind_vertex_buffers(cb, 0, &[vertex_buf], &[0]);
        device.cmd_push_constants(
            cb,
            prim_layout,
            vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
            0,
            trap_pc.as_bytes(),
        );
        device.cmd_set_viewport(cb, 0, &trap_viewport);
        device.cmd_set_scissor(cb, 0, &[bbox_render_area]);
        device.cmd_draw(cb, 4, rt.instance_count, 0, 0);
        device.cmd_end_rendering(cb);
    }

    // Barrier mask: COLOR_ATTACHMENT → SHADER_READ_ONLY for the composite read.
    let to_read = [vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
        .src_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
        .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
        .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
        .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
        .image(mask_image)
        .subresource_range(color_range)];
    let dep = vk::DependencyInfo::default().image_memory_barriers(&to_read);
    unsafe { device.cmd_pipeline_barrier2(cb, &dep) };

    // ── (e) Composite phase — mirror engine.rs:7665-7735 ──

    // dst_readback snapshot for Disjoint/Conjoint.
    if rt.std_op.needs_dst_readback() {
        let dst_current = store
            .get(rt.dst_id)
            .expect("dst_id checked at append")
            .storage
            .current_layout;
        let rb = inner
            .dst_readback
            .as_mut()
            .expect("dst_readback: ensured at append");
        rb.record_copy_from(cb, rt.dst_image, dst_current, rt.dst_format, rt.dst_extent);
    }

    // needs_full_dst byte-pattern test from rt.op_byte (N5).
    let needs_full_dst = matches!(
        rt.op_byte,
        0 | 1 | 5 | 6 | 7 | 10 | 13 | 16..=27 | 32..=43
    );
    let (render_dst_x, render_dst_y, render_w, render_h, mask_off_x, mask_off_y) = if needs_full_dst
    {
        (
            0,
            0,
            rt.dst_extent.width,
            rt.dst_extent.height,
            -rt.bbox_x,
            -rt.bbox_y,
        )
    } else {
        (rt.bbox_x, rt.bbox_y, rt.bbox_w, rt.bbox_h, 0, 0)
    };

    // Compose src_xform: Gradient composes intrinsic; others pass user_src_xform.
    let combined_src_xform = match &rt.src_kind {
        super::frame_builder::RecordedTrapSrcKind::Gradient {
            intrinsic_axis_projection,
            ..
        } => crate::kms::backend::compose_affines(*intrinsic_axis_projection, rt.user_src_xform),
        _ => rt.user_src_xform,
    };
    // Effective src_repeat: PAD for synthetic 1×1, else recorded constant.
    // Cast back to i32 for CompositeAttrs (the shader constants are 0..=3).
    #[allow(clippy::cast_possible_wrap)]
    let effective_src_repeat: i32 = if rt.src_is_synthetic_1x1 {
        crate::kms::vk::render_pipeline::REPEAT_PAD
    } else {
        rt.src_repeat as i32
    };

    let attrs = vk_render::CompositeAttrs {
        src_extent: rt.src_extent,
        mask_extent,
        src_repeat: effective_src_repeat,
        mask_repeat: crate::kms::vk::render_pipeline::REPEAT_NONE,
        src_force_opaque: rt.src_force_opaque,
        mask_force_opaque: false,
        src_xform: combined_src_xform,
        mask_xform: vk_render::AffineXform::IDENTITY,
    };

    // Source sampling origin. Mirrors Xorg `miTrapezoids`: the src is
    // sampled at `xSrc + dst_px`. `rt.src_origin_{x,y}` already carries
    // the redirect/x_off-shifted `xSrc`/`ySrc`. For the `needs_full_dst`
    // branch the composite renders over the whole dst (the mask carries
    // the bbox offset via `mask_off`), so the src aligns directly at the
    // recorded origin; otherwise the composite renders at the bbox
    // origin and the src must add it back. Confined to `Drawable`
    // sources — `Solid` is a constant colour (origin irrelevant) and
    // `Gradient` is positioned by its intrinsic transform — so the only
    // behaviour change vs the prior hardcoded `0` is the picture-source
    // case (e.g. GTK CSD shadow blur-mask ramps sampled at `ySrc != 0`).
    let (src_org_x, src_org_y) = match &rt.src_kind {
        super::frame_builder::RecordedTrapSrcKind::Drawable { .. } => (
            trap_composite_src_origin_axis(rt.src_origin_x, rt.bbox_x, needs_full_dst),
            trap_composite_src_origin_axis(rt.src_origin_y, rt.bbox_y, needs_full_dst),
        ),
        _ => (0, 0),
    };

    let rects = [vk_render::CompositeRect {
        src_x: src_org_x,
        src_y: src_org_y,
        mask_x: mask_off_x,
        mask_y: mask_off_y,
        dst_x: render_dst_x,
        dst_y: render_dst_y,
        width: render_w,
        height: render_h,
    }];

    // Phase B.3 fix: under deferred recording, the GPU dst layout may
    // diverge from `storage.current_layout` — prior ops in the SAME
    // frame transitioned the dst on the GPU but storage isn't committed
    // until `commit_close_success` reads back from the frame overlay on
    // submit success. Driving the `to_color` barrier from
    // `storage.current_layout` here mis-declares old_layout to the
    // implementation, producing driver-undefined dst contents — the
    // observed symptom was partial α loss on depth-32 backings when
    // marco's frame trapezoids followed an inner-window `render_composite`
    // in the same frame.
    //
    // Match the B.2 render_composite emit path: use `RecordedCompositeTarget`
    // (constant `COLOR_ATTACHMENT_OPTIMAL`, non-mutating storage) and
    // `record_render_composite_open_with_old_layout` with the recorded
    // `dst_old_layout` from the overlay-resolved append snapshot. Storage
    // is NOT mutated here; `commit_close_success` writes the frame
    // overlay's post-op layout back to `storage.current_layout` on
    // successful submit. The append-time
    // `push_op_and_set_layouts([(dst_id, SHADER_READ_ONLY_OPTIMAL)])`
    // call records that post-op layout in the overlay.
    let mut adapter = RecordedCompositeTarget {
        image: rt.dst_image,
        view: rt.dst_view,
        extent: rt.dst_extent,
    };

    vk_render::record_render_composite_open_with_old_layout(
        &inner.vk,
        cb,
        &adapter,
        pipeline,
        rt.dst_old_layout,
    )?;
    vk_render::record_render_composite_draws(
        &inner.vk,
        cb,
        pipeline_layout,
        descriptor_set,
        rt.dst_extent,
        &attrs,
        &rects,
        &rt.clip_scissors,
    );
    vk_render::record_render_composite_close(&inner.vk, cb, &mut adapter);

    // ── (f) Post-emit CPU writeback (N5 LOAD-BEARING, codex round-10) ──
    // The composite-close barrier (inside record_render_composite) left the
    // mask_scratch image in SHADER_READ_ONLY_OPTIMAL on the GPU.
    // Advance the CPU-tracked layout NOW so the NEXT trap op's pre-barrier
    // emits from the correct old_layout instead of the stale pre-raster value.
    inner
        .mask_scratch
        .as_mut()
        .expect("mask_scratch: ensured at append")
        .set_current_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);

    Ok(())
}

/// Phase B.2 Task 12: no-storage [`CompositeTarget`] adapter used at
/// emit-time to replay a `RecordedRenderComposite`. Carries the
/// pre-resolved image / view / extent the op was recorded against; the
/// payload's `dst_old_layout` is supplied separately via
/// [`vk_render::record_render_composite_open_with_old_layout`].
///
/// Two semantic properties of this adapter:
///
/// - `current_layout()` returns `COLOR_ATTACHMENT_OPTIMAL` as a
///   constant. The open overload uses the explicit `old_layout`
///   parameter, so the trait read is structurally unused by the open
///   path. `record_render_composite_close` does NOT read
///   `current_layout()` either — it hard-codes
///   `COLOR_ATTACHMENT_OPTIMAL` as the to_read barrier's old layout
///   (render.rs:~377). Returning the same constant keeps the adapter
///   honest about the layout the image IS in between open and close.
/// - `set_current_layout` is a NO-OP. The recorder's close transition
///   calls `dst.set_current_layout(SHADER_READ_ONLY_OPTIMAL)` (codex R5
///   audit catch); under B.2's deferred-recording rule storage layout
///   is NEVER mutated during recording — `commit_close_success` walks
///   the frame's `FrameLayoutTable` overlay and writes the post-op
///   layout back to `Drawable::storage.current_layout` only on submit
///   success. Mutating the adapter would be a write-to-the-void.
struct RecordedCompositeTarget {
    image: vk::Image,
    view: vk::ImageView,
    extent: vk::Extent2D,
}

impl CompositeTarget for RecordedCompositeTarget {
    fn vk_image(&self) -> vk::Image {
        self.image
    }
    fn vk_image_view(&self) -> vk::ImageView {
        self.view
    }
    fn extent(&self) -> vk::Extent2D {
        self.extent
    }
    fn current_layout(&self) -> vk::ImageLayout {
        // See struct doc — `_with_old_layout` doesn't read this; the
        // close path doesn't read it either. Return the layout the
        // image IS in between open and close (a constant) as
        // defence-in-depth against a future refactor that adds a read.
        vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL
    }
    fn set_current_layout(&mut self, _layout: vk::ImageLayout) {
        // Intentional no-op — see struct doc. Codex R5 audit point.
    }
}

/// Phase B.1 Task 12 / Phase B.2 Task 4: commit recorder-side state
/// to engine + atlas + drawable store after `flush_submit_group`
/// returned Ok.
///
/// - **Drawable layout commit** (B.2 LOAD-BEARING, USER-codex U-R6.F1):
///   walk the `FrameLayoutTable::drawables` overlay and write each
///   entry's `current_in_frame_layout` back to
///   `Drawable::storage.current_layout`. The recorded ops' barriers
///   transitioned the GPU image to this layout; storage was
///   deliberately NOT mutated during recording so failed frames can
///   drop the overlay without rolling back. On success, storage MUST
///   catch up — otherwise subsequent ops (legacy or post-B.4 ported)
///   emit barriers from stale `old_layout` values, producing a
///   Vulkan validation hazard / corrupt sampled image / device loss.
///   Under B.1's recorder (which mutates storage in-place during
///   emit) the overlay is structurally empty for the porting paths,
///   so this loop is a no-op on B.1 frames — the harm shape only
///   shows up once a B.2 port (Task 11+) routes its layout updates
///   exclusively through the overlay.
/// - **Touched-drawable `last_render_ticket` commit:** no-op — the
///   recorder already called `store.touch_render_fence` at append.
/// - **Atlas layout commit:** the B.1 recorder mutates
///   `V2GlyphAtlas::current_layout` in place during composite_glyphs,
///   so the atlas overlay is structurally empty on B.1 frames.
///   Reserved as a no-op-friendly write for Task 11 (when ported ops
///   read the atlas layout via overlay too). Idempotent against the
///   B.1 path because B.1's recorder leaves the overlay entry's
///   `current_in_frame_layout` equal to the atlas's actual
///   post-frame layout in the rare case it touches both.
/// - **Glyph cache inserts:** drained here onto the atlas.
/// - **Atlas last_render_ticket:** stamped with the closed frame's
///   ticket.
fn commit_close_success(
    inner: &mut RenderEngineInner,
    store: &mut DrawableStore,
    layouts: super::frame_builder::FrameLayoutTable,
    touched: super::frame_builder::TouchedDrawables,
    pending: super::frame_builder::PendingGlyphInserts,
    frame_ticket: &FenceTicket,
) {
    let _ = touched;
    // Drawables: commit overlay → storage. Empty on B.1 frames; the
    // load-bearing write is reserved for B.2 Task 11+ ports that
    // route their layout updates exclusively through the overlay.
    for (id, entry) in layouts.drawables {
        if let Some(d) = store.get_mut(id) {
            d.storage.current_layout = entry.current_in_frame_layout;
        }
    }
    if let Some(atlas) = inner.glyph_atlas.as_mut() {
        // Atlas: commit overlay → atlas.current_layout. Structurally
        // empty under B.1's recorder (which mutates the atlas
        // layout in place during emit). Reserved for Task 11+ when
        // the ported path consults the overlay-resolved layout.
        if let Some(entry) = layouts.atlas {
            atlas.set_current_layout(entry.current_in_frame_layout);
        }
        for (key, entry) in pending.entries {
            atlas.insert_entry(key, entry);
        }
        atlas.set_last_render_ticket(frame_ticket.clone());
    }
}

/// Phase B.1 Task 12: rollback drawable-side state to pre-frame on
/// any close-time failure. Walks the layout overlay + touched-set
/// to undo any in-frame mutations the recorder already wrote into
/// the store. Atlas-side rollback is handled by `rollback_atlas`.
fn rollback_pre_submit(
    store: &mut DrawableStore,
    open_frame: &mut super::frame_builder::OpenFrame,
) {
    for (id, entry) in open_frame.layouts.drawables.drain() {
        if let Some(d) = store.get_mut(id) {
            d.storage.current_layout = entry.pre_frame_layout;
        }
    }
    for (id, prior) in open_frame.touched.snapshots.drain() {
        if let Some(d) = store.get_mut(id) {
            d.last_render_ticket = prior;
        }
    }
}

/// Phase B.1 Task 12: rollback atlas-side state to pre-frame on any
/// close-time failure. Restores the pre-frame layout (if the frame
/// touched the atlas) and the pre-frame `last_render_ticket`
/// snapshot (if the frame snapshotted it).
fn rollback_atlas(
    inner: &mut RenderEngineInner,
    layouts_atlas: Option<super::frame_builder::LayoutOverlayEntry>,
    atlas_prev_ticket_snapshot: Option<Option<FenceTicket>>,
) {
    if let Some(atlas) = inner.glyph_atlas.as_mut() {
        if let Some(entry) = layouts_atlas {
            atlas.set_current_layout(entry.pre_frame_layout);
        }
        if let Some(prior) = atlas_prev_ticket_snapshot {
            match prior {
                Some(t) => atlas.set_last_render_ticket(t),
                None => atlas.clear_last_render_ticket(),
            }
        }
    }
}

fn color_layers() -> vk::ImageSubresourceLayers {
    vk::ImageSubresourceLayers::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .layer_count(1)
}

/// Single-image-layout barrier helper for scratch images that
/// `Drawable::record_layout_transition` can't touch (the scratch
/// isn't a tracked drawable).
#[allow(clippy::too_many_arguments)]
fn barrier_to_layout(
    device: &ash::Device,
    cb: vk::CommandBuffer,
    image: vk::Image,
    old_layout: vk::ImageLayout,
    new_layout: vk::ImageLayout,
    src_stage: vk::PipelineStageFlags2,
    src_access: vk::AccessFlags2,
    dst_stage: vk::PipelineStageFlags2,
    dst_access: vk::AccessFlags2,
) {
    let b = [vk::ImageMemoryBarrier2::default()
        .src_stage_mask(src_stage)
        .src_access_mask(src_access)
        .dst_stage_mask(dst_stage)
        .dst_access_mask(dst_access)
        .old_layout(old_layout)
        .new_layout(new_layout)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image)
        .subresource_range(
            vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .level_count(1)
                .layer_count(1),
        )];
    let dep = vk::DependencyInfo::default().image_memory_barriers(&b);
    unsafe { device.cmd_pipeline_barrier2(cb, &dep) };
}

/// Allocate a scratch image for `copy_area`'s overlap path.
/// Device-local, OPTIMAL tiling, TRANSFER_SRC|TRANSFER_DST usage.
/// Caller is responsible for adopting it into the op's
/// `SubmittedOp.scratch` so it retires on the fence.
fn allocate_scratch_image(
    vk: &Arc<VkContext>,
    _platform: &PlatformBackend,
    width: u32,
    height: u32,
    format: vk::Format,
) -> Result<ScratchImage, RenderError> {
    let info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(format)
        .extent(vk::Extent3D {
            width,
            height,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::TRANSFER_DST)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED);
    let image = unsafe { vk.device.create_image(&info, None)? };
    let mem_reqs = unsafe { vk.device.get_image_memory_requirements(image) };
    let mem_props = unsafe {
        vk.instance
            .get_physical_device_memory_properties(vk.physical_device)
    };
    let Some(mt) = (0..mem_props.memory_type_count).find(|&i| {
        mem_reqs.memory_type_bits & (1 << i) != 0
            && mem_props.memory_types[i as usize]
                .property_flags
                .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
    }) else {
        unsafe { vk.device.destroy_image(image, None) };
        return Err(RenderError::Vk(vk::Result::ERROR_FEATURE_NOT_PRESENT));
    };
    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(mem_reqs.size)
        .memory_type_index(mt);
    let memory = match unsafe { vk.device.allocate_memory(&alloc_info, None) } {
        Ok(m) => m,
        Err(e) => {
            unsafe { vk.device.destroy_image(image, None) };
            return Err(RenderError::Vk(e));
        }
    };
    if let Err(e) = unsafe { vk.device.bind_image_memory(image, memory, 0) } {
        unsafe {
            vk.device.free_memory(memory, None);
            vk.device.destroy_image(image, None);
        }
        return Err(RenderError::Vk(e));
    }
    Ok(ScratchImage {
        vk: Arc::clone(vk),
        image,
        memory,
        size_bytes: mem_reqs.size,
    })
}

/// Stage 5 Task 3: build clip-scissor list for render_composite
/// (mirrors the inline arithmetic in `render_composite`). `None`
/// → single full-extent scissor; `Some(cr)` → clamped per-rect
/// list (empty rects skipped). Returns empty Vec if no rect is
/// visible.
fn build_render_clip_scissors(
    clip_rects: Option<&[Rectangle16]>,
    dst_extent: vk::Extent2D,
) -> Vec<vk::Rect2D> {
    match clip_rects {
        None => vec![vk::Rect2D {
            offset: vk::Offset2D::default(),
            extent: dst_extent,
        }],
        Some(cr) => {
            let mut out = Vec::with_capacity(cr.len());
            for r in cr {
                if r.width == 0 || r.height == 0 {
                    continue;
                }
                let x0 = i32::from(r.x).max(0);
                let y0 = i32::from(r.y).max(0);
                let x1 = (i32::from(r.x) + i32::from(r.width))
                    .min(i32::try_from(dst_extent.width).unwrap_or(i32::MAX));
                let y1 = (i32::from(r.y) + i32::from(r.height))
                    .min(i32::try_from(dst_extent.height).unwrap_or(i32::MAX));
                if x1 <= x0 || y1 <= y0 {
                    continue;
                }
                out.push(vk::Rect2D {
                    offset: vk::Offset2D { x: x0, y: y0 },
                    extent: vk::Extent2D {
                        #[allow(clippy::cast_sign_loss)]
                        width: (x1 - x0) as u32,
                        #[allow(clippy::cast_sign_loss)]
                        height: (y1 - y0) as u32,
                    },
                });
            }
            out
        }
    }
}

pub(crate) fn clamp_rect(rect: vk::Rect2D, extent: vk::Extent2D) -> vk::Rect2D {
    let max_x = i32::try_from(extent.width).unwrap_or(i32::MAX);
    let max_y = i32::try_from(extent.height).unwrap_or(i32::MAX);
    let x0 = rect.offset.x.max(0).min(max_x);
    let y0 = rect.offset.y.max(0).min(max_y);
    let x1 = rect
        .offset
        .x
        .saturating_add_unsigned(rect.extent.width)
        .clamp(0, max_x);
    let y1 = rect
        .offset
        .y
        .saturating_add_unsigned(rect.extent.height)
        .clamp(0, max_y);
    vk::Rect2D {
        offset: vk::Offset2D { x: x0, y: y0 },
        extent: vk::Extent2D {
            width: u32::try_from((x1 - x0).max(0)).unwrap_or(0),
            height: u32::try_from((y1 - y0).max(0)).unwrap_or(0),
        },
    }
}

/// Compute the destination rect (in storage coords) and the
/// (sx, sy) origin in the input image where copying should start.
/// Returns `None` if no pixels are visible.
fn clamp_put_rect(
    dst_pos: vk::Offset2D,
    src_extent: vk::Extent2D,
    dst_extent: vk::Extent2D,
) -> Option<(vk::Rect2D, (u32, u32))> {
    let max_x = i32::try_from(dst_extent.width).unwrap_or(i32::MAX);
    let max_y = i32::try_from(dst_extent.height).unwrap_or(i32::MAX);
    let x0 = dst_pos.x.max(0);
    let y0 = dst_pos.y.max(0);
    let sx = (x0 - dst_pos.x).max(0);
    let sy = (y0 - dst_pos.y).max(0);
    let x1 = dst_pos
        .x
        .saturating_add_unsigned(src_extent.width)
        .min(max_x);
    let y1 = dst_pos
        .y
        .saturating_add_unsigned(src_extent.height)
        .min(max_y);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some((
        vk::Rect2D {
            offset: vk::Offset2D { x: x0, y: y0 },
            extent: vk::Extent2D {
                width: u32::try_from((x1 - x0).max(0)).unwrap_or(0),
                height: u32::try_from((y1 - y0).max(0)).unwrap_or(0),
            },
        },
        (
            u32::try_from(sx).unwrap_or(0),
            u32::try_from(sy).unwrap_or(0),
        ),
    ))
}

/// X11 ZPixmap source row stride for a given depth + width. Per
/// the wire format: scanline padded to 32 bits.
fn x11_src_row_stride(depth: u8, width: u32) -> usize {
    let bits_per_row = match depth {
        1 => width,
        4 => u32::from(4u8) * width,
        8 => u32::from(8u8) * width,
        24 | 32 => 32 * width,
        _ => 32 * width,
    };
    // Pad up to 32 bits (4 bytes).
    let bits_padded = bits_per_row.div_ceil(32) * 32;
    (bits_padded / 8) as usize
}

/// Copy a sub-rect of `src` (ZPixmap wire, padded rows) into
/// `dst_ptr` (tightly packed bytes matching the storage format).
///
/// # Safety
///
/// `dst_ptr` must be valid for `dst_w * dst_h * dst_bpp` bytes.
///
/// # Errors
///
/// `TruncatedSource` if `src` is shorter than the row stride ×
/// (sy + dst_h) the depth implies.
fn unpack_to_staging(
    src: &[u8],
    src_extent: vk::Extent2D,
    sx: u32,
    sy: u32,
    dst_w: u32,
    dst_h: u32,
    src_depth: u8,
    dst_ptr: *mut u8,
) -> Result<(), RenderError> {
    let src_row_bytes = x11_src_row_stride(src_depth, src_extent.width);
    let expected_len =
        src_row_bytes
            .checked_mul((sy + dst_h) as usize)
            .ok_or(RenderError::TruncatedSource {
                expected: usize::MAX,
            })?;
    if src.len() < expected_len {
        return Err(RenderError::TruncatedSource {
            expected: expected_len,
        });
    }
    match src_depth {
        32 | 24 => {
            // BGRA8 wire → BGRA8 staging. For depth-24, force
            // alpha to 0xFF so subsequent sample-as-source has a
            // defined alpha channel.
            let row_dst_bytes = (dst_w * 4) as usize;
            for row in 0..dst_h {
                let src_row_off = (sy + row) as usize * src_row_bytes;
                let src_col_off = sx as usize * 4;
                let src_slice =
                    &src[src_row_off + src_col_off..src_row_off + src_col_off + row_dst_bytes];
                // SAFETY: caller guarantees dst_ptr is valid for
                // dst_w*dst_h*4 bytes; row * row_dst_bytes within.
                unsafe {
                    let dst = dst_ptr.add(row as usize * row_dst_bytes);
                    std::ptr::copy_nonoverlapping(src_slice.as_ptr(), dst, row_dst_bytes);
                    if src_depth == 24 {
                        // Stomp alpha to 0xFF every 4th byte.
                        for col in 0..dst_w as usize {
                            *dst.add(col * 4 + 3) = 0xFF;
                        }
                    }
                }
            }
        }
        8 => {
            let row_dst_bytes = dst_w as usize;
            for row in 0..dst_h {
                let src_row_off = (sy + row) as usize * src_row_bytes;
                let src_col_off = sx as usize;
                let src_slice =
                    &src[src_row_off + src_col_off..src_row_off + src_col_off + row_dst_bytes];
                unsafe {
                    let dst = dst_ptr.add(row as usize * row_dst_bytes);
                    std::ptr::copy_nonoverlapping(src_slice.as_ptr(), dst, row_dst_bytes);
                }
            }
        }
        4 => {
            let row_dst_bytes = dst_w as usize;
            for row in 0..dst_h {
                let src_row_off = (sy + row) as usize * src_row_bytes;
                let row_src = &src[src_row_off..src_row_off + src_row_bytes];
                unsafe {
                    let dst = dst_ptr.add(row as usize * row_dst_bytes);
                    for col in 0..dst_w as usize {
                        let byte = row_src[col / 2];
                        let nibble = if col % 2 == 0 {
                            byte & 0x0f
                        } else {
                            (byte >> 4) & 0x0f
                        };
                        *dst.add(col) = nibble;
                    }
                }
            }
        }
        1 => {
            // 1 bit per pixel → 1 byte per pixel (0xFF if set,
            // 0x00 if clear). Unpack each requested column from
            // the source bit position. Bit order matches the
            // server's advertised `bitmap-bit-order` — we forward
            // the client's `byte_order` from setup (typically
            // `LSBFirst` on x86), so bit 0 of a byte is pixel 0
            // in that 8-pixel group. Mirrors v1's depth-1 PutImage
            // unpacker at `kms::backend.rs:3995`.
            let row_dst_bytes = dst_w as usize;
            for row in 0..dst_h {
                let src_row_off = (sy + row) as usize * src_row_bytes;
                let row_src = &src[src_row_off..src_row_off + src_row_bytes];
                unsafe {
                    let dst = dst_ptr.add(row as usize * row_dst_bytes);
                    for col in 0..dst_w as usize {
                        let bit_index = sx as usize + col;
                        let byte = row_src[bit_index / 8];
                        let bit = (byte >> (bit_index % 8)) & 0x1;
                        *dst.add(col) = if bit != 0 { 0xFF } else { 0x00 };
                    }
                }
            }
        }
        _ => return Err(RenderError::UnsupportedDepth(src_depth)),
    }
    Ok(())
}

/// Convert tightly-packed storage bytes (from a GetImage
/// readback) into the wire format clients expect. Inverse of
/// [`unpack_to_staging`].
///
/// # Errors
///
/// `UnsupportedDepth` for depths other than 1/8/24/32.
fn pack_from_storage(raw: &[u8], w: u32, h: u32, depth: u8) -> Result<Vec<u8>, RenderError> {
    match depth {
        32 | 24 => {
            // Storage is BGRA8 tightly packed; wire ZPixmap is
            // also BGRA8 tightly packed for our advertised
            // visual (no scanline pad at depth-32 because
            // 32 bits already aligns). Round-trip is a memcpy.
            // depth-24 carries the alpha byte through (clients
            // ignore the X-byte position).
            Ok(raw.to_vec())
        }
        8 => {
            // Scanline padded to 32 bits.
            let row_dst_bytes = (w as usize + 3) & !3;
            let mut out = vec![0u8; row_dst_bytes * h as usize];
            for row in 0..h as usize {
                let src_off = row * w as usize;
                let dst_off = row * row_dst_bytes;
                out[dst_off..dst_off + w as usize]
                    .copy_from_slice(&raw[src_off..src_off + w as usize]);
            }
            Ok(out)
        }
        4 => {
            // Two pixels per byte, low nibble first, rows padded to 32 bits.
            let row_dst_bytes = w.div_ceil(8) as usize * 4;
            let mut out = vec![0u8; row_dst_bytes * h as usize];
            for row in 0..h as usize {
                let src_off = row * w as usize;
                let dst_off = row * row_dst_bytes;
                for col in 0..w as usize {
                    let nibble = raw[src_off + col] & 0x0f;
                    let dst = &mut out[dst_off + col / 2];
                    if col % 2 == 0 {
                        *dst = (*dst & 0xf0) | nibble;
                    } else {
                        *dst = (*dst & 0x0f) | (nibble << 4);
                    }
                }
            }
            Ok(out)
        }
        1 => {
            // Pack 0xFF/0x00 bytes back to 1bpp; scanline
            // padded to 32 bits. Bit order matches the server's
            // advertised `bitmap-bit-order` (LSBFirst when the
            // client requested it, which is the x86 default); bit
            // 0 of a byte is pixel 0 in that 8-pixel group.
            // Mirrors `unpack_to_staging`'s depth-1 branch above.
            let row_bytes = w.div_ceil(32) as usize * 4;
            let mut out = vec![0u8; row_bytes * h as usize];
            for row in 0..h as usize {
                let src_off = row * w as usize;
                let dst_off = row * row_bytes;
                for col in 0..w as usize {
                    if raw[src_off + col] != 0 {
                        out[dst_off + col / 8] |= 1 << (col % 8);
                    }
                }
            }
            Ok(out)
        }
        _ => Err(RenderError::UnsupportedDepth(depth)),
    }
}

/// Decode an X11 32-bit pixel (B in low byte, then G, R, A) into
/// an RGBA float-4 suitable for `vkCmdClearAttachments` against a
/// `B8G8R8A8_UNORM` target.
#[must_use]
pub(crate) fn decode_x11_pixel_bgra(pixel: u32) -> [f32; 4] {
    let b = (pixel & 0xff) as f32 / 255.0;
    let g = ((pixel >> 8) & 0xff) as f32 / 255.0;
    let r = ((pixel >> 16) & 0xff) as f32 / 255.0;
    let a = ((pixel >> 24) & 0xff) as f32 / 255.0;
    // `vkCmdClearAttachments` clearColor.float32 against a
    // BGRA8_UNORM attachment writes [R, G, B, A] components per
    // spec — the format swizzle handles the BGRA→RGBA mapping at
    // store time. So we pass logical RGBA here.
    [r, g, b, a]
}

/// L1 server-α invariant: depth-24 / depth-8 / depth-1 destinations
/// are server-owned-α, so the stored alpha byte must read back as
/// `0xFF` regardless of what the X11 pixel's upper byte happens to
/// contain (typically `0x00` for `0x00RRGGBB` colour literals). The
/// scene compositor binds `storage.image_view` (IDENTITY swizzle —
/// required because the same view doubles as a colour attachment per
/// VUID-VkFramebufferCreateInfo-pAttachments-00891) and runs window
/// draws in `alpha_passthrough=true` mode, so a paint that leaves
/// α=0 in storage renders as a fully-transparent window — the layer
/// underneath leaks through. v1 forces this at every fill site
/// (`kms/backend.rs:try_vk_solid_fill`); this helper is v2's
/// equivalent.
#[must_use]
pub(crate) fn decode_x11_pixel_server_alpha(pixel: u32, depth: u8) -> [f32; 4] {
    let mut c = decode_x11_pixel_bgra(pixel);
    if depth != 32 {
        c[3] = 1.0;
    }
    c
}

/// Decode an X11 pixel for direct storage writes.
///
/// `R8_UNORM` targets are alpha-mask style storages, so the byte must
/// land in the attachment's first component, not the BGRA low byte
/// interpretation used by `decode_x11_pixel_server_alpha`.
#[must_use]
pub(crate) fn decode_x11_pixel_for_storage(pixel: u32, depth: u8, format: vk::Format) -> [f32; 4] {
    if format == vk::Format::R8_UNORM {
        [
            (pixel & 0xff) as f32 / 255.0,
            0.0,
            0.0,
            if depth == 32 {
                ((pixel >> 24) & 0xff) as f32 / 255.0
            } else {
                1.0
            },
        ]
    } else {
        decode_x11_pixel_server_alpha(pixel, depth)
    }
}

// ────────────────────────────────────────────────────────────────
// Tests — logic-only (no live Vk).
//
// Vk-backed integration tests are gated by `#[ignore = "needs live
// Vulkan ICD"]` so they run only when explicitly requested. The
// Stage 2 acceptance harness (Stage 2f) drives them end-to-end.
// ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_open_frame_with_no_open_frame_returns_already_closed() {
        let mut engine = RenderEngine::stub();
        let mut store = DrawableStore::stub();
        let mut platform = PlatformBackend::for_tests();
        let out = engine
            .close_open_frame(
                &mut store,
                &mut platform,
                super::super::frame_builder::CloseReason::Shutdown,
            )
            .expect("close on a closed frame must Ok");
        assert!(matches!(
            out,
            super::super::frame_builder::CloseOutcome::AlreadyClosed
        ));
    }

    #[test]
    fn stub_engine_declines_paint_ops() {
        let mut engine = RenderEngine::stub();
        let mut store = DrawableStore::new();
        let mut platform = PlatformBackend::for_tests();
        let storage = super::super::store::Storage::for_tests_null(
            vk::Extent2D {
                width: 4,
                height: 4,
            },
            vk::Format::B8G8R8A8_UNORM,
        );
        let id = store
            .allocate(
                0x1,
                super::super::store::DrawableKind::Pixmap,
                32,
                false,
                storage,
            )
            .unwrap();
        let err = engine
            .fill_rect(
                &mut store,
                &mut platform,
                id,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 4,
                        height: 4,
                    },
                },
                [1.0, 0.0, 0.0, 1.0],
            )
            .expect_err("stub engine must reject");
        assert!(matches!(err, RenderError::NoVk));
        assert!(!engine.is_live());
    }

    #[test]
    fn decode_pixel_bgra_round_trip() {
        // 0xAARRGGBB → r,g,b,a in 0..1
        let rgba = decode_x11_pixel_bgra(0xFF_80_40_20);
        assert!((rgba[0] - 128.0 / 255.0).abs() < 1e-3); // R = 0x80
        assert!((rgba[1] - 64.0 / 255.0).abs() < 1e-3); // G = 0x40
        assert!((rgba[2] - 32.0 / 255.0).abs() < 1e-3); // B = 0x20
        assert!((rgba[3] - 255.0 / 255.0).abs() < 1e-3); // A = 0xFF
    }

    #[test]
    fn x11_row_stride_pad_to_32_bits() {
        // depth-1, width 9 → 9 bits → ceil(9/32)*4 = 4 bytes.
        assert_eq!(x11_src_row_stride(1, 9), 4);
        // depth-1, width 33 → ceil(33/32)*4 = 8.
        assert_eq!(x11_src_row_stride(1, 33), 8);
        // depth-4 is nibble-packed and padded to 32 bits.
        assert_eq!(x11_src_row_stride(4, 3), 4);
        assert_eq!(x11_src_row_stride(4, 9), 8);
        // depth-8, width 3 → 24 bits padded to 32 → 4 bytes.
        assert_eq!(x11_src_row_stride(8, 3), 4);
        // depth-8, width 5 → 40 bits padded to 64 → 8 bytes.
        assert_eq!(x11_src_row_stride(8, 5), 8);
        // depth-32, width 10 → 320 bits = 40 bytes (already aligned).
        assert_eq!(x11_src_row_stride(32, 10), 40);
    }

    #[test]
    fn clamp_put_rect_inside_returns_unchanged() {
        let r = clamp_put_rect(
            vk::Offset2D { x: 2, y: 3 },
            vk::Extent2D {
                width: 4,
                height: 5,
            },
            vk::Extent2D {
                width: 16,
                height: 16,
            },
        )
        .unwrap();
        assert_eq!(r.0.offset, vk::Offset2D { x: 2, y: 3 });
        assert_eq!(
            r.0.extent,
            vk::Extent2D {
                width: 4,
                height: 5,
            },
        );
        assert_eq!(r.1, (0, 0));
    }

    #[test]
    fn clamp_put_rect_partial_clip_records_source_offset() {
        // dst_pos = (-1, -2), src 4×5 against a 16×16 storage →
        // dst rect (0,0,3,3) with source-input origin (1, 2).
        let r = clamp_put_rect(
            vk::Offset2D { x: -1, y: -2 },
            vk::Extent2D {
                width: 4,
                height: 5,
            },
            vk::Extent2D {
                width: 16,
                height: 16,
            },
        )
        .unwrap();
        assert_eq!(r.0.offset, vk::Offset2D { x: 0, y: 0 });
        assert_eq!(
            r.0.extent,
            vk::Extent2D {
                width: 3,
                height: 3,
            },
        );
        assert_eq!(r.1, (1, 2));
    }

    #[test]
    fn clamp_put_rect_outside_returns_none() {
        let r = clamp_put_rect(
            vk::Offset2D { x: 100, y: 100 },
            vk::Extent2D {
                width: 4,
                height: 4,
            },
            vk::Extent2D {
                width: 16,
                height: 16,
            },
        );
        assert!(r.is_none());
    }

    #[test]
    fn depth1_unpack_round_trip() {
        // 1×8 source padded to a 32-bit scanline (4 bytes). Bit
        // order LSB-first per the server's advertised
        // `bitmap-bit-order`: 0xAA = 1010_1010 = bits 1, 3, 5, 7
        // set → pixels 1, 3, 5, 7 set. Remaining 3 bytes are
        // scanline pad.
        let src = vec![0xAAu8, 0x00, 0x00, 0x00];
        let src_extent = vk::Extent2D {
            width: 8,
            height: 1,
        };
        let mut out = vec![0u8; 8];
        unpack_to_staging(&src, src_extent, 0, 0, 8, 1, 1, out.as_mut_ptr()).unwrap();
        assert_eq!(out, vec![0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF]);

        let packed = pack_from_storage(&out, 8, 1, 1).unwrap();
        // Row stride is 4 bytes (32 bits) per depth-1 pad rule;
        // the first byte holds the data, repacked LSB-first →
        // 0xAA round-trips (the byte is self-symmetric under
        // pack/unpack inversion).
        assert_eq!(packed.len(), 4);
        assert_eq!(packed[0], 0xAA);
    }

    #[test]
    fn depth32_unpack_is_memcpy() {
        // 2×2 BGRA8 source.
        let src: Vec<u8> = vec![
            0x10, 0x20, 0x30, 0xFF, 0x11, 0x21, 0x31, 0xFF, // row 0
            0x12, 0x22, 0x32, 0xFF, 0x13, 0x23, 0x33, 0xFF, // row 1
        ];
        let src_extent = vk::Extent2D {
            width: 2,
            height: 2,
        };
        let mut out = vec![0u8; 16];
        unpack_to_staging(&src, src_extent, 0, 0, 2, 2, 32, out.as_mut_ptr()).unwrap();
        assert_eq!(out, src);
    }

    #[test]
    fn depth4_unpack_and_pack_follow_nibble_layout() {
        let src = vec![0x21u8, 0x00, 0x00, 0x00];
        let src_extent = vk::Extent2D {
            width: 2,
            height: 1,
        };
        let mut out = vec![0u8; 2];
        unpack_to_staging(&src, src_extent, 0, 0, 2, 1, 4, out.as_mut_ptr()).unwrap();
        assert_eq!(out, vec![0x01, 0x02]);

        let packed = pack_from_storage(&out, 2, 1, 4).unwrap();
        assert_eq!(packed, vec![0x21, 0x00, 0x00, 0x00]);
    }

    // ── Vk-backed integration tests ─────────────────────────────
    //
    // Each `#[ignore]` test needs a live Vulkan ICD (lavapipe is
    // fine). Run with:
    //   `cargo test -p yserver --lib kms::v2::engine::tests:: -- --ignored`
    // The Stage 2 acceptance harness (Stage 2f) folds these into
    // the synthetic acceptance binary.

    fn live_platform() -> Option<PlatformBackend> {
        // Can't reuse `PlatformBackend::open_with_commit` here —
        // it tries to acquire a real DRM device. Tests need a
        // VkContext-only fixture. We build one by hand:
        // construct a `for_tests` fixture, then swap in a real
        // VkContext + OpsCommandPool + FencePool.
        let mut p = PlatformBackend::for_tests();
        let vk = match VkContext::new() {
            Ok(v) => v,
            Err(_) => return None,
        };
        let ops_pool = match crate::kms::vk::ops::OpsCommandPool::new(Arc::clone(&vk)) {
            Ok(o) => o,
            Err(_) => return None,
        };
        let fence_pool = super::super::platform::FencePool::new(Arc::clone(&vk));
        p.vk = Some(vk);
        p.ops_command_pool = Some(ops_pool);
        p.fence_pool = Some(fence_pool);
        Some(p)
    }

    /// Alias of `live_platform` used by Task 3 tests.
    fn try_for_tests_with_vk() -> Option<PlatformBackend> {
        live_platform()
    }

    /// Allocate a pixmap drawable in `store` backed by a real Vk
    /// storage. Returns the `DrawableId`. Used by Task 3 tests.
    fn create_pixmap(
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        xid: u32,
        w: u16,
        h: u16,
        depth: u8,
    ) -> Result<DrawableId, RenderError> {
        let storage = platform
            .allocate_drawable_storage(w, h, depth)
            .map_err(RenderError::Vk)?;
        store
            .allocate(
                xid,
                super::super::store::DrawableKind::Pixmap,
                depth,
                false,
                storage,
            )
            .map_err(|_| RenderError::NoVk)
    }

    /// Task 4 test helper: drive N `render_composite` (OP_OVER,
    /// `src` → `dst`, no mask) calls, one per `(x_off, y_off, w, h)`
    /// tuple. All calls share the same dst+src so the render-batch
    /// coalescer can aggregate them into a single CB.
    ///
    /// Panics if any call returns an error.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    fn drive_render_composite_same_key_for_tests(
        engine: &mut RenderEngine,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        dst: DrawableId,
        src: DrawableId,
        rects: &[(i32, i32, u32, u32)],
    ) {
        const OP_OVER: u8 = 3;
        for &(x_off, y_off, w, h) in rects {
            let composite_rect = [crate::kms::vk::ops::render::CompositeRect {
                src_x: x_off,
                src_y: y_off,
                mask_x: 0,
                mask_y: 0,
                dst_x: x_off,
                dst_y: y_off,
                width: w,
                height: h,
            }];
            engine
                .render_composite(
                    store,
                    platform,
                    OP_OVER,
                    ResolvedSource::Drawable(src),
                    ResolvedSource::None,
                    dst,
                    &composite_rect,
                    None,
                    Repeat::None,
                    Repeat::None,
                    None,
                    None,
                    false,
                    0,
                    0,
                    0,
                )
                .expect("render_composite");
        }
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn depth32_put_image_get_image_round_trip() {
        let Some(mut platform) = live_platform() else {
            eprintln!("no VkContext available — skipping");
            return;
        };
        let mut store = DrawableStore::new();
        let mut engine = RenderEngine::new(&platform).expect("engine");

        let storage = platform
            .allocate_drawable_storage(8, 8, 32)
            .expect("alloc storage");
        let id = store
            .allocate(
                0x1,
                super::super::store::DrawableKind::Pixmap,
                32,
                false,
                storage,
            )
            .expect("store.allocate");

        // 8x8 BGRA8 gradient.
        let mut src = vec![0u8; 8 * 8 * 4];
        for y in 0..8 {
            for x in 0..8 {
                let off = (y * 8 + x) * 4;
                src[off] = (x * 32) as u8; // B
                src[off + 1] = (y * 32) as u8; // G
                src[off + 2] = ((x + y) * 16) as u8; // R
                src[off + 3] = 0xFF; // A
            }
        }
        engine
            .put_image(
                &mut store,
                &mut platform,
                id,
                vk::Offset2D { x: 0, y: 0 },
                vk::Extent2D {
                    width: 8,
                    height: 8,
                },
                &src,
                32,
            )
            .expect("put_image");

        let out = engine
            .get_image(
                &mut store,
                &mut platform,
                id,
                vk::Rect2D {
                    offset: vk::Offset2D { x: 0, y: 0 },
                    extent: vk::Extent2D {
                        width: 8,
                        height: 8,
                    },
                },
                32,
            )
            .expect("get_image");
        assert_eq!(out, src, "depth-32 round-trip must be byte-identical");

        engine.drain_all(&mut platform);
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn fill_then_get_image_observes_clear_color() {
        let Some(mut platform) = live_platform() else {
            eprintln!("no VkContext available — skipping");
            return;
        };
        let mut store = DrawableStore::new();
        let mut engine = RenderEngine::new(&platform).expect("engine");

        let storage = platform.allocate_drawable_storage(4, 4, 32).expect("alloc");
        let id = store
            .allocate(
                0x1,
                super::super::store::DrawableKind::Pixmap,
                32,
                false,
                storage,
            )
            .unwrap();

        // Fill the whole pixmap with bright red (R=0xFF, G=0, B=0, A=0xFF).
        let color = decode_x11_pixel_bgra(0xFF_FF_00_00);
        engine
            .fill_rect(
                &mut store,
                &mut platform,
                id,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 4,
                        height: 4,
                    },
                },
                color,
            )
            .expect("fill_rect");

        let out = engine
            .get_image(
                &mut store,
                &mut platform,
                id,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 4,
                        height: 4,
                    },
                },
                32,
            )
            .expect("get_image");
        // Storage is BGRA8: every pixel should be [B=0, G=0, R=0xFF, A=0xFF].
        for px in out.chunks_exact(4) {
            assert_eq!(px[0], 0x00, "B");
            assert_eq!(px[1], 0x00, "G");
            assert_eq!(px[2], 0xFF, "R");
            assert_eq!(px[3], 0xFF, "A");
        }

        engine.drain_all(&mut platform);
    }

    /// `fill_rect` must write the source byte into `R8_UNORM`
    /// storage, not treat it like BGRA. This locks the depth-8
    /// GXcopy path that Xlib9 `XFillRectangle` exercises.
    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn fill_depth8_observes_r8_source_byte() {
        let Some(mut platform) = live_platform() else {
            eprintln!("no VkContext available — skipping");
            return;
        };
        let mut store = DrawableStore::new();
        let mut engine = RenderEngine::new(&platform).expect("engine");

        let storage = platform.allocate_drawable_storage(4, 4, 8).expect("alloc");
        let id = store
            .allocate(
                0x1,
                super::super::store::DrawableKind::Pixmap,
                8,
                false,
                storage,
            )
            .unwrap();

        let color = decode_x11_pixel_for_storage(0x01, 8, vk::Format::R8_UNORM);
        engine
            .fill_rect(
                &mut store,
                &mut platform,
                id,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 4,
                        height: 4,
                    },
                },
                color,
            )
            .expect("fill_rect");

        let out = engine
            .get_image(
                &mut store,
                &mut platform,
                id,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 4,
                        height: 4,
                    },
                },
                8,
            )
            .expect("get_image");
        for b in out {
            assert_eq!(b, 0x01, "R8 fill must preserve the source byte");
        }

        engine.drain_all(&mut platform);
    }

    /// Stage 3f.2: `engine.logic_fill` applies the per-`GcFunction`
    /// `VkLogicOp` per pixel. Drives `Xor` against a pre-loaded BGRA8
    /// pattern; expects each component to be the pre-load XOR'd with
    /// the fg byte. Alpha is preserved via the `opaque_alpha=true`
    /// pipeline (L1 server-α invariant on depth-24).
    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn logic_fill_xor_applies_per_pixel() {
        use yserver_core::backend::GcFunction;

        let Some(mut platform) = live_platform() else {
            eprintln!("no VkContext available — skipping");
            return;
        };
        let mut store = DrawableStore::new();
        let mut engine = RenderEngine::new(&platform).expect("engine");

        // 4x4 BGRA8 pixmap. Store BGRA wire bytes B, G, R, A.
        let storage = platform.allocate_drawable_storage(4, 4, 24).expect("alloc");
        let id = store
            .allocate(
                0x1,
                super::super::store::DrawableKind::Pixmap,
                24,
                false,
                storage,
            )
            .unwrap();

        // Load every pixel with [B=0x20, G=0x40, R=0x80, A=0xFF].
        let mut pre = vec![0u8; 4 * 4 * 4];
        for px in pre.chunks_exact_mut(4) {
            px[0] = 0x20;
            px[1] = 0x40;
            px[2] = 0x80;
            px[3] = 0xFF;
        }
        engine
            .put_image(
                &mut store,
                &mut platform,
                id,
                vk::Offset2D::default(),
                vk::Extent2D {
                    width: 4,
                    height: 4,
                },
                &pre,
                32,
            )
            .expect("put_image");

        // XOR with fg pixel 0x00FFFFFF (X11 wire = AARRGGBB: A=0,
        // R=0xFF, G=0xFF, B=0xFF). The recorder's `BGRA8_UNORM`
        // branch puts R/G/B into [0]/[1]/[2] of `fg_color`; the
        // logic-op output then targets the BGRA8 attachment in the
        // same channel order, so post-XOR every component reads as
        // `pre ^ 0xFF`.
        let rect = Rectangle16 {
            x: 0,
            y: 0,
            width: 4,
            height: 4,
        };
        engine
            .logic_fill(
                &mut store,
                &mut platform,
                id,
                GcFunction::Xor,
                /* opaque_alpha */ true,
                /* fg */ 0x00FF_FFFF,
                &[rect],
            )
            .expect("logic_fill");

        let out = engine
            .get_image(
                &mut store,
                &mut platform,
                id,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 4,
                        height: 4,
                    },
                },
                32,
            )
            .expect("get_image");

        for px in out.chunks_exact(4) {
            assert_eq!(px[0], 0x20 ^ 0xFF, "B (XOR pre 0x20 with fg 0xFF)");
            assert_eq!(px[1], 0x40 ^ 0xFF, "G (XOR pre 0x40 with fg 0xFF)");
            assert_eq!(px[2], 0x80 ^ 0xFF, "R (XOR pre 0x80 with fg 0xFF)");
            // opaque_alpha=true: alpha channel mask drops alpha from
            // the LogicOp, so the destination's pre-load 0xFF is
            // preserved.
            assert_eq!(px[3], 0xFF, "A preserved by opaque_alpha mask");
        }

        engine.drain_all(&mut platform);
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn copy_area_disjoint_pixmaps_round_trip() {
        let Some(mut platform) = live_platform() else {
            eprintln!("no VkContext available — skipping");
            return;
        };
        let mut store = DrawableStore::new();
        let mut engine = RenderEngine::new(&platform).expect("engine");

        let storage_src = platform.allocate_drawable_storage(4, 4, 32).unwrap();
        let storage_dst = platform.allocate_drawable_storage(8, 4, 32).unwrap();
        let src = store
            .allocate(
                0x1,
                super::super::store::DrawableKind::Pixmap,
                32,
                false,
                storage_src,
            )
            .unwrap();
        let dst = store
            .allocate(
                0x2,
                super::super::store::DrawableKind::Pixmap,
                32,
                false,
                storage_dst,
            )
            .unwrap();

        // Fill src with red.
        let red = decode_x11_pixel_bgra(0xFF_FF_00_00);
        engine
            .fill_rect(
                &mut store,
                &mut platform,
                src,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 4,
                        height: 4,
                    },
                },
                red,
            )
            .unwrap();
        // Fill dst with blue.
        let blue = decode_x11_pixel_bgra(0xFF_00_00_FF);
        engine
            .fill_rect(
                &mut store,
                &mut platform,
                dst,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 8,
                        height: 4,
                    },
                },
                blue,
            )
            .unwrap();
        // Copy src into dst at (4, 0).
        engine
            .copy_area(
                &mut store,
                &mut platform,
                src,
                dst,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 4,
                        height: 4,
                    },
                },
                vk::Offset2D { x: 4, y: 0 },
            )
            .unwrap();

        let out = engine
            .get_image(
                &mut store,
                &mut platform,
                dst,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 8,
                        height: 4,
                    },
                },
                32,
            )
            .unwrap();
        // Left half (0..4) should be blue (B=0xFF, G=0, R=0, A=0xFF).
        for y in 0..4 {
            for x in 0..4 {
                let off = (y * 8 + x) * 4;
                assert_eq!(&out[off..off + 4], &[0xFF, 0x00, 0x00, 0xFF], "left blue");
            }
        }
        // Right half (4..8) should be red (B=0, G=0, R=0xFF, A=0xFF).
        for y in 0..4 {
            for x in 4..8 {
                let off = (y * 8 + x) * 4;
                assert_eq!(&out[off..off + 4], &[0x00, 0x00, 0xFF, 0xFF], "right red");
            }
        }

        engine.drain_all(&mut platform);
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn logic_fill_depth32_preserves_wire_alpha_when_not_opaque() {
        use yserver_core::backend::GcFunction;

        let Some(mut platform) = live_platform() else {
            eprintln!("no VkContext available — skipping");
            return;
        };
        let mut store = DrawableStore::new();
        let mut engine = RenderEngine::new(&platform).expect("engine");

        let storage = platform
            .allocate_drawable_storage(2, 2, 32)
            .expect("storage");
        let id = store
            .allocate(
                0x1,
                super::super::store::DrawableKind::Pixmap,
                32,
                false,
                storage,
            )
            .expect("alloc");

        engine
            .fill_rect(
                &mut store,
                &mut platform,
                id,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 2,
                        height: 2,
                    },
                },
                decode_x11_pixel_bgra(0),
            )
            .expect("clear");

        engine
            .logic_fill(
                &mut store,
                &mut platform,
                id,
                GcFunction::Copy,
                /* opaque_alpha */ false,
                /* fg */ 0x0000_0001,
                &[Rectangle16 {
                    x: 0,
                    y: 0,
                    width: 2,
                    height: 2,
                }],
            )
            .expect("logic_fill");

        let out = engine
            .get_image(
                &mut store,
                &mut platform,
                id,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 2,
                        height: 2,
                    },
                },
                32,
            )
            .expect("get_image");

        for px in out.chunks_exact(4) {
            assert_eq!(px, &[0x01, 0x00, 0x00, 0x00]);
        }

        engine.drain_all(&mut platform);
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn logic_fill_r8_not_family_matches_x11_bytes() {
        use yserver_core::backend::GcFunction;

        let Some(mut platform) = live_platform() else {
            eprintln!("no VkContext available — skipping");
            return;
        };
        let mut store = DrawableStore::new();
        let mut engine = RenderEngine::new(&platform).expect("engine");

        let storage = platform
            .allocate_drawable_storage(2, 1, 8)
            .expect("storage");
        let id = store
            .allocate(
                0x1,
                super::super::store::DrawableKind::Pixmap,
                8,
                false,
                storage,
            )
            .expect("alloc");

        // Preload dst bytes [0x00, 0x03].
        let pre = vec![0x00, 0x03, 0x00, 0x00];
        engine
            .put_image(
                &mut store,
                &mut platform,
                id,
                vk::Offset2D::default(),
                vk::Extent2D {
                    width: 2,
                    height: 1,
                },
                &pre,
                8,
            )
            .expect("put_image");

        let rect = Rectangle16 {
            x: 0,
            y: 0,
            width: 2,
            height: 1,
        };

        engine
            .logic_fill(
                &mut store,
                &mut platform,
                id,
                GcFunction::Set,
                /* opaque_alpha */ true,
                /* fg */ 0,
                &[rect],
            )
            .expect("logic_fill set");
        let out = engine
            .get_image(
                &mut store,
                &mut platform,
                id,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 2,
                        height: 1,
                    },
                },
                8,
            )
            .expect("get_image set");
        assert_eq!(&out[..2], &[0xff, 0xff], "GXset must write all 1 bits");

        engine
            .put_image(
                &mut store,
                &mut platform,
                id,
                vk::Offset2D::default(),
                vk::Extent2D {
                    width: 2,
                    height: 1,
                },
                &pre,
                8,
            )
            .expect("put_image reload");
        engine
            .logic_fill(
                &mut store,
                &mut platform,
                id,
                GcFunction::Invert,
                /* opaque_alpha */ true,
                /* fg */ 0,
                &[rect],
            )
            .expect("logic_fill invert");
        let out = engine
            .get_image(
                &mut store,
                &mut platform,
                id,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 2,
                        height: 1,
                    },
                },
                8,
            )
            .expect("get_image invert");
        assert_eq!(&out[..2], &[0xff, 0xfc], "GXinvert must flip all 8 bits");

        engine.drain_all(&mut platform);
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn copy_area_self_overlap_scratch_path() {
        let Some(mut platform) = live_platform() else {
            eprintln!("no VkContext available — skipping");
            return;
        };
        let mut store = DrawableStore::new();
        let mut engine = RenderEngine::new(&platform).expect("engine");

        let storage = platform.allocate_drawable_storage(8, 1, 32).unwrap();
        let id = store
            .allocate(
                0x1,
                super::super::store::DrawableKind::Pixmap,
                32,
                false,
                storage,
            )
            .unwrap();

        // PutImage a horizontal gradient: 8 pixels each with a
        // distinct red value.
        let mut src = vec![0u8; 8 * 4];
        for x in 0..8 {
            let off = x * 4;
            src[off] = 0x00; // B
            src[off + 1] = 0x00; // G
            src[off + 2] = (x as u8) * 0x20; // R
            src[off + 3] = 0xFF; // A
        }
        engine
            .put_image(
                &mut store,
                &mut platform,
                id,
                vk::Offset2D::default(),
                vk::Extent2D {
                    width: 8,
                    height: 1,
                },
                &src,
                32,
            )
            .unwrap();
        // Copy (0..4) → (2..6) (overlap; scratch path engages).
        engine
            .copy_area(
                &mut store,
                &mut platform,
                id,
                id,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 4,
                        height: 1,
                    },
                },
                vk::Offset2D { x: 2, y: 0 },
            )
            .unwrap();

        let out = engine
            .get_image(
                &mut store,
                &mut platform,
                id,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 8,
                        height: 1,
                    },
                },
                32,
            )
            .unwrap();
        // Expected R-channel sequence: [0, 0x20, 0, 0x20, 0x40, 0x60, 0xC0, 0xE0]
        // After copy of (0..4) → (2..6):
        //   col 0: original (R=0)
        //   col 1: original (R=0x20)
        //   col 2: src col 0 (R=0)
        //   col 3: src col 1 (R=0x20)
        //   col 4: src col 2 (R=0x40)
        //   col 5: src col 3 (R=0x60)
        //   col 6: original col 6 (R=0xC0)
        //   col 7: original col 7 (R=0xE0)
        let expected_r = [0x00, 0x20, 0x00, 0x20, 0x40, 0x60, 0xC0, 0xE0];
        for (x, &exp) in expected_r.iter().enumerate() {
            let off = x * 4 + 2;
            assert_eq!(
                out[off], exp,
                "R at col {x} (got {:#x}, want {exp:#x})",
                out[off]
            );
        }

        engine.drain_all(&mut platform);
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn put_image_then_fill_overwrites() {
        let Some(mut platform) = live_platform() else {
            eprintln!("no VkContext available — skipping");
            return;
        };
        let mut store = DrawableStore::new();
        let mut engine = RenderEngine::new(&platform).expect("engine");

        let storage = platform.allocate_drawable_storage(4, 4, 32).expect("alloc");
        let id = store
            .allocate(
                0x1,
                super::super::store::DrawableKind::Pixmap,
                32,
                false,
                storage,
            )
            .unwrap();

        // PutImage all-blue, then fill (1,1)..(3,3) with green.
        // B=0xFF, G=0, R=0, A=0xFF
        let blue = [0xFFu8, 0x00, 0x00, 0xFF].repeat(16);
        engine
            .put_image(
                &mut store,
                &mut platform,
                id,
                vk::Offset2D::default(),
                vk::Extent2D {
                    width: 4,
                    height: 4,
                },
                &blue,
                32,
            )
            .unwrap();
        let green = decode_x11_pixel_bgra(0xFF_00_FF_00);
        engine
            .fill_rect(
                &mut store,
                &mut platform,
                id,
                vk::Rect2D {
                    offset: vk::Offset2D { x: 1, y: 1 },
                    extent: vk::Extent2D {
                        width: 2,
                        height: 2,
                    },
                },
                green,
            )
            .unwrap();

        let out = engine
            .get_image(
                &mut store,
                &mut platform,
                id,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 4,
                        height: 4,
                    },
                },
                32,
            )
            .unwrap();
        // (0,0) still blue.
        assert_eq!(&out[0..4], &[0xFF, 0x00, 0x00, 0xFF]);
        // (1,1) green: B=0, G=0xFF, R=0, A=0xFF.
        let off_1_1 = (4 + 1) * 4;
        assert_eq!(&out[off_1_1..off_1_1 + 4], &[0x00, 0xFF, 0x00, 0xFF]);
        // (3,3) still blue.
        let off_3_3 = (3 * 4 + 3) * 4;
        assert_eq!(&out[off_3_3..off_3_3 + 4], &[0xFF, 0x00, 0x00, 0xFF]);

        engine.drain_all(&mut platform);
    }

    #[test]
    fn depth24_unpack_forces_alpha_ff() {
        // Source 1×1 with X-byte (alpha-slot) = 0x55.
        let src = vec![0x10u8, 0x20, 0x30, 0x55];
        let src_extent = vk::Extent2D {
            width: 1,
            height: 1,
        };
        let mut out = vec![0u8; 4];
        unpack_to_staging(&src, src_extent, 0, 0, 1, 1, 24, out.as_mut_ptr()).unwrap();
        assert_eq!(out, vec![0x10, 0x20, 0x30, 0xFF]);
    }

    // ── Stage 3a Vk-backed integration tests ────────────────────

    /// Helper: allocate a depth-32 storage and return a registered
    /// DrawableId. Mirrors the pattern Stage 2c tests use.
    fn alloc_drawable_3a(
        platform: &PlatformBackend,
        store: &mut DrawableStore,
        xid: u32,
        w: u16,
        h: u16,
    ) -> DrawableId {
        alloc_drawable_3a_with_kind(
            platform,
            store,
            xid,
            w,
            h,
            super::super::store::DrawableKind::Pixmap,
            false,
        )
    }

    fn alloc_drawable_3a_with_kind(
        platform: &PlatformBackend,
        store: &mut DrawableStore,
        xid: u32,
        w: u16,
        h: u16,
        kind: super::super::store::DrawableKind,
        scene_participating: bool,
    ) -> DrawableId {
        let storage = platform
            .allocate_drawable_storage(w, h, 32)
            .expect("alloc storage");
        store
            .allocate(xid, kind, 32, scene_participating, storage)
            .expect("store allocate")
    }

    /// Build a `PreparedGlyph` with `w × h` filled bytes (the
    /// fill byte is 0xFF so the shader paints solid foreground).
    fn build_glyph(codepoint: u32, dst_x: i32, dst_y: i32, w: usize, h: usize) -> PreparedGlyph {
        PreparedGlyph {
            dst_x,
            dst_y,
            w,
            h,
            pixels: vec![0xFF_u8; w * h],
            codepoint,
        }
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn image_text_run_records_damage_on_target() {
        let Some(mut platform) = live_platform() else {
            eprintln!("no VkContext available — skipping");
            return;
        };
        let mut store = DrawableStore::new();
        let mut engine = RenderEngine::new(&platform).expect("engine");

        // Window-kind + scene-participating so presentation damage
        // accumulates (per the I5 spec amendment, pixmaps no longer
        // accumulate any damage in the store — protocol DamageNotify
        // fanout lives at the request layer).
        let id = alloc_drawable_3a_with_kind(
            &platform,
            &mut store,
            0x1,
            64,
            32,
            super::super::store::DrawableKind::Window,
            true,
        );
        // Two glyphs spanning x=[10..22] × y=[5..17].
        let glyphs = vec![
            build_glyph(u32::from(b'A'), 10, 5, 6, 12),
            build_glyph(u32::from(b'B'), 16, 5, 6, 12),
        ];
        let stats = engine
            .image_text(
                &mut store,
                &mut platform,
                id,
                7,
                [1.0, 1.0, 1.0, 1.0],
                &glyphs,
            )
            .expect("image_text");
        assert_eq!(stats.atlas_interns, 2);
        assert_eq!(stats.glyph_uploads, 2);
        assert_eq!(stats.glyphs_dropped, 0);

        // Damage union covers the two glyph quads.
        let d = store.get(id).expect("drawable");
        let rects: Vec<vk::Rect2D> = d.presentation_damage.rects().to_vec();
        assert!(!rects.is_empty(), "presentation damage should be set");
        let mut min_x = i32::MAX;
        let mut min_y = i32::MAX;
        let mut max_x = i32::MIN;
        let mut max_y = i32::MIN;
        for r in rects {
            min_x = min_x.min(r.offset.x);
            min_y = min_y.min(r.offset.y);
            max_x = max_x.max(r.offset.x + r.extent.width as i32);
            max_y = max_y.max(r.offset.y + r.extent.height as i32);
        }
        assert!(min_x <= 10);
        assert!(min_y <= 5);
        assert!(max_x >= 22);
        assert!(max_y >= 17);

        engine.drain_all(&mut platform);
    }

    /// **Load-bearing per codex round 1**: two back-to-back glyph
    /// uploads with distinct keys must not corrupt each other's
    /// atlas pixels. v1's shared persistent staging would clobber
    /// A when B's memcpy lands while A's GPU read is in flight; the
    /// v2 per-upload arena slice rules that out.
    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn atlas_back_to_back_upload_no_corruption() {
        let Some(mut platform) = live_platform() else {
            eprintln!("no VkContext available — skipping");
            return;
        };
        let mut store = DrawableStore::new();
        let mut engine = RenderEngine::new(&platform).expect("engine");
        let target = alloc_drawable_3a(&platform, &mut store, 0x1, 32, 32);

        // Pre-clear the target to black.
        engine
            .fill_rect(
                &mut store,
                &mut platform,
                target,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 32,
                        height: 32,
                    },
                },
                [0.0, 0.0, 0.0, 1.0],
            )
            .expect("clear");

        // Two glyphs with distinguishable solid-alpha rectangles.
        // The text shader does `foreground × atlas.r`; with
        // 0xFF-filled atlas and white foreground, the dst quads
        // come out (B=0xFF, G=0xFF, R=0xFF, A=0xFF).
        let glyphs = vec![
            build_glyph(u32::from(b'A'), 1, 1, 4, 4),
            build_glyph(u32::from(b'B'), 10, 1, 4, 4),
        ];
        let stats = engine
            .image_text(
                &mut store,
                &mut platform,
                target,
                42,
                [1.0, 1.0, 1.0, 1.0],
                &glyphs,
            )
            .expect("image_text");
        assert_eq!(stats.atlas_interns, 2);

        // Read back: both quads should be white; pixels between
        // them should be the original black.
        let out = engine
            .get_image(
                &mut store,
                &mut platform,
                target,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 32,
                        height: 32,
                    },
                },
                32,
            )
            .expect("get_image");
        let pixel_at = |x: usize, y: usize| {
            let off = (y * 32 + x) * 4;
            (out[off], out[off + 1], out[off + 2], out[off + 3])
        };
        // A's quad: (1..5, 1..5).
        for y in 1..5 {
            for x in 1..5 {
                let (b, g, r, _a) = pixel_at(x, y);
                assert_eq!(
                    (b, g, r),
                    (0xFF, 0xFF, 0xFF),
                    "glyph A quad pixel ({x},{y}) corrupted: ({b:#x},{g:#x},{r:#x})",
                );
            }
        }
        // B's quad: (10..14, 1..5).
        for y in 1..5 {
            for x in 10..14 {
                let (b, g, r, _a) = pixel_at(x, y);
                assert_eq!(
                    (b, g, r),
                    (0xFF, 0xFF, 0xFF),
                    "glyph B quad pixel ({x},{y}) corrupted: ({b:#x},{g:#x},{r:#x})",
                );
            }
        }
        // Between the quads (7, 2) should still be black.
        let (b, g, r, _a) = pixel_at(7, 2);
        assert_eq!(
            (b, g, r),
            (0x00, 0x00, 0x00),
            "between-quad pixel (7,2) should be background black; got ({b:#x},{g:#x},{r:#x})"
        );

        engine.drain_all(&mut platform);
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn atlas_full_drops_glyph_and_increments_counter() {
        // Drive the atlas to exhaustion via the engine's image_text
        // pipeline. 4096² atlas; two 2049×2049 glyphs don't both
        // fit — the second exceeds the remaining vertical room.
        let Some(mut platform) = live_platform() else {
            eprintln!("no VkContext available — skipping");
            return;
        };
        let mut store = DrawableStore::new();
        let mut engine = RenderEngine::new(&platform).expect("engine");
        let target = alloc_drawable_3a(&platform, &mut store, 0x1, 4, 4);
        // First glyph fits.
        let g0 = build_glyph(1, 0, 0, 2049, 2049);
        let g1 = build_glyph(2, 0, 0, 2049, 2049);

        let stats = engine
            .image_text(
                &mut store,
                &mut platform,
                target,
                1,
                [1.0, 1.0, 1.0, 1.0],
                &[g0],
            )
            .expect("first image_text");
        assert_eq!(stats.atlas_interns, 1);
        assert_eq!(stats.glyphs_dropped, 0);

        let stats2 = engine
            .image_text(
                &mut store,
                &mut platform,
                target,
                1,
                [1.0, 1.0, 1.0, 1.0],
                &[g1],
            )
            .expect("second image_text");
        assert_eq!(stats2.atlas_interns, 0);
        assert_eq!(stats2.glyphs_dropped, 1);

        engine.drain_all(&mut platform);
    }

    // ── Stage 3c.3 acceptance tests ─────────────────────────────
    //
    // Engine-direct RENDER paint oracles. Each test allocates one
    // or two Vk-backed drawables, drives `render_composite` /
    // `render_fill_rectangles` through `RenderEngine`, then
    // round-trips via `get_image` and asserts pixel-level
    // correctness against a CPU oracle. The seventh acceptance
    // test (`render_composite_no_gc_clip_leak`) lives in
    // `tests/v2_acceptance.rs` because the "no GC clip leak"
    // property is a Backend-trait invariant (engine has no GC
    // clip notion).

    /// Allocate a Vk-backed depth-32 pixmap and pre-fill it with
    /// `color` via the engine's fill_rect path. Returns the
    /// store DrawableId.
    fn alloc_filled_pixmap(
        platform: &mut PlatformBackend,
        store: &mut DrawableStore,
        engine: &mut RenderEngine,
        xid: u32,
        w: u16,
        h: u16,
        color_bgra_premul: [f32; 4],
    ) -> DrawableId {
        let storage = platform
            .allocate_drawable_storage(w, h, 32)
            .expect("alloc storage");
        let id = store
            .allocate(
                xid,
                super::super::store::DrawableKind::Pixmap,
                32,
                false,
                storage,
            )
            .expect("store.allocate");
        engine
            .fill_rect(
                store,
                platform,
                id,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: u32::from(w),
                        height: u32::from(h),
                    },
                },
                color_bgra_premul,
            )
            .expect("pre-fill");
        id
    }

    fn full_rect(w: u32, h: u32) -> crate::kms::vk::ops::render::CompositeRect {
        crate::kms::vk::ops::render::CompositeRect {
            src_x: 0,
            src_y: 0,
            mask_x: 0,
            mask_y: 0,
            dst_x: 0,
            dst_y: 0,
            width: w,
            height: h,
        }
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn render_composite_over_renders_alpha_blended() {
        // 50%-alpha red (premultiplied: r=0.5, a=0.5) Over opaque
        // green. Over: out = src + dst * (1 - src.a).
        //   out.b = 0 + 0 * 0.5 = 0
        //   out.g = 0 + 1 * 0.5 = 0.5 → 0x80
        //   out.r = 0.5 + 0 * 0.5 = 0.5 → 0x80
        //   out.a = 0.5 + 1 * 0.5 = 1.0 → 0xFF
        let Some(mut platform) = live_platform() else {
            eprintln!("no Vk — skipping");
            return;
        };
        let mut store = DrawableStore::new();
        let mut engine = RenderEngine::new(&platform).expect("engine");

        let dst = alloc_filled_pixmap(
            &mut platform,
            &mut store,
            &mut engine,
            0x1,
            4,
            4,
            [0.0, 1.0, 0.0, 1.0], // opaque green
        );

        let stats = engine
            .render_composite(
                &mut store,
                &mut platform,
                3,                                           // Over
                ResolvedSource::Solid([0.5, 0.0, 0.0, 0.5]), // 50% red premul
                ResolvedSource::None,
                dst,
                &[full_rect(4, 4)],
                None,
                Repeat::None,
                Repeat::None,
                None,
                None,
                false,
                0,
                0,
                0,
            )
            .expect("render_composite");
        assert_eq!(stats.recorded_draws, 1);
        assert!(!stats.used_dst_readback);
        assert!(!stats.used_src_alias_scratch);

        let out = engine
            .get_image(
                &mut store,
                &mut platform,
                dst,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 4,
                        height: 4,
                    },
                },
                32,
            )
            .expect("get_image");
        // Centre pixel (1, 1): BGRA = [0, 0x80, 0x80, 0xFF] (±1).
        let off = (4 + 1) * 4;
        let near = |a: u8, b: u8| a.abs_diff(b) <= 2;
        assert!(near(out[off], 0x00), "B at centre: got {:#x}", out[off]);
        assert!(
            near(out[off + 1], 0x80),
            "G at centre: got {:#x}",
            out[off + 1]
        );
        assert!(
            near(out[off + 2], 0x80),
            "R at centre: got {:#x}",
            out[off + 2]
        );
        assert!(
            near(out[off + 3], 0xFF),
            "A at centre: got {:#x}",
            out[off + 3]
        );

        engine.drain_all(&mut platform);
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn render_composite_picture_clip_per_rect() {
        // Two disjoint clip rects with a hole between them; one
        // composite covering the union bbox must paint inside both
        // rects AND leave the hole untouched. Exercises plan §4's
        // per-rect scissoring against v1's union-bbox shortcut.
        let Some(mut platform) = live_platform() else {
            eprintln!("no Vk — skipping");
            return;
        };
        let mut store = DrawableStore::new();
        let mut engine = RenderEngine::new(&platform).expect("engine");

        let dst = alloc_filled_pixmap(
            &mut platform,
            &mut store,
            &mut engine,
            0x1,
            8,
            4,
            [0.0, 0.0, 1.0, 1.0], // RGBA: opaque blue
        );
        // Two clip rects with a 2-wide hole at x=3..=4.
        let clip = vec![
            Rectangle16 {
                x: 0,
                y: 0,
                width: 3,
                height: 4,
            },
            Rectangle16 {
                x: 5,
                y: 0,
                width: 3,
                height: 4,
            },
        ];
        engine
            .render_composite(
                &mut store,
                &mut platform,
                1,                                           // Src
                ResolvedSource::Solid([1.0, 0.0, 0.0, 1.0]), // RGBA: opaque red
                ResolvedSource::None,
                dst,
                &[full_rect(8, 4)],
                Some(&clip),
                Repeat::None,
                Repeat::None,
                None,
                None,
                false,
                0,
                0,
                0,
            )
            .expect("render_composite");
        // Verify observable output: red inside both clip rects, original
        // blue preserved in the 2-wide hole. (The internal `recorded_draws`
        // count is an implementation detail of the pre-rework submit path
        // and is intentionally not asserted — the pixels are the contract.)
        let out = engine
            .get_image(
                &mut store,
                &mut platform,
                dst,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 8,
                        height: 4,
                    },
                },
                32,
            )
            .expect("get_image");
        // BGRA layout: B at +0, R at +2.
        for y in 0..4 {
            for x in 0..8u32 {
                let off = (y * 8 + x as usize) * 4;
                let in_clip = (0..3).contains(&x) || (5..8).contains(&x);
                if in_clip {
                    assert_eq!(out[off + 2], 0xFF, "R painted at ({x},{y})");
                    assert_eq!(out[off], 0x00, "B cleared at ({x},{y})");
                } else {
                    // Hole (x=3..=4): original blue.
                    assert_eq!(out[off], 0xFF, "B preserved at ({x},{y})");
                    assert_eq!(out[off + 2], 0x00, "R untouched at ({x},{y})");
                }
            }
        }

        engine.drain_all(&mut platform);
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn render_composite_solid_fill_source_path() {
        // SolidFill source over (op=Src) an unrelated start colour —
        // every dst pixel must equal the source's premul colour.
        let Some(mut platform) = live_platform() else {
            eprintln!("no Vk — skipping");
            return;
        };
        let mut store = DrawableStore::new();
        let mut engine = RenderEngine::new(&platform).expect("engine");

        let dst = alloc_filled_pixmap(
            &mut platform,
            &mut store,
            &mut engine,
            0x1,
            4,
            4,
            [0.0, 0.0, 0.0, 1.0], // opaque black
        );
        engine
            .render_composite(
                &mut store,
                &mut platform,
                1,                                             // Src
                ResolvedSource::Solid([0.25, 0.5, 0.75, 1.0]), // RGBA premul
                ResolvedSource::None,
                dst,
                &[full_rect(4, 4)],
                None,
                Repeat::None,
                Repeat::None,
                None,
                None,
                false,
                0,
                0,
                0,
            )
            .expect("render_composite");
        let out = engine
            .get_image(
                &mut store,
                &mut platform,
                dst,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 4,
                        height: 4,
                    },
                },
                32,
            )
            .expect("get_image");
        // Storage BGRA bytes for RGBA(0.25, 0.5, 0.75, 1.0):
        // B=0.75→0xC0, G=0.5→0x80, R=0.25→0x40, A=1→0xFF.
        let near = |a: u8, b: u8| a.abs_diff(b) <= 1;
        for px in out.chunks_exact(4) {
            assert!(near(px[0], 0xC0), "B: {:#x}", px[0]);
            assert!(near(px[1], 0x80), "G: {:#x}", px[1]);
            assert!(near(px[2], 0x40), "R: {:#x}", px[2]);
            assert!(near(px[3], 0xFF), "A: {:#x}", px[3]);
        }
        engine.drain_all(&mut platform);
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn render_composite_linear_gradient_horizontal_two_stop() {
        // 256×1 dst pre-filled black; Composite Src + LinearGradient
        // source (p1=(0,0), p2=(256,0)<<16) with two stops:
        //   pos=0   black (0,0,0,1)
        //   pos=0xFFFFFFFF white (1,1,1,1)
        // Stage 3f.13 wires the LUT path — pixel n should read
        // roughly (n, n, n, 0xFF) ± a couple of units (NEAREST
        // sampler + LUT rounding).
        use crate::kms::vk::gradient::Stop;
        let Some(mut platform) = live_platform() else {
            eprintln!("no Vk — skipping");
            return;
        };
        let mut store = DrawableStore::new();
        let mut engine = RenderEngine::new(&platform).expect("engine");

        let dst = alloc_filled_pixmap(
            &mut platform,
            &mut store,
            &mut engine,
            0x1,
            256,
            1,
            [0.0, 0.0, 0.0, 1.0],
        );

        let grad_xid = 0xABBA_FACE_u32;
        engine
            .build_and_insert_linear_gradient(
                &platform,
                grad_xid,
                (0, 0),
                (256_i32 << 16, 0),
                &[
                    Stop {
                        pos: 0,
                        r: 0,
                        g: 0,
                        b: 0,
                        a: 0xFFFF,
                    },
                    // 16.16 fixed-point: 1.0 = 0x10000. Using i32::MAX
                    // here would put the second stop far past t=1.0,
                    // so `sample_stops` would lerp `(target - 0) /
                    // i32::MAX ≈ 0` and every LUT pixel would read
                    // the first stop (black).
                    Stop {
                        pos: 0x10000,
                        r: 0xFFFF,
                        g: 0xFFFF,
                        b: 0xFFFF,
                        a: 0xFFFF,
                    },
                ],
            )
            .expect("build gradient");

        let stats = engine
            .render_composite(
                &mut store,
                &mut platform,
                1, // Src — copy source to dst, no blend
                ResolvedSource::Gradient(grad_xid),
                ResolvedSource::None,
                dst,
                &[full_rect(256, 1)],
                None,
                Repeat::None,
                Repeat::None,
                None,
                None,
                false,
                0,
                0,
                0,
            )
            .expect("render_composite gradient");
        assert_eq!(stats.recorded_draws, 1);

        let out = engine
            .get_image(
                &mut store,
                &mut platform,
                dst,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 256,
                        height: 1,
                    },
                },
                32,
            )
            .expect("get_image");

        // Sample several points along the ramp; tolerate ±4 due to
        // NEAREST sampler + 8-bit LUT quantisation + premultiplied
        // colour conversion. Direction-of-travel + monotonicity is
        // the strong gate (rules out the 3f.12 first-stop collapse,
        // which would read 0 at every x).
        let bgra = |x: usize| (out[x * 4], out[x * 4 + 1], out[x * 4 + 2], out[x * 4 + 3]);
        let (b0, g0, r0, _a0) = bgra(0);
        let (bm, gm, rm, _am) = bgra(128);
        let (b255, g255, r255, _a255) = bgra(255);
        // x=0 is near-black; x=255 is near-white; x=128 sits between.
        assert!(b0 <= 4 && g0 <= 4 && r0 <= 4, "x=0 BGRA={:?}", bgra(0));
        assert!(
            b255 >= 0xF0 && g255 >= 0xF0 && r255 >= 0xF0,
            "x=255 BGRA={:?}",
            bgra(255),
        );
        assert!(
            (0x40..=0xC0).contains(&bm)
                && (0x40..=0xC0).contains(&gm)
                && (0x40..=0xC0).contains(&rm),
            "x=128 BGRA={:?} (expected mid-grey)",
            bgra(128),
        );

        // Cleanup so the gradient image is freed in this drain.
        engine.picture_paint_remove(grad_xid);
        engine.drain_all(&mut platform);
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn render_composite_radial_gradient_centred() {
        // 64×64 dst, radial gradient centred at (32,32) inner_r=0
        // outer_r=32, stops black→white. Center pixel should be
        // dark (t near 0 = first stop = black); border pixel should
        // be near-white.
        use crate::kms::vk::gradient::Stop;
        let Some(mut platform) = live_platform() else {
            eprintln!("no Vk — skipping");
            return;
        };
        let mut store = DrawableStore::new();
        let mut engine = RenderEngine::new(&platform).expect("engine");

        let dst = alloc_filled_pixmap(
            &mut platform,
            &mut store,
            &mut engine,
            0x1,
            64,
            64,
            [0.5, 0.5, 0.5, 1.0],
        );

        let grad_xid = 0xDEAD_BEEF_u32;
        engine
            .build_and_insert_radial_gradient(
                &platform,
                grad_xid,
                (32_i32 << 16, 32_i32 << 16, 0),
                (32_i32 << 16, 32_i32 << 16, 32_i32 << 16),
                &[
                    Stop {
                        pos: 0,
                        r: 0,
                        g: 0,
                        b: 0,
                        a: 0xFFFF,
                    },
                    // 16.16 fixed-point: 1.0 = 0x10000. See linear-
                    // gradient test above for why i32::MAX is wrong.
                    Stop {
                        pos: 0x10000,
                        r: 0xFFFF,
                        g: 0xFFFF,
                        b: 0xFFFF,
                        a: 0xFFFF,
                    },
                ],
            )
            .expect("build radial");

        let stats = engine
            .render_composite(
                &mut store,
                &mut platform,
                1, // Src
                ResolvedSource::Gradient(grad_xid),
                ResolvedSource::None,
                dst,
                &[full_rect(64, 64)],
                None,
                Repeat::None,
                Repeat::None,
                None,
                None,
                false,
                0,
                0,
                0,
            )
            .expect("render_composite radial");
        assert_eq!(stats.recorded_draws, 1);

        let out = engine
            .get_image(
                &mut store,
                &mut platform,
                dst,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 64,
                        height: 64,
                    },
                },
                32,
            )
            .expect("get_image");

        let bgra = |x: usize, y: usize| {
            let off = (y * 64 + x) * 4;
            (out[off], out[off + 1], out[off + 2], out[off + 3])
        };
        // Centre near-black, edge near-white.
        let (bc, gc, rc, _ac) = bgra(32, 32);
        assert!(
            bc < 0x40 && gc < 0x40 && rc < 0x40,
            "centre BGRA={:?} (expected dark)",
            bgra(32, 32),
        );
        // Corner is outside the unit circle for an inscribed
        // radial — pick a point on the rim instead (x=62, y=32 →
        // r ≈ 30/32).
        let (be, ge, re_, _ae) = bgra(62, 32);
        assert!(
            be > 0xC0 && ge > 0xC0 && re_ > 0xC0,
            "rim BGRA={:?} (expected near-white)",
            bgra(62, 32),
        );

        engine.picture_paint_remove(grad_xid);
        engine.drain_all(&mut platform);
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn render_composite_missing_gradient_picture_is_gap() {
        // Engine receives a ResolvedSource::Gradient(xid) for an
        // xid that has no picture_paint entry (LUT build failed or
        // dropped early). Must return stats with recorded_draws=0,
        // log a debug gap, and NOT panic.
        let Some(mut platform) = live_platform() else {
            eprintln!("no Vk — skipping");
            return;
        };
        let mut store = DrawableStore::new();
        let mut engine = RenderEngine::new(&platform).expect("engine");

        let dst = alloc_filled_pixmap(
            &mut platform,
            &mut store,
            &mut engine,
            0x1,
            4,
            4,
            [0.0, 0.0, 0.0, 1.0],
        );

        let stats = engine
            .render_composite(
                &mut store,
                &mut platform,
                1, // Src
                ResolvedSource::Gradient(0xC0FF_EE00),
                ResolvedSource::None,
                dst,
                &[full_rect(4, 4)],
                None,
                Repeat::None,
                Repeat::None,
                None,
                None,
                false,
                0,
                0,
                0,
            )
            .expect("render_composite Ok even on missing gradient");
        assert_eq!(stats.recorded_draws, 0);
        engine.drain_all(&mut platform);
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn render_composite_self_alias() {
        // src == dst: pre-fill with a vertical gradient, then
        // Composite(Over, dst, NoMask, dst). Over with itself on
        // opaque alpha yields self exactly (out = src + dst*(1-1) =
        // src). Without the scratch path the GPU samples a region
        // as it writes it — undefined behaviour; with it, the
        // result must be bit-identical to the pre-fill.
        let Some(mut platform) = live_platform() else {
            eprintln!("no Vk — skipping");
            return;
        };
        let mut store = DrawableStore::new();
        let mut engine = RenderEngine::new(&platform).expect("engine");

        // Allocate + PutImage a distinct pattern (per-pixel unique).
        let storage = platform.allocate_drawable_storage(8, 4, 32).expect("alloc");
        let dst = store
            .allocate(
                0x1,
                super::super::store::DrawableKind::Pixmap,
                32,
                false,
                storage,
            )
            .expect("alloc");
        let mut src_bytes = vec![0u8; 8 * 4 * 4];
        for y in 0u8..4 {
            for x in 0u8..8 {
                let off = (usize::from(y) * 8 + usize::from(x)) * 4;
                src_bytes[off] = x * 0x20; // B
                src_bytes[off + 1] = y * 0x40; // G
                src_bytes[off + 2] = (x + y) * 0x10; // R
                src_bytes[off + 3] = 0xFF; // A (opaque)
            }
        }
        engine
            .put_image(
                &mut store,
                &mut platform,
                dst,
                vk::Offset2D::default(),
                vk::Extent2D {
                    width: 8,
                    height: 4,
                },
                &src_bytes,
                32,
            )
            .expect("put_image");

        engine
            .render_composite(
                &mut store,
                &mut platform,
                3, // Over
                ResolvedSource::Drawable(dst),
                ResolvedSource::None,
                dst,
                &[full_rect(8, 4)],
                None,
                Repeat::None,
                Repeat::None,
                None,
                None,
                false,
                0,
                0,
                0,
            )
            .expect("render_composite");
        // The real contract: Over(self, NoMask, self) on opaque alpha must
        // be bit-identical to self — i.e. the engine must NOT let the GPU
        // sample dst while writing it (read-write hazard → corruption). We
        // assert that on the observable output below rather than on the
        // internal `used_src_alias_scratch` routing flag (an implementation
        // detail of how the hazard is avoided).
        let after = engine
            .get_image(
                &mut store,
                &mut platform,
                dst,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 8,
                        height: 4,
                    },
                },
                32,
            )
            .expect("get_image");
        assert_eq!(
            after, src_bytes,
            "Over(self, NoMask, self) must equal self bit-identical",
        );

        engine.drain_all(&mut platform);
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn render_fill_rectangles_src_clears_to_color() {
        // render_fill_rectangles(op=Src, premul colour) — every
        // pixel in the rect must equal the premul colour.
        let Some(mut platform) = live_platform() else {
            eprintln!("no Vk — skipping");
            return;
        };
        let mut store = DrawableStore::new();
        let mut engine = RenderEngine::new(&platform).expect("engine");

        let dst = alloc_filled_pixmap(
            &mut platform,
            &mut store,
            &mut engine,
            0x1,
            4,
            4,
            [0.0, 0.0, 0.0, 1.0],
        );
        let stats = engine
            .render_fill_rectangles(
                &mut store,
                &mut platform,
                1,                    // Src
                [1.0, 0.0, 0.0, 1.0], // RGBA: opaque red premul
                dst,
                &[full_rect(4, 4)],
                None,
            )
            .expect("render_fill_rectangles");
        assert_eq!(stats.recorded_draws, 1);
        let out = engine
            .get_image(
                &mut store,
                &mut platform,
                dst,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 4,
                        height: 4,
                    },
                },
                32,
            )
            .expect("get_image");
        // BGRA: B=0, G=0, R=0xFF, A=0xFF.
        for px in out.chunks_exact(4) {
            assert_eq!(&px[..4], &[0x00, 0x00, 0xFF, 0xFF]);
        }
        engine.drain_all(&mut platform);
    }

    // ── Stage 3e.2 decoder + degenerate-trap unit tests ─────────

    /// Per plan §3e: round-trip a known wire bytestream through
    /// the trapezoid decoder. Verifies field offsets + 16.16
    /// fixed-point interpretation. Uses the same shape as v1's
    /// `try_vk_render_trapezoids_path` (kms/backend.rs:4286)
    /// since v2's `render_trapezoids` mirrors that decoder.
    #[test]
    fn trapezoid_decoder_x11_wire_layout() {
        // Build a single trapezoid wire record: 10 i32 fields, 40
        // bytes. Field order: top, bottom, left_p1.x, left_p1.y,
        // left_p2.x, left_p2.y, right_p1.x, right_p1.y,
        // right_p2.x, right_p2.y. All values are 16.16 fixed-point.
        let mut wire: Vec<u8> = Vec::with_capacity(40);
        let fields: [i32; 10] = [
            0,        // top = 0.0
            10 << 16, // bottom = 10.0
            2 << 16,  // left_p1.x = 2.0
            0,        // left_p1.y = 0.0
            2 << 16,  // left_p2.x = 2.0
            10 << 16, // left_p2.y = 10.0
            8 << 16,  // right_p1.x = 8.0
            0,        // right_p1.y = 0.0
            8 << 16,  // right_p2.x = 8.0
            10 << 16, // right_p2.y = 10.0
        ];
        for v in fields {
            wire.extend_from_slice(&v.to_le_bytes());
        }

        // Decode mirroring the backend's `render_trapezoids` body.
        let chunk: &[u8] = &wire;
        let read_i32 = |o: usize| -> i32 {
            i32::from_le_bytes([chunk[o], chunk[o + 1], chunk[o + 2], chunk[o + 3]])
        };
        let trap = crate::kms::vk::ops::traps::Trapezoid {
            top: read_i32(0),
            bottom: read_i32(4),
            left_p1: (read_i32(8), read_i32(12)),
            left_p2: (read_i32(16), read_i32(20)),
            right_p1: (read_i32(24), read_i32(28)),
            right_p2: (read_i32(32), read_i32(36)),
        };
        assert_eq!(trap.top, 0);
        assert_eq!(trap.bottom, 10 << 16);
        assert_eq!(trap.left_p1, (2 << 16, 0));
        assert_eq!(trap.left_p2, (2 << 16, 10 << 16));
        assert_eq!(trap.right_p1, (8 << 16, 0));
        assert_eq!(trap.right_p2, (8 << 16, 10 << 16));

        // bbox: x ∈ [2, 8], y ∈ [0, 10]; integer = (2, 0, 8, 10).
        let bbox = crate::kms::vk::ops::traps::trapezoid_bbox(&[trap])
            .expect("bbox for non-degenerate trap");
        assert_eq!(bbox, (2, 0, 8, 10));
    }

    /// Per plan §3e: each Triangle's three vertices round-trip
    /// through the wire decoder, and the bbox helper hits each
    /// vertex (so a degenerate triangle — three colinear points —
    /// still produces a finite bbox if the points span pixels).
    /// Mirrors v1's `try_vk_render_triangles_path` decoder shape.
    #[test]
    fn triangle_to_trap_degenerate() {
        let tri = crate::kms::vk::ops::traps::Triangle {
            p1: (0, 0),
            p2: (4 << 16, 0),
            p3: (2 << 16, 8 << 16),
        };
        let inst = tri.to_instance_data();
        assert!((inst.p1[0] - 0.0).abs() < 1e-6);
        assert!((inst.p2[0] - 4.0).abs() < 1e-6);
        assert!((inst.p3[1] - 8.0).abs() < 1e-6);
        let bbox = crate::kms::vk::ops::traps::triangle_bbox(&[tri])
            .expect("bbox for non-degenerate triangle");
        assert_eq!(bbox, (0, 0, 4, 8));

        // Degenerate (three colinear points) — bbox helper still
        // returns Some(extents) because the points span the axes.
        // What v1 + v2 do with such an input is: GPU pipeline draws
        // a zero-area triangle (no pixels covered), CB safely
        // completes. The plan's "degenerate trap" phrasing refers
        // to the encoding (trap with one zero-length edge), not a
        // helper output — the test confirms the trivial bbox path
        // doesn't choke on it.
        let colinear = crate::kms::vk::ops::traps::Triangle {
            p1: (0, 0),
            p2: (4 << 16, 0),
            p3: (8 << 16, 0),
        };
        assert!(crate::kms::vk::ops::traps::triangle_bbox(&[colinear]).is_none());
    }

    /// Stage 3f.15: `fill_rect_batch` records N rects into ONE CB +
    /// ONE submit + ONE `SubmittedOp`. Drives 3 disjoint rects on a
    /// 16×4 BGRA8 dst pre-cleared to blue, fills them red, and
    /// asserts (a) the dst observes red inside each rect and blue
    /// outside, and (b) `inner.submitted` grew by exactly 1 across the
    /// two fill calls (blue-prefill + red-batch) after the frame closes.
    ///
    /// Phase B.3 update: fill_rect / fill_rect_batch now append to the
    /// open frame instead of submitting immediately. The count assertion
    /// is now gated on closing the frame first (via
    /// `close_open_frame_for_timeout_for_tests`), then asserting submitted
    /// grew by the expected count. The pixel-correctness assertions are
    /// unchanged — `get_image` closes any open frame internally (via
    /// `close_open_frame(SyncWait)`) so they still observe all fills.
    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn fill_rect_batch_one_submit_for_n_rects() {
        let Some(mut platform) = live_platform() else {
            eprintln!("no VkContext available — skipping");
            return;
        };
        let mut store = DrawableStore::new();
        let mut engine = RenderEngine::new(&platform).expect("engine");

        let storage = platform
            .allocate_drawable_storage(16, 4, 32)
            .expect("alloc");
        let id = store
            .allocate(
                0x1,
                super::super::store::DrawableKind::Pixmap,
                32,
                false,
                storage,
            )
            .unwrap();

        // Pre-fill the dst with blue so we can see the batch-painted
        // rects against a known background. Phase B.3: this now appends
        // to the open frame instead of submitting a per-op CB.
        let blue = decode_x11_pixel_bgra(0xFF_00_00_FF);
        engine
            .fill_rect(
                &mut store,
                &mut platform,
                id,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 16,
                        height: 4,
                    },
                },
                blue,
            )
            .expect("blue prefill");

        // Close the blue-prefill frame so the red-batch starts in a
        // fresh frame. This mirrors the production sequence where
        // fill_rect is followed by a different op that closes the frame.
        // After close + flush, the blue-prefill SubmittedOp is in submitted.
        engine
            .close_open_frame_for_timeout_for_tests(&mut store, &mut platform)
            .expect("close blue-prefill frame");
        engine
            .flush_submit_group(
                &mut store,
                &mut platform,
                super::super::submit_group::FlushReason::SyncBoundary,
            )
            .expect("setup flush");

        // Snapshot the SubmittedOp count BEFORE the red batch so we
        // can assert exactly +1 (the red frame) across the call.
        let before = engine
            .inner
            .as_ref()
            .map(|i| i.submitted.len())
            .unwrap_or(0);

        let red = decode_x11_pixel_bgra(0xFF_FF_00_00);
        let rects = [
            vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: vk::Extent2D {
                    width: 2,
                    height: 2,
                },
            },
            vk::Rect2D {
                offset: vk::Offset2D { x: 6, y: 1 },
                extent: vk::Extent2D {
                    width: 3,
                    height: 2,
                },
            },
            vk::Rect2D {
                offset: vk::Offset2D { x: 13, y: 2 },
                extent: vk::Extent2D {
                    width: 3,
                    height: 2,
                },
            },
        ];
        engine
            .fill_rect_batch(&mut store, &mut platform, id, red, &rects)
            .expect("fill_rect_batch");

        // Phase B.3: close the open frame (red batch) before asserting
        // the SubmittedOp count — the op is now frame-resident until close.
        engine
            .close_open_frame_for_timeout_for_tests(&mut store, &mut platform)
            .expect("close red-batch frame");
        engine
            .flush_submit_group(
                &mut store,
                &mut platform,
                super::super::submit_group::FlushReason::SyncBoundary,
            )
            .expect("flush before count assertion");

        let after = engine
            .inner
            .as_ref()
            .map(|i| i.submitted.len())
            .unwrap_or(0);
        assert_eq!(
            after,
            before + 1,
            "fill_rect_batch (red rects) must produce exactly ONE SubmittedOp \
             regardless of rect count — N4 invariant (before={before}, after={after})"
        );

        let out = engine
            .get_image(
                &mut store,
                &mut platform,
                id,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 16,
                        height: 4,
                    },
                },
                32,
            )
            .expect("get_image");

        // Helper: does (x, y) fall inside any of the painted rects?
        let in_rect = |x: i32, y: i32| -> bool {
            rects.iter().any(|r| {
                x >= r.offset.x
                    && y >= r.offset.y
                    && x < r.offset.x + r.extent.width as i32
                    && y < r.offset.y + r.extent.height as i32
            })
        };
        for y in 0..4 {
            for x in 0..16 {
                let off = (y * 16 + x) as usize * 4;
                let px = &out[off..off + 4];
                if in_rect(x, y) {
                    assert_eq!(px[2], 0xFF, "rect pixel ({x},{y}) R should be 0xFF (red)");
                    assert_eq!(px[0], 0x00, "rect pixel ({x},{y}) B should be 0x00");
                } else {
                    assert_eq!(
                        px[0], 0xFF,
                        "background pixel ({x},{y}) B should be 0xFF (blue)"
                    );
                    assert_eq!(px[2], 0x00, "background pixel ({x},{y}) R should be 0x00");
                }
            }
        }

        engine.drain_all(&mut platform);
    }

    /// X11 Render PictFormat fix — resolver-level oracle.
    ///
    /// Per the X11 Render spec, a Picture wrapping a depth-24
    /// drawable has `PictFormat.alpha_mask = 0`; samples must
    /// return α = 1.0 regardless of the storage's padding byte.
    /// `resolve_force_opaque` is the single point where v2's
    /// `render_composite` and `render_traps_or_tris` decide
    /// whether to set the shader-side force-opaque bit on the
    /// src/mask picture.
    ///
    /// This test is the logic-only gate: a depth-24 Drawable
    /// must resolve to `true`; depth-32 to `false`. Solid and
    /// Gradient sources carry α intrinsically (LUT-baked or
    /// caller-supplied), so they're always `false`. `None` is
    /// the synthetic white-mask path — `α = 1.0` already by
    /// construction, so no override needed.
    #[test]
    fn render_composite_resolve_force_opaque_oracle() {
        let mut store = DrawableStore::new();
        let storage32 = super::super::store::Storage::for_tests_null(
            vk::Extent2D {
                width: 4,
                height: 4,
            },
            vk::Format::B8G8R8A8_UNORM,
        );
        let id32 = store
            .allocate(
                0xA001,
                super::super::store::DrawableKind::Pixmap,
                32,
                false,
                storage32,
            )
            .unwrap();
        let storage24 = super::super::store::Storage::for_tests_null(
            vk::Extent2D {
                width: 4,
                height: 4,
            },
            vk::Format::B8G8R8A8_UNORM,
        );
        let id24 = store
            .allocate(
                0xA002,
                super::super::store::DrawableKind::Pixmap,
                24,
                false,
                storage24,
            )
            .unwrap();

        // depth-32 Drawable: storage's α byte is client-meaningful,
        // do not force.
        assert!(!resolve_force_opaque(
            &store,
            &ResolvedSource::Drawable(id32)
        ));
        // depth-24 Drawable: storage's α byte is server-owned
        // padding, force α = 1.0.
        assert!(resolve_force_opaque(
            &store,
            &ResolvedSource::Drawable(id24)
        ));

        // Solid: α is caller-supplied premul. Gradient: α is
        // LUT-baked. None: white-mask scratch is initialised to
        // α = 1.0 at engine init. All three pass through.
        assert!(!resolve_force_opaque(
            &store,
            &ResolvedSource::Solid([1.0, 0.0, 0.0, 1.0]),
        ));
        assert!(!resolve_force_opaque(
            &store,
            &ResolvedSource::Gradient(0x1234)
        ));
        assert!(!resolve_force_opaque(&store, &ResolvedSource::None));

        // depth-1 (bitmap mask) and depth-8 (a8 alpha picture)
        // both have meaningful α in their PictFormat — α carries
        // the bitmap value / coverage. Forcing α = 1.0 on those
        // would turn coverage masks into solid blocks, so the
        // resolver explicitly excludes them. Only depth-24 (the
        // x8r8g8b8 / r8g8b8 case where storage's α byte is
        // server-owned padding) gets the override.
        let storage1 = super::super::store::Storage::for_tests_null(
            vk::Extent2D {
                width: 4,
                height: 4,
            },
            vk::Format::B8G8R8A8_UNORM,
        );
        let id1 = store
            .allocate(
                0xA003,
                super::super::store::DrawableKind::Pixmap,
                1,
                false,
                storage1,
            )
            .unwrap();
        assert!(!resolve_force_opaque(
            &store,
            &ResolvedSource::Drawable(id1)
        ));
        let storage8 = super::super::store::Storage::for_tests_null(
            vk::Extent2D {
                width: 4,
                height: 4,
            },
            vk::Format::R8_UNORM,
        );
        let id8 = store
            .allocate(
                0xA004,
                super::super::store::DrawableKind::Pixmap,
                8,
                false,
                storage8,
            )
            .unwrap();
        assert!(!resolve_force_opaque(
            &store,
            &ResolvedSource::Drawable(id8)
        ));
    }

    /// Audit #4 (2026-05-19) — `pict_format` overrides the depth
    /// heuristic for `Drawable` sources. A picture wrapping a
    /// depth-32 storage with `RENDER_FMT_XRGB32` declares
    /// `alpha_mask=0` — the storage's α byte is padding, not
    /// client-meaningful. Engine must force α=1 even though
    /// `d.depth == 32`. Pre-fix `resolve_force_opaque` ignored
    /// pict_format → depth-32 storages with xRGB32 sampled as
    /// transparent black against the wallpaper.
    #[test]
    fn render_composite_resolve_force_opaque_honors_xrgb32_pict_format() {
        use yserver_protocol::x11::{RENDER_FMT_ARGB32, RENDER_FMT_RGB24, RENDER_FMT_XRGB32};

        let mut store = DrawableStore::new();
        // Depth-32 storage (would normally sample with real α).
        let storage32 = super::super::store::Storage::for_tests_null(
            vk::Extent2D {
                width: 4,
                height: 4,
            },
            vk::Format::B8G8R8A8_UNORM,
        );
        let id32 = store
            .allocate(
                0xA101,
                super::super::store::DrawableKind::Pixmap,
                32,
                false,
                storage32,
            )
            .unwrap();
        // Depth-24 storage (α is padding regardless of pict_format).
        let storage24 = super::super::store::Storage::for_tests_null(
            vk::Extent2D {
                width: 4,
                height: 4,
            },
            vk::Format::B8G8R8A8_UNORM,
        );
        let id24 = store
            .allocate(
                0xA102,
                super::super::store::DrawableKind::Pixmap,
                24,
                false,
                storage24,
            )
            .unwrap();
        let src32 = ResolvedSource::Drawable(id32);
        let src24 = ResolvedSource::Drawable(id24);

        // pict_format=0 (no picture context) → fall back to depth
        // heuristic (the engine-internal callers that synthesize
        // sources pass 0 here).
        assert!(!resolve_force_opaque_pict_format(&store, &src32, 0));
        assert!(resolve_force_opaque_pict_format(&store, &src24, 0));

        // pict_format=RENDER_FMT_XRGB32 on depth-32 storage → force
        // opaque (the audit-#4 case). Pre-fix would have returned
        // false because depth==32.
        assert!(resolve_force_opaque_pict_format(
            &store,
            &src32,
            RENDER_FMT_XRGB32,
        ));
        // pict_format=RENDER_FMT_ARGB32 on depth-32 storage → use
        // storage α (current behavior preserved).
        assert!(!resolve_force_opaque_pict_format(
            &store,
            &src32,
            RENDER_FMT_ARGB32,
        ));
        // pict_format=RENDER_FMT_RGB24 on depth-24 storage → force
        // opaque (consistent with the legacy depth-24 path).
        assert!(resolve_force_opaque_pict_format(
            &store,
            &src24,
            RENDER_FMT_RGB24,
        ));
    }

    /// Audit #4 (2026-05-19) — destination `pict_format` overrides
    /// the depth-32 storage heuristic. A Picture wrapping a
    /// depth-32 storage with `RENDER_FMT_XRGB32` declares
    /// `alpha_mask = 0` — the dst storage has no client-meaningful
    /// alpha channel, padding bytes only. The engine must drive
    /// the pipeline + readback selection as "no alpha target,"
    /// matching the depth-24 case, otherwise post-composite reads
    /// of those padding bytes leak through to subsequent samples
    /// as partial transparency. Pre-fix `dst_has_alpha = depth == 32`
    /// unconditionally → xRGB32 destination treated as ARGB.
    #[test]
    fn render_composite_dst_has_alpha_honors_xrgb32_pict_format() {
        use yserver_protocol::x11::{RENDER_FMT_ARGB32, RENDER_FMT_RGB24, RENDER_FMT_XRGB32};

        // pict_format=0 (no picture context — engine-internal callers
        // synthesizing draws) → depth heuristic.
        assert!(!dst_has_alpha_for_pict_format(
            vk::Format::B8G8R8A8_UNORM,
            24,
            0,
        ));
        assert!(dst_has_alpha_for_pict_format(
            vk::Format::B8G8R8A8_UNORM,
            32,
            0,
        ));

        // XRGB32 on depth-32 storage → no alpha (audit #4 case).
        assert!(!dst_has_alpha_for_pict_format(
            vk::Format::B8G8R8A8_UNORM,
            32,
            RENDER_FMT_XRGB32,
        ));
        // ARGB32 on depth-32 storage → use storage alpha
        // (current behavior preserved).
        assert!(dst_has_alpha_for_pict_format(
            vk::Format::B8G8R8A8_UNORM,
            32,
            RENDER_FMT_ARGB32,
        ));
        // RGB24 on depth-24 storage → no alpha (consistent with
        // legacy depth-24 path).
        assert!(!dst_has_alpha_for_pict_format(
            vk::Format::B8G8R8A8_UNORM,
            24,
            RENDER_FMT_RGB24,
        ));
        // R8 storage (A8 mask destination) is alpha-only regardless
        // of pict_format — A8 destinations DO have alpha bytes.
        assert!(dst_has_alpha_for_pict_format(vk::Format::R8_UNORM, 8, 0));
    }

    /// Audit #4 — `swizzle_class_for` must pick `BgraNoAlpha`
    /// (force α=ONE swizzle on the sample view) whenever the
    /// picture's PictFormat declares `alpha_mask=0`, not just
    /// when `depth == 24`. Pre-fix, depth-32 storages always got
    /// `RgbaIdent` (pass-through), so an xRGB32 picture wrapping
    /// a depth-32 storage with α=0 padding bytes sampled as
    /// transparent.
    #[test]
    fn render_composite_swizzle_class_for_pict_format_xrgb32_is_no_alpha() {
        use yserver_protocol::x11::{RENDER_FMT_ARGB32, RENDER_FMT_RGB24, RENDER_FMT_XRGB32};

        // pict_format=0 falls back to depth heuristic.
        assert_eq!(
            swizzle_class_for_pict_format(vk::Format::B8G8R8A8_UNORM, 24, 0),
            SwizzleClass::BgraNoAlpha,
        );
        assert_eq!(
            swizzle_class_for_pict_format(vk::Format::B8G8R8A8_UNORM, 32, 0),
            SwizzleClass::RgbaIdent,
        );

        // xRGB32 on depth-32 storage → BgraNoAlpha (force α=ONE).
        assert_eq!(
            swizzle_class_for_pict_format(vk::Format::B8G8R8A8_UNORM, 32, RENDER_FMT_XRGB32,),
            SwizzleClass::BgraNoAlpha,
        );
        // ARGB32 on depth-32 storage → RgbaIdent (use storage α).
        assert_eq!(
            swizzle_class_for_pict_format(vk::Format::B8G8R8A8_UNORM, 32, RENDER_FMT_ARGB32,),
            SwizzleClass::RgbaIdent,
        );
        // RGB24 on depth-24 storage → BgraNoAlpha (already true via
        // depth, preserved when pict_format aligns).
        assert_eq!(
            swizzle_class_for_pict_format(vk::Format::B8G8R8A8_UNORM, 24, RENDER_FMT_RGB24,),
            SwizzleClass::BgraNoAlpha,
        );
        // R8 storage (A8 mask) is alpha-only regardless of pict_format.
        assert_eq!(
            swizzle_class_for_pict_format(vk::Format::R8_UNORM, 8, 0),
            SwizzleClass::AlphaOnlyR8,
        );
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn engine_exposes_descriptor_pool_ring_lifetime_counters() {
        let b = match super::super::backend::KmsBackendV2::for_tests_with_vk() {
            Ok(b) => b,
            Err(e) => {
                eprintln!("skipping: no Vk: {e}");
                return;
            }
        };
        assert_eq!(b.engine.descriptor_pool_creates_lifetime(), 0);
        assert_eq!(b.engine.descriptor_pool_resets_lifetime(), 0);
    }

    // ── Task 3 Phase A regression tests ─────────────────────────

    // ── Task 4 Phase A regression tests ─────────────────────────

    // ── Task 7 Phase A regression tests ─────────────────────────

    // ────────────────────────────────────────────────────────────
    // Phase B.2 Task 4: overlay-as-source-of-truth read accessor
    // + commit_close_success overlay → storage write-back.
    // ────────────────────────────────────────────────────────────

    /// `RenderEngineInner::current_layout_for_drawable` returns the
    /// overlay's `current_in_frame_layout` once the drawable has been
    /// first-touched and updated in-frame; falls back to
    /// `storage.current_layout` when no frame is open / drawable
    /// untouched.
    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn current_layout_for_drawable_reads_overlay_when_first_touched() {
        let Some(mut platform) = live_platform() else {
            eprintln!("no VkContext available — skipping");
            return;
        };
        let mut store = DrawableStore::new();
        let mut engine = RenderEngine::new(&platform).expect("engine");
        let id = engine
            .create_pixmap(&mut store, &mut platform, 0x4f00_0001, 8, 8, 32)
            .expect("create");

        // Storage seeded with UNDEFINED by `allocate_drawable_storage`.
        // Pre-condition (no frame open): wrapper returns the storage
        // value directly.
        {
            let inner = engine.inner.as_ref().expect("inner");
            assert_eq!(
                inner.current_layout_for_drawable(&store, id),
                vk::ImageLayout::UNDEFINED,
                "no frame open + UNDEFINED storage → wrapper returns UNDEFINED",
            );
        }

        // Open a frame and first-touch the drawable, then update its
        // in-frame layout to COLOR_ATTACHMENT_OPTIMAL — same shape a
        // ported `render_composite` will use at op-append time.
        let ticket = platform
            .submit_group_ticket_or_open()
            .expect("submit_group_ticket_or_open");
        engine.open_frame_for_paint_for_tests(ticket);
        {
            let inner = engine.inner.as_mut().expect("inner");
            let open = inner.frame_builder.open.as_mut().expect("open");
            open.layouts
                .first_touch_drawable(id, vk::ImageLayout::UNDEFINED);
            open.layouts
                .set_drawable_in_frame(id, vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);
        }

        // Wrapper now consults the overlay — must see the in-frame
        // value, NOT the (still UNDEFINED) storage value.
        {
            let inner = engine.inner.as_ref().expect("inner");
            assert_eq!(
                inner.current_layout_for_drawable(&store, id),
                vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                "frame open + drawable first-touched → wrapper returns overlay's \
                 current_in_frame_layout (overlay-as-source-of-truth invariant)",
            );
        }
        // Storage unchanged during recording.
        assert_eq!(
            store.get(id).expect("drawable").storage.current_layout,
            vk::ImageLayout::UNDEFINED,
            "storage NOT mutated during recording (B.2 invariant)",
        );

        // Untouched second drawable in the same open frame falls
        // through to its storage layout.
        let id2 = engine
            .create_pixmap(&mut store, &mut platform, 0x4f00_0002, 8, 8, 32)
            .expect("create #2");
        {
            let inner = engine.inner.as_ref().expect("inner");
            assert_eq!(
                inner.current_layout_for_drawable(&store, id2),
                vk::ImageLayout::UNDEFINED,
                "untouched drawable in open frame → wrapper falls back to storage",
            );
        }

        // Close the frame cleanly so drop-time invariants hold.
        engine
            .close_open_frame_for_timeout_for_tests(&mut store, &mut platform)
            .expect("close");
        engine.drain_all(&mut platform);
    }

    /// `commit_close_success` writes each touched drawable's
    /// `current_in_frame_layout` back to `storage.current_layout`
    /// (USER-codex U-R6.F1 — LOAD-BEARING).
    ///
    /// Without this commit, a B.2 frame ports that route layout
    /// transitions exclusively through the overlay would leave
    /// `Drawable::storage.current_layout` stale after submit — the
    /// next op (legacy or ported) would emit a barrier from the wrong
    /// `old_layout`, corrupting / device-losing on the next render.
    ///
    /// This unit test substitutes for the integration test sketched
    /// in the plan (Step 6) which depends on Task 5's
    /// `set_frame_builder_render_composite_enabled_for_tests` gate +
    /// Task 8's `render_composite_via_frame_builder` body — neither
    /// has landed yet. The substitute exercises the commit path
    /// directly: seed the overlay manually, drive the close, assert
    /// storage caught up.
    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn commit_close_success_writes_overlay_into_storage() {
        let Some(mut platform) = live_platform() else {
            eprintln!("no VkContext available — skipping");
            return;
        };
        let mut store = DrawableStore::new();
        let mut engine = RenderEngine::new(&platform).expect("engine");
        let id = engine
            .create_pixmap(&mut store, &mut platform, 0x4f01_0001, 8, 8, 32)
            .expect("create");
        // Storage starts UNDEFINED.
        assert_eq!(
            store.get(id).expect("drawable").storage.current_layout,
            vk::ImageLayout::UNDEFINED,
        );

        // Open a frame, seed the overlay as a ported op would: first
        // touch records the pre-frame layout, then `set_*_in_frame`
        // captures the post-op exit layout. (For `render_composite`,
        // that's SHADER_READ_ONLY_OPTIMAL per Pitfall 6.)
        let ticket = platform
            .submit_group_ticket_or_open()
            .expect("submit_group_ticket_or_open");
        engine.open_frame_for_paint_for_tests(ticket);
        {
            let inner = engine.inner.as_mut().expect("inner");
            let open = inner.frame_builder.open.as_mut().expect("open");
            open.layouts
                .first_touch_drawable(id, vk::ImageLayout::UNDEFINED);
            open.layouts
                .set_drawable_in_frame(id, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
        }
        // While the frame is open, storage MUST NOT have moved.
        assert_eq!(
            store.get(id).expect("drawable").storage.current_layout,
            vk::ImageLayout::UNDEFINED,
            "storage unchanged during recording (B.2 invariant)",
        );

        // Close on success — frame has no recorded ops, so the
        // empty CB submits cleanly and `commit_close_success` runs.
        engine
            .close_open_frame_for_timeout_for_tests(&mut store, &mut platform)
            .expect("close");

        // Storage MUST have caught up to the overlay's in-frame value.
        assert_eq!(
            store.get(id).expect("drawable").storage.current_layout,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            "commit_close_success wrote overlay → storage \
             (USER-codex U-R6.F1 LOAD-BEARING invariant)",
        );
        engine.drain_all(&mut platform);
    }

    /// RENDER `Trapezoids`/`Triangles` must honour the client's
    /// `xSrc`/`ySrc` source origin when the source is a picture (e.g.
    /// GTK CSD shadow blur-ramp masks sampled at `ySrc != 0`). The
    /// trap composite hardcoded `src_x/src_y = 0`, collapsing the ramp
    /// to a solid slab → opaque black bar below tooltips. This locks
    /// the origin convention the emit now applies.
    #[test]
    fn trap_composite_src_origin_honours_xsrc_ysrc() {
        // full-dst branch (op=Src builds the A8 mask): the coverage
        // mask carries the bbox offset, so the source aligns directly
        // at the shifted client origin (ySrc=25 in the tooltip trace).
        assert_eq!(trap_composite_src_origin_axis(25, 18, true), 25);
        assert_eq!(trap_composite_src_origin_axis(0, 18, true), 0);
        // non-full-dst branch: composite renders at the bbox origin, so
        // the source adds it back (Xorg miTrapezoids: src at xSrc+dst).
        assert_eq!(trap_composite_src_origin_axis(25, 18, false), 43);
        // shifted-negative base (redirect/x_off pushed the origin left)
        // composes linearly with the bbox add.
        assert_eq!(trap_composite_src_origin_axis(-4, 10, false), 6);
        // The pre-fix behaviour (origin always 0) is now only correct
        // for a zero client origin on the full-dst path — proving the
        // hardcoded 0 was wrong for every nonzero xSrc/ySrc.
        assert_ne!(trap_composite_src_origin_axis(25, 18, true), 0);
    }
}
