//! Per-bo state machine + scanout-bo allocation (sub-phase 4.1.2).
//!
//! Spec: docs/superpowers/specs/2026-05-07-phase4-1-vulkan-compositor-design.md
//! §"Per-buffer release fence" — table of transitions and fence-handle
//! ownership rules.
//!
//! ## Allocation direction
//!
//! **GBM-first, Vulkan-fallback.** The preferred path allocates the
//! scanout BO via `gbm_bo_create_with_modifiers(RENDERING|SCANOUT)`
//! and imports the resulting dma-buf into Vulkan as the compose
//! render target (`VK_EXT_image_drm_format_modifier` +
//! `VkImportMemoryFdInfoKHR`). This is the ecosystem-standard path
//! (Xorg modesetting DDX, mutter, GNOME) and the only one that
//! produces a display-correct tiled scanout buffer on NVIDIA
//! proprietary — Vulkan-allocated block-linear images lack the
//! display-engine layout the driver applies through GBM, so all
//! gob-height modifier variants garble on Pascal (HW-confirmed on
//! GTX 1050). See
//! docs/superpowers/specs/2026-07-20-nvidia-gbm-scanout-allocation.md.
//!
//! Fallback: allocate the `VkImage` first, export via
//! `vkGetMemoryFdKHR`, import into DRM via `PRIME_FD_TO_HANDLE`.
//! Kept for the Venus (virtio-gpu blob) path where the GBM-import
//! direction aborts the driver, and for drivers/planes with no
//! Vulkan-importable modifier on offer. Both paths hand the same
//! GEM handle to `AddFB2WithModifiers`.
//!
//! ### What NVIDIA actually ends up with
//!
//! **Block-linear tiled, on every NVIDIA card measured — one path, not
//! three.** NVIDIA's GBM rejects `gbm_bo_create_with_modifiers` for
//! `DRM_FORMAT_MOD_LINEAR` with `EINVAL` on a `RENDERING|SCANOUT` BO, so
//! the GBM-LINEAR plan always fails and the first tiled variant wins.
//! HW-confirmed across generation, driver and pitch alignment:
//!
//! - GTX 1050 (Pascal, 2560x1440, `0x3000000004fe015`) — 2026-07-30,
//!   with the GBM-LINEAR `EINVAL` logged explicitly.
//! - GTX 1060 (Pascal, 3440x1440 ultrawide, `0x3000000004fe015`,
//!   pitch 13760) — 2026-07-26, 91-second session, no device-lost.
//! - RTX 3060 Ti (Ampere, driver 595.71.05, 1920x1080,
//!   `0x300000000606015`) — 2026-07-29, issue #32 telemetry.
//!
//! All three display correctly, and the 1050 has been dogfooded on this
//! path since `5fdb56eb` (2026-07-22) — `8cf45085`'s "nvidia box smooth"
//! validation on 2026-07-26 was itself run on GBM tiled scanout.
//!
//! This matters for reading the LINEAR-preference policy below:
//! [`scanout_prefers_linear`] was written for the Vulkan-alloc era
//! (2026-06-21, before GBM-first) and since 2026-07-22 it only governs
//! the Vulkan-alloc fallback plans — on NVIDIA it reorders a candidate
//! list whose LINEAR entry is guaranteed to fail. It is still correct
//! and still needed *there*, because Vulkan-allocated block-linear
//! genuinely does garble; it is simply not what NVIDIA runs today.

mod alloc_plan;
mod bo;
mod bo_pool;
mod bo_state;
mod copied;
mod dmabuf_metadata;
mod errors;
mod image_alloc;
mod modifiers;
mod probe;

use alloc_plan::*;
use bo::*;
use copied::*;
use dmabuf_metadata::*;
pub(crate) use errors::*;
use image_alloc::*;
use modifiers::*;
use probe::*;

#[cfg(test)]
mod tests;

