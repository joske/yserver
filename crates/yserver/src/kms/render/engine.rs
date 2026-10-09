//! `RenderEngine`: records paint ops into the open frame and replays them
//! into Vulkan command buffers against [`DrawableStore`] storage.
//!
//! It owns the frame builder (record, coalesce, close, submit), the upload
//! arena and staging pool, scratch images and clip snapshots, the RENDER /
//! glyph / text / trapezoid pipelines and caches, and the retirement of the
//! images and resources its submissions guard. This file holds the imports
//! and every type (private fields stay visible to all children); the
//! methods live in:
//!
//! - `lifecycle`: create, poll/retire, shutdown, drain, `Drop`.
//! - `export`: GLX-TFP promotion of a pixmap onto dma-buf-exportable storage.
//! - `frame`: frame builder open/close/submit, coalescing stats,
//!   commit/rollback, present-completion batches, descriptor sets.
//! - `staging`, `scratch`: upload arena and staging buffers; scratch images
//!   and clip snapshots.
//! - `fill_copy`, `put_get`, `text`, `glyphs`, `composite`, `batch`,
//!   `gradients`, `traps`: the op-recording entry points.
//! - `emit`: close-time replay of recorded ops into the command buffer.
//! - `pixels`: wire <-> storage pixel conversion.
//! - `for_tests`: `*_for_tests` entry points; `tests`: unit tests.

#![allow(
    dead_code,
    reason = "RenderEngine primitives are consumed by Stages 2d–2f"
)]

mod batch;
mod composite;
mod emit;
mod export;
mod fill_copy;
mod for_tests;
mod frame;
mod glyphs;
mod gradients;
mod lifecycle;
mod pixels;
mod put_get;
mod scratch;
mod staging;
mod text;
mod traps;

use composite::*;
use emit::*;
pub(crate) use fill_copy::*;
use frame::*;
pub(crate) use glyphs::*;
pub(crate) use pixels::*;
#[cfg(test)]
use put_get::{clamp_put_rect, clamp_put_rect_to};
use scratch::*;
use staging::*;

// ────────────────────────────────────────────────────────────────
// Tests — logic-only (no live Vk).
//
// Vk-backed integration tests are gated by `#[ignore = "needs live
// Vulkan ICD"]` so they run only when explicitly requested. The
// Stage 2 acceptance harness (Stage 2f) drives them end-to-end.
// ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    ptr::NonNull,
    sync::Arc,
};

use ash::vk;