use std::{
    io,
    os::fd::{AsFd, FromRawFd, IntoRawFd, OwnedFd},
    rc::Rc,
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

use ash::vk;
use drm::{
    Device as DrmDevice, DriverCapability,
    buffer::{DrmFourcc, DrmModifier, Handle as DrmBufferHandle, PlanarBuffer as DrmPlanarBuffer},
    control::{Device as DrmControlDevice, FbCmd2Flags, framebuffer},
};

/// Type alias for the GBM device we hold per pool. Instantiated with
/// the KMS DRM device the pool was constructed against — allocations
/// go through this driver-side allocator so the resulting BO gets the
/// scanout-correct layout the display engine expects.
type GbmDevice = gbm::Device<Rc<crate::drm::Device>>;

use super::{
    device::VkContext,
    probe_digest::ProbeDigestPipeline,
    probe_pattern::CopiedProbePatternPipeline,
    target::{
        COPIED_TRANSPORT_IMAGE_USAGE, DrawableImage, DrawableImageError, ExportableImage,
        NonExplicitLinearLayoutPolicy, allocate_copied_source_exact,
    },
};
use crate::kms::scanout_route::{RenderKmsRelationship, ScanoutRoute};

pub(crate) use super::target::CopiedSourcePlan;

#[derive(Clone, Copy)]
enum CopiedProbeReadback<'a> {
    CpuExact,
    GpuDigest(&'a ProbeDigestPipeline),
}

/// Per-bo phase. The lifecycle is roughly
/// `Free → Recording → Submitted → Pending → OnScreen → Retiring → Free`.
/// `Submitted` can also revert to `Recording` on atomic-EBUSY, or jump
/// to `Free` on modeset preempt.
#[derive(Debug, Default, PartialEq, Eq, Clone, Copy)]
pub enum BoPhase {
    /// Not in flight; GPU may write into it. No fences attached.
    #[default]
    Free,
    /// Composite CB being recorded for this bo.
    Recording,
    /// `vkQueueSubmit2` issued; we still own the `IN_FENCE_FD` until
    /// the atomic commit either accepts (kernel consumes it) or
    /// rejects (we close it).
    Submitted,
    /// `drmModeAtomicCommit` accepted; `IN_FENCE_FD` ownership
    /// transferred to kernel; we now own the `OUT_FENCE_FD` (the
    /// release fence).
    Pending,
    /// Pageflip-complete arrived. Bo is on-screen. The release fence
    /// is signal-pending (KMS signals it when the next flip retires
    /// this bo).
    OnScreen,
    /// A later flip's pageflip-complete arrived; this bo is no
    /// longer on screen. Release fence is signalled. Returns to
    /// `Free` once all GPU readers (e.g. damage-diff sources)
    /// complete.
    Retiring,
}

/// Reuse state for a binary semaphore whose submitted payload is exported as
/// `SYNC_FD`.
///
/// A successful SYNC_FD export transfers the payload out of the semaphore —
/// including the Vulkan-valid `fd = -1` already-signalled result — and leaves
/// the object reusable. If export itself fails after queue submission, the
/// binary payload may still be signalled. Signalling that object again would
/// be invalid, so it must be recreated only after the submitting queue is
/// proven quiescent.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum ExportSemaphoreReuseState {
    #[default]
    Reusable,
    NeedsRearm,
}

/// Ownership of renderer A's linear copied-transport allocation.
///
/// The transport is created on A, so its first A write acquires ownership
/// implicitly. Every submitted compose copies the local optimal target into
/// the transport and releases A -> FOREIGN; B then acquires, copies, and
/// releases B -> FOREIGN. A may not overwrite the transport again until it
/// imports B's retained completion payload and records FOREIGN -> A. The
/// optimal compose target itself never leaves A.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum CopiedSourceOwnership {
    #[default]
    RendererFirstUse,
    ForeignAwaitingSink,
    ForeignAwaitingRenderer,
    /// A released to FOREIGN but no matching synchronized B->A return can be
    /// used. The next guaranteed-full repaint may implicitly reacquire while
    /// discarding the old contents from `UNDEFINED`.
    RendererDiscard,
    /// B submitted a FOREIGN release but its completion has not yet been
    /// retained. Recovery may turn this into `RendererDiscard` only after B
    /// is proven idle.
    ForeignReturnPending,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum CopiedRenderTargetContents {
    #[default]
    Uninitialized,
    Initialized,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CopiedTransportPreparation {
    foreign_acquire: bool,
    local_old_layout: vk::ImageLayout,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum CopiedDestinationOwnership {
    /// Vulkan allocated the destination on B; first B use is local.
    #[default]
    LocalFirstUse,
    /// GBM/output allocated the destination and Vulkan imported it. First B
    /// use must acquire from FOREIGN while discarding old contents.
    ForeignImportedFirstUse,
    /// B released a GENERAL image, but KMS has not yet retired it.
    ForeignPendingKmsFromSink,
    /// A synchronous modeset installed an image that B never initialized.
    /// KMS ownership does not establish a Vulkan layout; retirement must
    /// preserve the UNDEFINED/discard provenance.
    ForeignPendingKmsUninitialized,
    /// A later flip retired the image from KMS. The display-pool phase now
    /// permits reuse, but B must first acquire it from FOREIGN.
    ForeignRetiredByKms,
    /// B released the image but KMS rejected before acquiring it. The next B
    /// use must discard from `UNDEFINED`, not invent a matching KMS release.
    ReleasedButAtomicRejected,
}

enum RetainedSyncFile {
    AlreadySignalled,
    Fd(OwnedFd),
}

/// Fence-fd handles + the current phase. Owns no DRM/Vulkan state
/// directly — callers thread the actual `VkImage` / framebuffer
/// alongside.
#[derive(Debug, Default)]
pub struct BoState {
    pub phase: BoPhase,
    /// Fence we exported from `vkGetSemaphoreFdKHR` after submit and
    /// will pass to KMS as `IN_FENCE_FD`. We own it until the kernel
    /// consumes it on atomic accept.
    pub in_fence_fd: Option<i32>,
    /// Fence the kernel allocated and handed back via `OUT_FENCE_PTR`.
    /// Signalled when the next flip retires this bo.
    pub release_fence_fd: Option<i32>,
}

/// Fences released when a bo is force-reset on modeset. Caller closes
/// each `Some(fd)` exactly once.
#[derive(Debug)]
pub struct ModesetReleased {
    pub in_fence: Option<i32>,
    pub release_fence: Option<i32>,
}

/// One scanout buffer object: a Vulkan-allocated `VkImage` exported
/// as a dma-buf and imported into the DRM device for KMS scanout.
///
/// All fields are populated after `allocate()` returns successfully.
/// Drop unwinds them in the right order (DRM framebuffer → GEM handle
/// close → VkImage → memory → semaphore → command pool).
#[allow(dead_code)] // most fields used by 4.1.2.5+ atomic-commit driver.
pub struct ScanoutBo {
    pub state: BoState,
    pub width: u32,
    pub height: u32,
    /// `true` for client-imported alien BOs (Phase 4.2.4 Flip /
    /// DirectScanout); `false` for pool-allocated server BOs. Alien
    /// BOs share the framebuffer-registration code path but skip the
    /// allocator: they're wired in by `ScanoutBoPool::register_alien`.
    pub is_alien: bool,
    /// Row pitch in bytes — what the driver chose for our
    /// `TILING_LINEAR` image. Passed to KMS as `pitch[0]` and to the
    /// blit copy as the destination row stride.
    pub pitch: u32,
    /// Compose GPU-render time (ns) measured on THIS bo's PREVIOUS
    /// compose via its timestamp pool, read at the start of the next
    /// compose (prior fence signaled → no wait) and surfaced to
    /// `tick_one_output` → `telemetry.record_gpu_render_ns`. `.take()`n
    /// each frame. `None` until the bo has composed at least twice / on
    /// devices without timestamp support.
    pub last_gpu_render_ns: Option<u64>,
    pub vk_image: vk::Image,
    pub vk_memory: vk::DeviceMemory,
    /// Color image view bound by the composite pass's
    /// `vkCmdBeginRendering` as the color attachment. Lives as long
    /// as `vk_image`. Built lazily on first use to avoid forcing
    /// every PixmanShadow-only deployment to allocate a view it
    /// never reads.
    pub vk_image_view: vk::ImageView,
    /// Long-lived binary semaphore used as `signalSemaphore` on the
    /// per-frame composite submit. Its payload is exported as a
    /// SYNC_FD after every submit and handed to KMS as `IN_FENCE_FD`.
    /// Object reused for the bo's whole lifetime; only the fd
    /// payload churns.
    pub vk_semaphore: vk::Semaphore,
    export_semaphore_reuse: ExportSemaphoreReuseState,
    /// DRM framebuffer registered against this bo's GEM handle.
    /// `Option` so Drop can take it.
    pub fb_handle: Option<framebuffer::Handle>,
    /// GEM handle from `PRIME_FD_TO_HANDLE`. Closed via `GEM_CLOSE`
    /// in Drop. `Option` so Drop can take it.
    pub gem_handle: Option<DrmBufferHandle>,
    /// Per-bo transfer resources: command pool + a single command
    /// buffer recycled across frames, a host-mapped staging buffer
    /// sized for the bo (XRGB8888 → 4 bytes × width × height), and
    /// the device memory backing it.
    pub vk_transfer: TransferResources,
    /// Shared DRM device handle (for un-registering the framebuffer
    /// + closing the GEM handle in Drop).
    drm: Rc<crate::drm::Device>,
    /// Held to keep image+memory destructors anchored to a live
    /// device. Cloned per bo from the pool's Arc so individual bos
    /// can be moved/dropped independently.
    vk: Arc<VkContext>,
    /// When `true`, `Drop` early-returns: no explicit
    /// `destroy_framebuffer`, no GEM close, no Vk teardown.
    /// Resources are then leaked until process-exit DRM-fd close +
    /// VkDevice teardown — the kernel reaps GEM/FB on device-fd close
    /// and the userspace heap goes away with the process. This is a
    /// deliberate last-resort leak path, not a normal cleanup route.
    /// Set by `disarm()` from the shutdown path when atomic
    /// `disable_output` failed for this BO's CRTC — KMS may still
    /// hold the FB, so user-side teardown would corrupt kernel state.
    ///
    /// **ONLY safe to use at final process exit.** This Drop
    /// short-circuit bypasses Vk image / memory / GEM / FB cleanup
    /// but does NOT prevent Rust from dropping other fields (like
    /// the `Arc<VkContext>`). Using disarm at runtime (hotplug,
    /// modeset recovery) could produce a zombie VkImage when the
    /// VkContext's refcount expires.
    disarmed: bool,
    /// GBM buffer object backing this bo, when the GBM-first
    /// allocation path was used. `None` for Vulkan-first (fallback)
    /// allocations. Kept alive here so the `gbm_bo` outlives the
    /// dependent GEM handle, DRM framebuffer, and imported Vulkan
    /// image / memory — declared last so Rust drops it after the
    /// explicit `Drop` impl has torn those down.
    gbm_bo: Option<gbm::BufferObject<()>>,
}

/// Per-bo transfer-side resources (command pool/buffer + staging
/// buffer).
#[allow(dead_code)] // exercised by 4.1.2.5 atomic-commit driver.
pub struct TransferResources {
    pub command_pool: vk::CommandPool,
    pub command_buffer: vk::CommandBuffer,
    pub staging_buffer: vk::Buffer,
    pub staging_memory: vk::DeviceMemory,
    pub staging_mapped: std::ptr::NonNull<u8>,
    pub staging_size: u64,
    /// 2-query TIMESTAMP pool bracketing the compose GPU work (TOP at
    /// CB start, BOTTOM before end). Read on the NEXT compose of this
    /// BO (its prior fence has signaled — no wait) to derive
    /// `gpu_render_ns`. `null` if the device has no timestamp support.
    pub timestamp_pool: vk::QueryPool,
    /// Whether a submitted compose has reset and written both queries.
    /// Reading a query that was never reset is invalid (validation:
    /// "query not reset"), so the first compose of a new pool must skip
    /// the read rather than rely on `NOT_READY`.
    pub timestamps_written: bool,
}

// Intentionally `!Send + !Sync`: the mapped staging pointer and every
// scanout resource stay on the single core/backend thread.

/// One pool per CRTC; holds N bos that rotate through the state
/// machine. Three is the documented sweet spot (design §2): one
/// scanning out, one queued, one being recorded into.
#[allow(dead_code)] // wired in via KmsBackend in a later commit.
pub struct ScanoutBoPool {
    pub bos: Vec<ScanoutBo>,
    pub width: u32,
    pub height: u32,
    /// Stable renderer and KMS endpoints this pool connects. Kept separately
    /// from the observations below because incomplete device metadata must not
    /// erase either endpoint's identity.
    pub(crate) route: ScanoutRoute,
    /// Endpoint that allocated every BO in this pool. Exact-plan pool
    /// allocation keeps this uniform across all three BOs.
    pub(crate) ownership: ScanoutOwnership,
    /// Exact allocation representation shared by every BO in the pool.
    pub(crate) allocation_plan: ScanoutAllocationPlan,
    /// Non-authoritative capability observations for both zero-copy allocation
    /// directions. Real GBM/Vulkan allocation and import remain the source of
    /// truth; these observations never filter or reorder allocation plans.
    pub(crate) metadata: DmabufScanoutMetadata,
    /// Aggregate diagnostic summary of `metadata`. Real allocation, import,
    /// rendering and atomic TEST_ONLY operations remain authoritative even
    /// when this observation is `Incompatible`.
    pub(crate) verdict: DmabufScanoutVerdict,
    /// GBM device on the pool's KMS DRM fd. Populated when
    /// `gbm_create_device` succeeds; individual BOs use this to
    /// allocate driver-side scanout-layout buffers (ecosystem-
    /// standard, and the only way tiled scanout is correct on
    /// NVIDIA — see 2026-07-20-nvidia-gbm-scanout-allocation.md).
    /// `None` degrades to the Vulkan-first legacy allocator.
    /// Declared last so it drops AFTER every BO in `bos` — GBM BOs
    /// hold their own device refcount, but the pool-level owner
    /// dying first would still be a lifetime foot-gun in future
    /// refactors.
    #[allow(dead_code)]
    gbm_device: Option<Rc<GbmDevice>>,
}

/// One output's installed presentation mechanism.
///
/// Shared scanout presents a single allocation visible to renderer and KMS.
/// Copied scanout pairs a renderer-owned source with an independent sink-local
/// destination; copy is transport and deliberately is not a third
/// [`ScanoutOwnership`].
pub(crate) enum OutputScanout {
    Shared(ScanoutBoPool),
    Copied(CopiedScanoutPool),
}

/// Exact renderer-source and sink-destination representations persisted from
/// disposable probing into live copied scanout.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CopiedScanoutPlan {
    pub(crate) source: CopiedSourcePlan,
    pub(crate) destination: ScanoutAllocationPlan,
}

/// Renderer A's local optimal compose target, its DMA-BUF transport,
/// and GPU B's imported transfer-source alias. The sink alias is declared
/// first, then the transport backing, so both aliases drop before their storage;
/// the independent local target drops last.
pub(crate) struct CopiedRenderSource {
    imported_on_sink: Option<DrawableImage>,
    transport_on_renderer: Option<ExportableImage>,
    render_target: Option<DrawableImage>,
    pub(crate) completion_semaphore: vk::Semaphore,
    completion_semaphore_reuse: ExportSemaphoreReuseState,
    pub(crate) transfer: TransferResources,
    pub(crate) last_gpu_render_ns: Option<u64>,
    render_vk: Arc<VkContext>,
    sink_vk: Arc<VkContext>,
    sink_wait_semaphore: Option<vk::Semaphore>,
    renderer_wait_semaphore: Option<vk::Semaphore>,
    renderer_return_completion: Option<RetainedSyncFile>,
    ownership: CopiedSourceOwnership,
    render_target_contents: CopiedRenderTargetContents,
    disarmed: bool,
}

/// Paired A-source/B-destination pool for copied reverse-PRIME transport.
pub(crate) struct CopiedScanoutPool {
    pub(crate) sources: Vec<CopiedRenderSource>,
    pub(crate) destinations: ScanoutBoPool,
    /// Outer semantic route: selected renderer A to KMS sink B.
    pub(crate) route: ScanoutRoute,
    /// Exact source/destination pair shared by every slot.
    #[allow(dead_code)] // persisted for route diagnostics and later replay checks.
    pub(crate) plan: CopiedScanoutPlan,
    sink_vk: Arc<VkContext>,
    destination_ownership: Vec<CopiedDestinationOwnership>,
}

/// Result of observing one advertised prerequisite for a DMA-BUF path.
///
/// `Unknown` is deliberately distinct from `Unsupported`: missing metadata or
/// a failed capability query cannot prove that the driver's real ioctls will
/// reject a buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScanoutMetadataSupport {
    Supported,
    Unsupported,
    Unknown,
}