use super::{
    glyph_atlas::GlyphAtlas,
    glyph_pixels::{GlyphPixels, GlyphSourceFormat},
    platform::{FenceTicket, PlatformBackend, PresentCompletionSignal},
    present_completion::{PendingPresentBatch, PendingPresentEntry, PresentBatchWait},
    store::{DrawableId, DrawableStore, RetiredImage},
    target::{Dst, Src},
};
use crate::kms::{
    cpu_types::{PictTransform, Rectangle16, Repeat},
    vk::{
        device::VkContext,
        dst_readback::DstReadback,
        glyph::{AtlasEntry, GlyphKey, GlyphLayout},
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
    #[error("unsupported depth {0} for Stage 2c ops")]
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

/// Why an open `PendingRenderBatch` is being flushed. Drives the
/// `vk renderpass flush src` telemetry line so the same-target
/// render-pass coalescing phases can be sized from real workloads
/// (perf/same-target-renderpass-coalescing, 2026-06-22). The same-dst
/// and per-kind variants are the merge opportunity; diff-dst,
/// readback, and present are genuine pass boundaries.
#[derive(Debug, Clone, Copy)]
pub(crate) enum RenderFlushReason {
    KeyChangeSameDst,
    KeyChangeDiffDst,
    Fill,
    Copy,
    Glyph,
    Traps,
    PutImage,
    Readback,
    Present,
    Other,
}

/// Coalescing-relevant classification of one recorded op, decoupled
/// from the (vk-handle-heavy) `RecordedOp` so the run/session fold can
/// be unit-tested without fabricating full payloads.
#[derive(Clone, Copy, Debug)]
enum CoalesceClass {
    /// Not a render-pass-emitting op (copy / put_image / glyph upload /
    /// clip-snapshot). Breaks every run.
    NonPass,
    /// A pass-emitting op that is NOT a `RenderComposite` (glyph / fill /
    /// image-text / traps). Counts toward the all-kinds `coalescable`
    /// ceiling but breaks the composite-only Slice-1 session.
    /// `is_fill_or_logic` distinguishes the Slice-2-phase-2 session-eligible
    /// subset (fill / logic_fill) from the still-standalone kinds (glyph /
    /// image_text / traps); it does NOT affect `coalescing_counts`.
    PassNonComposite {
        dst: Option<DrawableId>,
        is_fill_or_logic: bool,
    },
    /// A `RenderComposite`. `self_samples` = src/mask view IS the dst
    /// view. `folder_clean` = can FOLD into an open same-dst session
    /// (no solid clear, no dst self-read, not self-sampling) — those
    /// pre-pass transfer ops are illegal inside an open `begin_rendering`.
    /// `dirty_clear_only` = fold-blocked SOLELY by a solid src/mask clear
    /// (no dst self-read) — a per-op solid scratch (Slice 1.5) would make
    /// it fold-clean. Mutually exclusive with `folder_clean`.
    Composite {
        dst: DrawableId,
        self_samples: bool,
        folder_clean: bool,
        dirty_clear_only: bool,
    },
}

#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
struct CoalesceCounts {
    pass_ops: u64,
    /// Same-dst-as-previous-pass-op passes a fully general session could
    /// merge (all op kinds) — the whole-plan ceiling. Partitions exactly
    /// into `mergeable` + `coalescable_dirty_clear` + `coalescable_cross_kind`.
    coalescable: u64,
    /// Subset of `coalescable` Slice 1 (composite-only, fold-clean) can
    /// actually remove today.
    mergeable: u64,
    /// Subset blocked SOLELY by a solid src/mask clear on a consecutive
    /// same-dst composite — a per-op solid scratch (Slice 1.5) converts
    /// these to `mergeable` without the cross-kind recorder split.
    coalescable_dirty_clear: u64,
    /// The remaining coalescable passes — non-composite repeats, composites
    /// separated from their same-dst predecessor by a different op kind,
    /// or dst-readback composites. These need the full cross-kind session
    /// (Slice 2: split the monolithic fill/glyph/traps recorders).
    coalescable_cross_kind: u64,
    self_sample: u64,
}

/// Slice-2: an open dynamic-rendering color pass on `dst`, held across
/// consecutive same-dst session-eligible ops in the frame-builder replay.
/// `None` (in the loop's `Option<DstPassSession>`) means no pass is open.
/// One pre-barrier (the FIRST op's `dst_old_layout` → COLOR) was emitted
/// at `open`; one post-barrier (→ SHADER_READ) is emitted at `close`.
/// Intermediate continued ops emit NO barrier.
struct DstPassSession {
    dst_id: DrawableId,
    dst_image: vk::Image,
    dst_view: vk::ImageView,
    dst_extent: vk::Extent2D,
}

/// What the replay loop must do for one op given the open-session state.
#[derive(Debug, PartialEq, Eq)]
enum SessionStep {
    /// No session open, op is eligible: open a new pass + emit draws.
    OpenNew,
    /// Session open on the SAME dst, op is eligible: emit draws only.
    Continue,
    /// Session open on a DIFFERENT dst, op is eligible: close, then open
    /// a new pass + emit draws.
    FlushThenOpenNew,
    /// Session open, op is INELIGIBLE: close, then run the op standalone.
    FlushThenStandalone,
    /// No session open, op is INELIGIBLE: run the op standalone.
    Standalone,
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

/// Phase split of one `Engine::get_image`, in nanoseconds.
///
/// Sizes the deferred-readback question: only `wait_ns` is removed outright
/// by making the readback asynchronous. `drain_ns` is submit work that still
/// happens, and `copyout_ns` still runs on the loop thread — just later. See
/// `telemetry::Bucket::get_image_wait_ns`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct GetImagePhases {
    /// flush_batch + close_frame + flush1: getting prior paint submitted so
    /// the readback copy observes it.
    pub(crate) drain_ns: u64,
    /// `ticket.wait()` on the readback fence, plus the cache invalidate.
    pub(crate) wait_ns: u64,
    /// `pack_from_storage` out of the mapped staging buffer.
    pub(crate) copyout_ns: u64,
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
    /// Phase B.3 clip — per-op masked_copy_area self-overlap scratch images.
    /// Mirrors `scratch` but for the `SampledScratchImage` type (TRANSFER_DST |
    /// SAMPLED + IDENTITY view). The close-path walk `std::mem::take`s every
    /// `RecordedMaskedCopyArea::self_overlap_scratch` into this batch's `SubmittedOp`
    /// so the scratch's `Drop` is deferred behind the frame's fence (codex
    /// round-4 finding 4 — without adoption the GPU reads freed memory).
    sampled_scratch: Vec<SampledScratchImage>,
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
    retired_resources: Vec<Box<dyn crate::kms::render::batch_resource::BatchResource>>,
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

/// Opaque id for a GC-owned clip snapshot. The `ClipSnapshot` registry that
/// consumes it arrives in Task 11; defined here now so the Phase-1 recorded-op
/// payloads (`RecordedClipSnapshotRefresh`, `MaskedCopyMask`) compile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct SnapshotId(pub(crate) u64);

/// GC-owned pinned depth-1 mask snapshot. Sampled by masked_copy_area; written
/// (re-copied from the live clip pixmap) only on the refresh path. Lifetime is
/// the GC's clip-mask install; survives the source pixmap being freed.
pub(crate) struct ClipSnapshot {
    vk: Arc<VkContext>,
    pub(crate) image: vk::Image,
    pub(crate) view: vk::ImageView, // IDENTITY R8
    memory: vk::DeviceMemory,
    pub(crate) extent: vk::Extent2D,
    pub(crate) current_layout: vk::ImageLayout,
    pub(crate) last_render_ticket: Option<FenceTicket>,
    /// content_version of the live mask at last (re)snapshot; gates refresh.
    pub(crate) snapshotted_version: u64,
    pub(crate) size_bytes: u64,
}

/// Scratch image for the masked_blit self-overlap path. Unlike
/// `ScratchImage` (transfer-only), this is `TRANSFER_DST | SAMPLED` with an
/// IDENTITY view so the masked-blit draw can sample it after the src→scratch
/// transfer breaks the read-after-write.
pub(crate) struct SampledScratchImage {
    vk: Arc<VkContext>,
    pub(crate) image: vk::Image,
    pub(crate) view: vk::ImageView,
    memory: vk::DeviceMemory,
    pub(crate) size_bytes: u64,
}

impl std::fmt::Debug for SampledScratchImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SampledScratchImage")
            .field("size_bytes", &self.size_bytes)
            .finish_non_exhaustive()
    }
}

/// Mask source for a masked CopyArea: the GC-owned snapshot in production, a
/// plain depth-1 drawable in tests. `view` MUST be an IDENTITY R8 view.
///
/// ALL fields are defined here in Task 7 (incl. `snapshot_id`) so that no later
/// task has to widen the struct and update every call site — Phase-1 test
/// callers pass `snapshot_id: None` (codex round-4 finding 8). The masked op
/// only SAMPLES the mask; (re)population is the separate `refresh_clip_snapshot`
/// path (Task 11/14), so there is NO refresh field here.
pub(crate) struct MaskedCopyMask {
    pub(crate) image: vk::Image,
    pub(crate) view: vk::ImageView,
    /// MUST be SHADER_READ when this is a freshly-refreshed snapshot; the emit
    /// transitions to SHADER_READ regardless (handles the test plain-drawable).
    pub(crate) old_layout: vk::ImageLayout,
    pub(crate) extent: vk::Extent2D,
    pub(crate) clip_origin: [i32; 2],
    /// `Some(id)` when the mask is a GC-owned snapshot (Phase 2). `None` for the
    /// Phase-1 plain-drawable test path. Drives snapshot layout/ticket
    /// first-touch tracking on sample (Task 12). When `None`, the mask
    /// layout/ticket are NOT engine-managed.
    pub(crate) snapshot_id: Option<SnapshotId>,
}

/// One-shot host-visible buffer used for `put_image` upload or
/// `get_image` readback. Destroyed on drop.
pub(crate) struct StagingBuffer {
    vk: Arc<VkContext>,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    mapped: NonNull<u8>,
    size: u64,
    /// Whether the backing memory type is `HOST_COHERENT`. When false (a
    /// `HOST_CACHED`-only readback type), CPU reads of `mapped` must be
    /// preceded by `invalidate_for_read` so they observe the GPU's writes.
    coherent: bool,
    /// True if this buffer was handed out by [`StagingPool::acquire`] (the
    /// `put_image` upload path) and should be RETURNED to the pool at retire
    /// instead of destroyed. Fresh `new*` buffers (readback, custom usage,
    /// upload arena blocks) are `false` and drop normally. Perf: avoids
    /// per-upload vkCreateBuffer/vkAllocateMemory churn, which is costly on
    /// NVIDIA.
    from_pool: bool,
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

/// EXPERIMENT (NVIDIA per-op driver cost, 2026-07-20): free-list of reusable
/// upload `StagingBuffer`s for the `put_image` path, keyed by exact byte size.
///
/// `put_image` used to `vkCreateBuffer` + `vkAllocateMemory` + map a fresh
/// staging buffer per upload (~11.5k/session under an xfce drag storm) and
/// destroy it at retire. On NVIDIA proprietary each alloc/free is a costly
/// driver ioctl (perf: yserver CPU dominated by the nvidia stack); RADV does it
/// nearly for free (hence this only helps NVIDIA). Reuse eliminates the churn:
/// the returned buffer is fully overwritten by the next `unpack_to_staging`
/// before its GPU copy, so no stale-data hazard. Buckets are exact-size (upload
/// sizes recur per widget, like the pixmap pool). Bounded by per-bucket count +
/// total bytes; over-cap returns just drop (destroy). Remove with the rest of
/// this investigation if it doesn't pan out.
#[derive(Default)]
struct StagingPool {
    buckets: std::collections::HashMap<u64, Vec<StagingBuffer>>,
    pooled_bytes: u64,
    hits: u64,
    misses: u64,
    returned: u64,
    rejected: u64,
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
    /// `KmsBackend` and at shutdown).
    submitted: VecDeque<SubmittedOp>,
    /// EXPERIMENT (#nvidia perf): reusable upload staging buffers for
    /// `put_image`, to avoid per-upload vkCreateBuffer/vkAllocateMemory churn
    /// (costly on NVIDIA). See [`StagingPool`].
    staging_pool: StagingPool,
    /// #177: idle blocks of the per-frame upload arena that glyph-run /
    /// ImageText instance data, trapezoid/triangle vertices and glyph-atlas
    /// upload staging are bump-allocated from. A frame's blocks come back
    /// only from the `pending_frames` retire walk, once the frame's fence
    /// has signalled. See [`RenderEngineInner::upload_to_frame`].
    upload_arena: crate::kms::render::upload_arena::UploadArena<StagingBuffer>,
    /// Offset alignment of glyph-atlas upload sources in the upload arena,
    /// from [`upload_copy_align`].
    upload_copy_align: u64,
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
    glyph_atlas: Option<GlyphAtlas>,
    /// Stage 3a: text pipelines (TextRunTarget descriptor bound to
    /// the atlas image view), keyed by
    /// `(op, dst_format, dst_has_alpha)` — mirroring the RENDER
    /// `Composite` pipeline cache so glyph compositing supports the
    /// standard PictOp family (notably cairo's `Add`-into-a8-mask
    /// text path). Lazy — each entry is built on first use, after
    /// the atlas is constructed. Every pipeline's descriptor set
    /// references the atlas image view permanently; the atlas
    /// image's long-lived ownership makes this safe. The core
    /// `ImageText8/16` path always uses the
    /// `(Over, B8G8R8A8, true)` entry — identical blend state to
    /// the historical singleton.
    text_pipelines: HashMap<(u8, vk::Format, bool, bool), TextPipeline>,
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
    /// Dedicated masked_blit pipeline for GPU-side clip CopyArea (depth-1
    /// mask sampled, threshold, raw copy). Built lazily alongside
    /// render_pipelines in `ensure_render_assets`.
    masked_blit: Option<crate::kms::vk::masked_blit_pipeline::MaskedBlitPipeline>,
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
    descriptor_pool_ring: crate::kms::render::descriptor_pool_ring::DescriptorPoolRing,
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
    /// Running total of `get_image` phase costs, for the backend to drain
    /// into telemetry. The phase instants were already stamped for the
    /// `GET_IMAGE_SLOW_MS` tail log; this carries them out on EVERY call so
    /// the aggregate can size how much of a readback a deferred one removes.
    ///
    /// ACCUMULATES rather than holding the last call: `get_image` has a dozen
    /// callers (clip masks, cursor, CopyArea, CopyPlane, …) and a "last one
    /// wins" slot silently misattributes one site's phases to whichever site
    /// happens to drain next, while dropping every call in between.
    get_image_phase_totals: GetImagePhases,
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
    pending_flush_outcomes: Vec<crate::kms::render::platform::FlushOutcome>,
    /// Phase B.1: in-flight frames awaiting retirement. Parallel to
    /// `submitted`; both gate on the same `FenceTicket`s when the
    /// frame builder is in play. Walked by `poll_retired` and
    /// `drain_all`.
    pending_frames:
        std::collections::VecDeque<crate::kms::render::frame_builder::FrameSubmittedRecord>,
    /// Phase B.1: telemetry events from close paths. Drained by the
    /// backend via `RenderEngine::drain_frame_close_events()`. Task 21
    /// wires the consumer side. Bounded at 1024 to prevent unbounded
    /// growth if maybe_composite stops ticking.
    pending_frame_close_events: Vec<crate::kms::render::frame_builder::FrameCloseEvent>,
    /// Phase B.1: monotonic frame sequence for telemetry attribution.
    /// Bumped on every `FrameBuilder::close_into_cb` success.
    frame_seq: u64,
    /// Phase B.1: per-frame deferred op-list recorder. `Closed` is
    /// the hot path; transitions to `OpenForPaint` only when a ported
    /// paint op (composite_glyphs in B.1) appends. Embedded so the
    /// engine can drive open/close from its existing paint entry
    /// points (Tasks 12-20 wire the transitions).
    frame_builder: crate::kms::render::frame_builder::FrameBuilder,
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
    /// Task 11: GC-owned pinned clip-mask snapshots, keyed by opaque
    /// [`SnapshotId`]. Created at clip-mask install (Task 14), populated by
    /// `refresh_clip_snapshot` (Task 13), sampled by `masked_copy_area`.
    clip_snapshots: HashMap<SnapshotId, ClipSnapshot>,
    next_snapshot_id: u64,
    /// Snapshots whose Drop is deferred behind a fence (retired this frame).
    retired_snapshots: Vec<(ClipSnapshot, Option<FenceTicket>)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameBuilderTraceFilter {
    DrawableId(u64),
    RedirectedArgbBackings,
}

/// The fields one `CompositeGlyphs` request shares across every draw
/// run it is recorded as. Carried as a struct so
/// [`RenderEngine::record_glyph_runs`] stays inside clippy's argument
/// budget, and so a future run-splitter cannot accidentally vary one
/// of them per run — all six are request-wide by specification.
struct GlyphRunCommon {
    dst_id: DrawableId,
    dst_old_layout: vk::ImageLayout,
    /// Wire PictOp, validated at append.
    op: u8,
    dst_has_alpha: bool,
    foreground_rgba: [f32; 4],
    /// The request's clip scissor list; every run scissors to all of
    /// it (one clip region per request, spec stage 2b).
    clip_scissors: Vec<vk::Rect2D>,
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

/// #133 step 3 (P4) — a RENDER source or mask that wraps a drawable,
/// with the geometry the engine needs to sample it as the PICTURE's
/// drawable rather than as raw storage.
///
/// Two cases that must not be collapsed:
///
/// - A picture on a **window**: its content sits `bw` inside the
///   storage (`compAllocPixmap`, `composite/compalloc.c:610`), so
///   sampling must start at the content origin. Xorg does the same by
///   construction — `create_bits_picture` builds the pixman image over
///   the whole backing pixmap and then adds `pict->pDrawable->x/y` to
///   the sampling offset (`fb/fbpict.c:293-296` and `:328-329`), which
///   for a redirected bordered window resolves to exactly `bw`
///   (`compAllocPixmap` sets `screen_x = drawable.x - bw`). `domain`
///   is the window's own `w x h`: the ring is not part of the window
///   drawable, so a `RepeatNone` sample outside it must not return it.
/// - A picture on a **COMPOSITE-named window pixmap**: the pixmap IS
///   the bordered image (`compAllocPixmap` allocates it at
///   `w + 2bw` x `h + 2bw`), so the border is part of that drawable on
///   purpose. `offset` is `(0, 0)` and `domain` is `None` — the whole
///   storage, ring included.
///
/// `offset == (0, 0)` with `domain == None` is [`Self::whole`], which
/// is what every pixmap and every `bw == 0` window resolves to, and is
/// exactly the pre-#133 behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SourceDrawable {
    id: DrawableId,
    offset: (i32, i32),
    domain: Option<vk::Extent2D>,
}

/// Picture source resolved against `KmsCore.pictures` by the
/// backend wrapper. The engine doesn't read protocol records
/// directly; the wrapper hands it one of these.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ResolvedSource {
    /// Picture wraps a drawable; the engine samples its storage
    /// through the geometry [`SourceDrawable`] carries.
    Drawable(SourceDrawable),
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
/// `KmsCore.glyphsets` and computes the per-glyph dst position from
/// the items stream's running pen + glyph metrics. Lifetimes:
/// `pixels` borrows the glyph's stored bytes from
/// `KmsCore.glyphsets[gs_xid].glyphs[glyph_id].pixels`; the engine
/// resolves them to the atlas's packed form and copies into a
/// per-glyph `StagingBuffer` **only on an atlas miss**, so the borrow
/// only needs to outlive the engine call itself.
pub(crate) struct CompositeGlyphInput<'a> {
    /// Glyphset xid the glyph came from (atlas key namespace).
    pub gs_xid: u32,
    /// Glyph id within the glyphset (atlas key codepoint).
    pub glyph_id: u32,
    /// Glyph width / height. 0×0 entries cache an empty entry and
    /// skip the upload (space glyphs after pen-only adjustment).
    pub w: u32,
    pub h: u32,
    /// Glyph pixels as stored in the glyphset: dense A8 (native a8),
    /// raw A1 wire, or raw ARGB32 wire (`[B, G, R, A]` CARD32s).
    ///
    /// **This is also the glyph's format tag** — the PROTOCOL fact,
    /// and the only format fact the backend is entitled to state:
    /// [`GlyphPixels::source_format`] reads it back off the variant.
    /// The mapping is total and 1:1, so a separate `source_format`
    /// field beside this one would be a second encoding of the same
    /// fact, free to disagree with the bytes it describes.
    ///
    /// A single request may switch glyphset mid-stream (the inline
    /// `count == 255` items element), and glyphsets may differ in
    /// format, so one request can interleave A8 and ARGB32 glyphs.
    /// The engine maps the format to the glyph's effective
    /// [`GlyphLayout`](crate::kms::vk::glyph::GlyphLayout) via
    /// [`RenderEngine::effective_glyph_layout`] and forms one draw
    /// run per contiguous stretch of equal layout; that mapping
    /// depends on `component_alpha_supported` and on what the atlas
    /// upload stores, which is device state the backend has no
    /// business reading.
    ///
    /// Conversion to the packed atlas form — A1 expansion, an ARGB32
    /// four-plane pack or its grayscale reduction — is deferred to
    /// the engine's atlas-miss branch
    /// ([`GlyphPixels::to_atlas_bytes`]) so a resident glyph never
    /// re-converts.
    pub pixels: GlyphPixels<'a>,
    /// Dst-space top-left corner for the glyph quad.
    pub dst_x: i32,
    pub dst_y: i32,
}

/// Telemetry surface for one [`RenderEngine::image_text`] call.
/// Caller (KmsBackend) feeds these into the telemetry sink so
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