/// Metadata for one allocation direction's explicit-modifier path.
///
/// `kms_prime` is KMS PRIME export for output-owned (GBM) allocations and KMS
/// PRIME import for renderer-owned (Vulkan) allocations. `modifiers` contains
/// the KMS-plane modifiers for which Vulkan advertised the direction's needed
/// external-memory feature. `modifier_path` combines those two observations;
/// it is diagnostic only and says nothing conclusive about linear fallbacks or
/// the success of a concrete allocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DmabufDirectionMetadata {
    pub(crate) kms_prime: ScanoutMetadataSupport,
    pub(crate) vulkan_modifiers: ScanoutMetadataSupport,
    pub(crate) modifiers: Vec<u64>,
    pub(crate) modifier_path: ScanoutMetadataSupport,
    pub(crate) linear: DmabufLinearMetadata,
}

/// How the KMS plane's metadata says a linear framebuffer would be registered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KmsLinearLayout {
    /// `IN_FORMATS` explicitly includes `DRM_FORMAT_MOD_LINEAR`.
    ExplicitModifier,
    /// No usable `IN_FORMATS` metadata was available. The allocator may still
    /// attempt traditional untagged `addfb2`, but the metadata cannot prove it.
    LegacyAddfb,
    /// `IN_FORMATS` was present and did not include linear.
    NotAdvertised,
}

/// Direction-specific evidence for a linear DMA-BUF path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DmabufLinearMetadata {
    /// Vulkan IMPORTABLE for output-owned explicit-linear GBM buffers;
    /// Vulkan EXPORTABLE with `VK_IMAGE_TILING_LINEAR` for renderer-owned
    /// buffers.
    pub(crate) vulkan: ScanoutMetadataSupport,
    pub(crate) kms_layout: KmsLinearLayout,
    /// PRIME, Vulkan, and KMS-layout evidence combined without affecting the
    /// allocator.
    pub(crate) path: ScanoutMetadataSupport,
}

/// Direction-specific metadata captured when a scanout pool is allocated.
///
/// The directions have asymmetric requirements:
/// - output-owned: KMS PRIME export plus Vulkan DMA-BUF import;
/// - renderer-owned: Vulkan DMA-BUF export plus KMS PRIME import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DmabufScanoutMetadata {
    pub(crate) vulkan_external_memory_fd: ScanoutMetadataSupport,
    pub(crate) output_owned: DmabufDirectionMetadata,
    pub(crate) renderer_owned: DmabufDirectionMetadata,
}

/// One conclusively unavailable DMA-BUF allocation direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DmabufDirectionIncompatibility {
    OutputOwnedKmsPrimeExportUnsupported,
    RendererOwnedKmsPrimeImportUnsupported,
}

/// Metadata that conclusively rules out a known-different scanout route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DmabufScanoutIncompatibility {
    VulkanExternalMemoryFdUnavailable,
    BothAllocationDirectionsUnavailable {
        output_owned: DmabufDirectionIncompatibility,
        renderer_owned: DmabufDirectionIncompatibility,
    },
}

/// Missing or inconclusive evidence that must preserve the real allocation
/// attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DmabufScanoutUncertainty {
    RenderKmsRelationshipUnknown,
    VulkanExternalMemoryFdUnknown,
    OutputOwnedGbmUnavailable,
    OutputOwnedKmsPrimeExportUnknown,
    RendererOwnedKmsPrimeImportUnknown,
    OutputOwnedLayoutMetadataIncomplete,
    RendererOwnedLayoutMetadataIncomplete,
    OutputOwnedNoAdvertisedSharedLayout,
    RendererOwnedNoAdvertisedSharedLayout,
}

/// Aggregate metadata-only route observation.
///
/// `Compatible` means either the same-device policy preserves established
/// behavior or at least one direction has the advertised prerequisites. It is
/// not proof that a concrete allocation will succeed. `Incompatible` records
/// conclusively absent advertised prerequisites, but does not suppress real
/// allocation attempts: driver capability metadata is not the runtime
/// authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DmabufScanoutVerdict {
    Compatible,
    Incompatible(DmabufScanoutIncompatibility),
    Unknown(Vec<DmabufScanoutUncertainty>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DmabufDirectionVerdict {
    Supported,
    Unsupported(DmabufDirectionIncompatibility),
    Unknown(DmabufScanoutUncertainty),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DmabufAllocationDirection {
    OutputOwned,
    RendererOwned,
}

#[derive(Clone, Copy)]
enum AllocationCleanupPolicy {
    BestEffort,
    StrictDisposable,
}

/// Staged owner used after PRIME_FD_TO_HANDLE succeeds but before a complete
/// `ScanoutBo` exists. The strict helper path can remove KMS registrations
/// before destroying backing, or retain this entire graph when either removal
/// ioctl fails. The live path keeps its established best-effort rollback.
struct PartialScanoutBoAllocation {
    vk: Arc<VkContext>,
    drm: Rc<crate::drm::Device>,
    image: vk::Image,
    memory: vk::DeviceMemory,
    image_view: Option<vk::ImageView>,
    semaphore: Option<vk::Semaphore>,
    transfer: Option<TransferResources>,
    framebuffer: Option<framebuffer::Handle>,
    gem: Option<DrmBufferHandle>,
    gbm_bo: Option<gbm::BufferObject<()>>,
}

/// Handle returned by [`ScanoutBoPool::register_alien`] — index into
/// `pool.bos` plus a generation token so a stale handle can't access
/// a re-used slot. Phase 4.2.4 design §3.3.2.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AlienBoHandle {
    pub index: u32,
}

/// Which endpoint owns the allocation backing one copy-free scanout pool.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ScanoutOwnership {
    Output,
    Renderer,
}

#[derive(Debug, thiserror::Error)]
#[error("{operation}: {result:?}")]
struct ScanoutVkOperationError {
    operation: &'static str,
    result: vk::Result,
}

#[derive(Debug, thiserror::Error)]
#[error("{context}: {source}")]
struct ScanoutIoContext {
    context: String,
    #[source]
    source: io::Error,
}

/// Failure from a disposable GPU route probe.
///
/// `quarantine` means ordinary destruction cannot safely touch the attempt's
/// backing. `abort_candidate_search` is deliberately separate: a strict KMS
/// cleanup failure is terminal even when no GPU submission is outstanding,
/// while a TEST_ONLY mode-blob failure must still release the pool's FB/GEM
/// registrations before the terminal result is returned.
#[derive(Debug, thiserror::Error)]
#[error("{source}")]
pub(crate) struct DisposableProbeError {
    #[source]
    source: io::Error,
    quarantine: bool,
    abort_candidate_search: bool,
}

/// Complete ownership of one disposable probe candidate.
///
/// The consuming split is deliberate: a known-quiescent attempt marks every
/// disposable context before ordinary child destruction, while an uncertain
/// attempt bypasses Drop for the complete graph. Keeping both branches behind
/// one production-used seam prevents a new return path from accidentally
/// running an unbounded defensive `vkDeviceWaitIdle`.
trait DisposableProbeAttempt {
    fn mark_known_quiescent(&self);

    fn release_strict_drm_resources(&mut self) -> io::Result<()>;

    fn retain_uncertain(self)
    where
        Self: Sized,
    {
        std::mem::forget(self);
    }
}

struct CopiedDisposableProbeAttempt {
    pool: CopiedScanoutPool,
    pattern: Option<CopiedProbePatternPipeline>,
    render_digest: Option<ProbeDigestPipeline>,
    sink_digest: Option<ProbeDigestPipeline>,
}

/// Fence lifetime guard for one disposable rendering probe.
///
/// Expected uncertain-submission paths explicitly abandon the raw fence while
/// the aggregate disposable attempt is quarantined. This Drop-side idle is a
/// defensive fallback for an unhandled unwind; Vulkan permits orderly object
/// destruction after `ERROR_DEVICE_LOST`, so that result also completes the
/// fallback teardown barrier.
struct ProbeFence<'a> {
    device: &'a ash::Device,
    handle: vk::Fence,
}

trait DisposableProbeFence {
    fn abandon(&mut self);
    fn destroy_idle(&mut self);
    fn wait_bounded(&mut self, timeout_ns: u64, operation: &'static str) -> io::Result<()>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PendingProbeSubmissions {
    None,
    Render,
    RenderAndSink,
}

#[derive(Clone, Copy, Debug)]
struct CopiedProbeFenceWaitDurations {
    renderer: Duration,
    sink: Duration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ScanoutAllocationPlan {
    /// Preferred path: allocate via GBM with the given DRM modifier,
    /// then import the dma-buf into Vulkan as the compose render
    /// target. Xorg modesetting DDX, mutter, GNOME all do this — and
    /// on NVIDIA it's the ONLY path that produces a display-correct
    /// tiled scanout buffer (Vulkan-alloc block-linear garbles).
    GbmModifier(u64),
    /// Fallback: allocate the `VkImage` first with the given DRM
    /// modifier, export via `vkGetMemoryFdKHR`, import into DRM.
    /// Kept for Venus (virtio-gpu blob) and for drivers/planes with
    /// no Vulkan-importable modifier on offer.
    DrmModifier(u64),
    /// LINEAR VkImage created via an EXPLICIT DRM-modifier layout
    /// (`VK_EXT_image_drm_format_modifier`) with a forced, 256-aligned
    /// `row_pitch` — for NVIDIA/Intel widths (e.g. 3440 ultrawide) whose tight
    /// LINEAR pitch the display engine rejects at atomic commit. Keeps the
    /// known-good LINEAR render path; only the stride is padded.
    PaddedExplicitLinear { row_pitch: u32 },
    /// Linear VkImage, but register the DRM framebuffer with an
    /// explicit DRM_FORMAT_MOD_LINEAR modifier.
    ExplicitLinear,
    /// Historical fallback: linear VkImage, untagged addfb2.
    LegacyLinear,
}

/// Diagnostic override of the scanout modifier policy, read once from
/// `YSERVER_SCANOUT_MODIFIER`.
///
/// [`scanout_prefers_linear`] is a per-driver policy inferred from a handful
/// of machines, and the two questions it raises can only be answered by
/// LOOKING at a display: does the GBM tiled path garble on THIS card, and
/// which of the six block-linear gob-height variants (0x…10 … 0x…15) is
/// clean? Both need the allocator pointed somewhere other than where the
/// policy points, on hardware the maintainers may not own — hence an env
/// knob rather than a patched branch per reporter.
///
/// Values (case-insensitive, `_` interchangeable with `-`):
/// - `tiled-first` — order tiled modifiers ahead of LINEAR, overriding a
///   driver that prefers LINEAR (the NVIDIA question).
/// - `linear-first` — order LINEAR first, overriding a driver that prefers
///   tiled (reproduces the RDNA4 corruption of issue #48).
/// - `0x<hex>` / `<hex>` — try exactly this modifier before all others,
///   whatever the policy says.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScanoutModifierOverride {
    TiledFirst,
    LinearFirst,
    First(u64),
}

/// Outputs of [`allocate_vk_scanout_image`] / [`allocate_gbm_scanout_image`]:
/// a bound VkImage (either allocated directly or imported from GBM),
/// its memory, the dma-buf fd (either exported from Vulkan or read
/// from the gbm_bo), the row pitch, the plane-0 byte offset, the DRM
/// modifier to use for framebuffer registration, and — for the
/// GBM-alloc path — the source gbm_bo the imported VkImage must
/// outlive.
struct VkScanoutImage {
    image: vk::Image,
    memory: vk::DeviceMemory,
    dmabuf: OwnedFd,
    pitch: u32,
    /// Plane-0 byte offset from `VkSubresourceLayout.offset` (Vulkan-alloc)
    /// or `gbm_bo_get_offset(bo, 0)` (GBM-alloc). Passed to AddFB2 —
    /// a tiled block-linear image can encode a non-zero plane offset
    /// and the display engine reads scanout from that offset.
    offset: u32,
    modifier: Option<u64>,
    /// Present only for the GBM-alloc path — the source gbm_bo whose
    /// dma-buf we imported into Vulkan. Kept alive by the caller
    /// (`ScanoutBo`) so it outlives the derived Vulkan memory / GEM
    /// handle / DRM framebuffer.
    gbm_bo: Option<gbm::BufferObject<()>>,
}

#[derive(Debug)]
enum GbmScanoutError {
    MissingExtension(&'static str),
    NotImportable(u64),
    GbmCreate(io::Error),
    MultiPlane(u32),
    UnexpectedModifier { requested: u64, actual: u64 },
    InvalidBoFd,
    FdDup(io::Error),
    NoImportableMemoryType,
    Vk(vk::Result),
}

/// Adapter that lets a freshly-imported GEM handle be passed to
/// drm 0.15's `add_planar_framebuffer` as a `PlanarBuffer`. Single
/// plane; modifier is present for explicit-modifier addfb2 paths and
/// absent only for the legacy untagged-linear fallback.
struct VkScanoutFb {
    gem_handle: DrmBufferHandle,
    width: u32,
    height: u32,
    pitch: u32,
    offset: u32,
    modifier: Option<u64>,
}
