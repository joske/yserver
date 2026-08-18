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
        allocate_copied_source_exact,
    },
};
use crate::kms::scanout_route::{RenderKmsRelationship, ScanoutRoute};

pub(crate) use super::target::CopiedSourcePlan;

/// Exact usage of GPU B's imported alias of A's DMA-BUF transport. Keep this
/// single value shared by the capability query and actual import.
const COPIED_SINK_IMPORT_USAGE: vk::ImageUsageFlags = vk::ImageUsageFlags::TRANSFER_SRC;

#[derive(Clone, Copy)]
enum CopiedProbeReadback<'a> {
    CpuExact,
    GpuDigest(&'a ProbeDigestPipeline),
}

impl CopiedProbeReadback<'_> {
    fn destination_buffer(self, transfer: &TransferResources) -> vk::Buffer {
        match self {
            Self::CpuExact => transfer.staging_buffer,
            Self::GpuDigest(digest) => digest.input_buffer(),
        }
    }
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

impl CopiedDestinationOwnership {
    fn foreign_acquire_layouts(self) -> Option<(vk::ImageLayout, vk::ImageLayout)> {
        match self {
            Self::ForeignImportedFirstUse => {
                Some((vk::ImageLayout::UNDEFINED, vk::ImageLayout::GENERAL))
            }
            Self::ForeignRetiredByKms => Some((vk::ImageLayout::GENERAL, vk::ImageLayout::GENERAL)),
            Self::LocalFirstUse | Self::ReleasedButAtomicRejected => None,
            Self::ForeignPendingKmsFromSink | Self::ForeignPendingKmsUninitialized => None,
        }
    }

    fn local_copy_old_layout(self) -> io::Result<vk::ImageLayout> {
        match self {
            Self::ForeignImportedFirstUse | Self::ForeignRetiredByKms => {
                Ok(vk::ImageLayout::GENERAL)
            }
            Self::LocalFirstUse | Self::ReleasedButAtomicRejected => Ok(vk::ImageLayout::UNDEFINED),
            Self::ForeignPendingKmsFromSink | Self::ForeignPendingKmsUninitialized => Err(
                io::Error::other("copied destination reused before KMS policy resolved"),
            ),
        }
    }

    fn after_lifecycle_quiescence(self) -> Self {
        let _ = self;
        // KMS is off and B is idle. Every copied frame is a guaranteed full
        // overwrite, so discard the old contents and start locally rather
        // than fabricating a release from an external owner that no longer
        // participates in the lifecycle.
        Self::ReleasedButAtomicRejected
    }

    fn after_kms_modeset(self) -> Self {
        match self {
            Self::ForeignPendingKmsFromSink | Self::ForeignPendingKmsUninitialized => self,
            Self::ForeignRetiredByKms => Self::ForeignPendingKmsFromSink,
            Self::LocalFirstUse
            | Self::ForeignImportedFirstUse
            | Self::ReleasedButAtomicRejected => Self::ForeignPendingKmsUninitialized,
        }
    }

    fn after_kms_retirement(self, bo_idx: usize) -> io::Result<Self> {
        match self {
            Self::ForeignPendingKmsFromSink => Ok(Self::ForeignRetiredByKms),
            Self::ForeignPendingKmsUninitialized => {
                // KMS never establishes a Vulkan layout. Keep the next use as
                // a FOREIGN acquire that discards from UNDEFINED.
                Ok(Self::ForeignImportedFirstUse)
            }
            state => Err(io::Error::other(format!(
                "copied destination {bo_idx} retired from unexpected ownership {state:?}",
            ))),
        }
    }
}

impl CopiedSourceOwnership {
    fn transport_preparation(self) -> io::Result<CopiedTransportPreparation> {
        match self {
            Self::RendererFirstUse | Self::RendererDiscard => Ok(CopiedTransportPreparation {
                foreign_acquire: false,
                local_old_layout: vk::ImageLayout::UNDEFINED,
            }),
            Self::ForeignAwaitingRenderer => Ok(CopiedTransportPreparation {
                foreign_acquire: true,
                local_old_layout: vk::ImageLayout::GENERAL,
            }),
            Self::ForeignAwaitingSink => Err(io::Error::other(
                "copied transport cannot return to renderer before sink handoff",
            )),
            Self::ForeignReturnPending => Err(io::Error::other(
                "copied transport B-to-A completion is not resolved",
            )),
        }
    }

    fn after_lifecycle_quiescence(self) -> Self {
        let _ = self;
        // A and B are both idle and the display has been taken off-screen.
        // The next copied compose is Full, so abandoning any interrupted
        // handoff and reinitializing from UNDEFINED is the safe common state.
        Self::RendererDiscard
    }
}

impl CopiedRenderTargetContents {
    fn note_submit_succeeded(&mut self) {
        *self = Self::Initialized;
    }

    fn invalidate(&mut self) {
        *self = Self::Uninitialized;
    }

    fn validate_readback(self) -> io::Result<()> {
        if self == Self::Initialized {
            Ok(())
        } else {
            Err(io::Error::other(
                "copied source has no preserved local render-target pixels",
            ))
        }
    }
}

enum RetainedSyncFile {
    AlreadySignalled,
    Fd(OwnedFd),
}

impl RetainedSyncFile {
    fn from_optional(fd: Option<OwnedFd>) -> Self {
        match fd {
            Some(fd) => Self::Fd(fd),
            None => Self::AlreadySignalled,
        }
    }

    fn into_optional(self) -> Option<OwnedFd> {
        match self {
            Self::AlreadySignalled => None,
            Self::Fd(fd) => Some(fd),
        }
    }
}

impl ExportSemaphoreReuseState {
    fn begin_post_submit_export(&mut self) {
        *self = Self::NeedsRearm;
    }

    fn finish_successful_export(&mut self) {
        *self = Self::Reusable;
    }

    fn needs_rearm(self) -> bool {
        self == Self::NeedsRearm
    }
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

impl BoState {
    /// `Free → Recording`: acquire for next frame's render target.
    pub fn transition_to_recording(&mut self) {
        debug_assert_eq!(self.phase, BoPhase::Free);
        self.phase = BoPhase::Recording;
    }

    /// `Recording → Submitted`: `vkQueueSubmit2` issued. Caller
    /// already exported `IN_FENCE_FD` and passes it in.
    pub fn transition_to_submitted(&mut self, in_fence_fd: i32) {
        debug_assert_eq!(self.phase, BoPhase::Recording);
        self.phase = BoPhase::Submitted;
        self.in_fence_fd = (in_fence_fd >= 0).then_some(in_fence_fd);
    }

    /// `Submitted → Pending`: atomic accepted. Returns the in-fence
    /// fd so the caller can close it (the kernel takes a reference
    /// to the underlying `sync_file` during the commit but does NOT
    /// own the fd — userspace must close it). Adopts the out-fence
    /// fd from KMS as the release fence.
    #[must_use = "in-fence fd must be closed by the caller"]
    pub fn transition_to_pending(&mut self, out_fence_fd: i32) -> Option<i32> {
        debug_assert_eq!(self.phase, BoPhase::Submitted);
        self.phase = BoPhase::Pending;
        let in_fence = self.in_fence_fd.take();
        self.release_fence_fd = (out_fence_fd >= 0).then_some(out_fence_fd);
        in_fence
    }

    /// `Submitted → Recording`: atomic returned `-EBUSY`. Caller is
    /// responsible for closing the returned in-fence fd.
    pub fn transition_to_recording_after_atomic_reject(&mut self) -> Option<i32> {
        debug_assert_eq!(self.phase, BoPhase::Submitted);
        self.phase = BoPhase::Recording;
        self.in_fence_fd.take()
    }

    /// `Submitted → Free`: modeset preempts (CRTC reconfigure
    /// mid-flight). Caller has already host-waited on the in-flight
    /// GPU work and must close the returned fd.
    pub fn transition_to_free_after_modeset_preempt(&mut self) -> Option<i32> {
        debug_assert_eq!(self.phase, BoPhase::Submitted);
        self.phase = BoPhase::Free;
        self.in_fence_fd.take()
    }

    /// `Pending → OnScreen`: first pageflip-complete event for this
    /// bo arrived.
    pub fn transition_to_on_screen(&mut self) {
        debug_assert_eq!(self.phase, BoPhase::Pending);
        self.phase = BoPhase::OnScreen;
    }

    /// `OnScreen → Retiring`: next flip's pageflip-complete arrived.
    /// Release fence is now signal-pending (will be signalled by
    /// KMS).
    pub fn transition_to_retiring(&mut self) {
        debug_assert_eq!(self.phase, BoPhase::OnScreen);
        self.phase = BoPhase::Retiring;
    }

    /// `Retiring → Free`: all GPU readers are done; caller will close
    /// the returned release fence fd.
    pub fn transition_to_free_after_retire(&mut self) -> Option<i32> {
        debug_assert_eq!(self.phase, BoPhase::Retiring);
        self.phase = BoPhase::Free;
        self.release_fence_fd.take()
    }

    /// `any → Free` on modeset reset (hotunplug, mode change). Caller
    /// must close every returned fd. The two slots may both be
    /// populated if the bo was Submitted-then-immediately-Pending
    /// somehow; in normal flow only one is.
    pub fn transition_to_free_after_modeset_reset(&mut self) -> ModesetReleased {
        let in_fence = self.in_fence_fd.take();
        let release_fence = self.release_fence_fd.take();
        self.phase = BoPhase::Free;
        ModesetReleased {
            in_fence,
            release_fence,
        }
    }

    /// Reserve a framebuffer installed by a synchronous modeset as the
    /// current front buffer. Unlike an ordinary nonblocking flip there is no
    /// PAGE_FLIP_EVENT transition through `Pending`; the commit has already
    /// latched before returning. Callers must have reset any old fence state
    /// first (or be reusing the existing OnScreen BO).
    pub fn mark_on_screen_after_modeset(&mut self) {
        debug_assert!(matches!(self.phase, BoPhase::Free | BoPhase::OnScreen));
        self.phase = BoPhase::OnScreen;
    }
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

impl OutputScanout {
    /// Semantic renderer-to-KMS route presented by this output.
    #[must_use]
    pub(crate) fn route(&self) -> ScanoutRoute {
        match self {
            Self::Shared(pool) => pool.route,
            Self::Copied(pool) => pool.route,
        }
    }

    /// Pool whose framebuffer is installed on the KMS CRTC.
    #[must_use]
    pub(crate) fn display_pool(&self) -> &ScanoutBoPool {
        match self {
            Self::Shared(pool) => pool,
            Self::Copied(pool) => &pool.destinations,
        }
    }

    pub(crate) fn display_pool_mut(&mut self) -> &mut ScanoutBoPool {
        match self {
            Self::Shared(pool) => pool,
            Self::Copied(pool) => &mut pool.destinations,
        }
    }

    #[must_use]
    #[allow(dead_code)] // immutable diagnostics; scene currently needs copied_mut.
    pub(crate) fn copied(&self) -> Option<&CopiedScanoutPool> {
        match self {
            Self::Shared(_) => None,
            Self::Copied(pool) => Some(pool),
        }
    }

    pub(crate) fn copied_mut(&mut self) -> Option<&mut CopiedScanoutPool> {
        match self {
            Self::Shared(_) => None,
            Self::Copied(pool) => Some(pool),
        }
    }

    pub(crate) fn note_kms_modeset_installed(&mut self, bo_idx: usize) -> io::Result<()> {
        match self {
            Self::Shared(_) => Ok(()),
            Self::Copied(pool) => pool.note_kms_modeset_installed(bo_idx),
        }
    }

    /// Leak only the KMS-visible destination backing after a failed final
    /// disable. Renderer-side copied sources are not referenced by KMS.
    pub(crate) fn disarm(&mut self) {
        match self {
            Self::Shared(pool) => {
                for bo in &mut pool.bos {
                    bo.disarm();
                }
            }
            Self::Copied(pool) => pool.disarm_display_backing(),
        }
    }

    pub(crate) fn drain_all_pending(&mut self, render_vk: &VkContext) -> io::Result<()> {
        match self {
            Self::Shared(pool) => {
                pool.drain_all_pending(render_vk);
                Ok(())
            }
            Self::Copied(pool) => pool.drain_all_pending(),
        }
    }
}

/// Exact renderer-source and sink-destination representations persisted from
/// disposable probing into live copied scanout.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CopiedScanoutPlan {
    pub(crate) source: CopiedSourcePlan,
    pub(crate) destination: ScanoutAllocationPlan,
}

impl CopiedScanoutPlan {
    pub(crate) fn describe(self) -> String {
        format!(
            "source-{} -> destination-{}",
            self.source.describe(),
            self.destination.describe(),
        )
    }
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

impl CopiedRenderSource {
    fn allocate_exact(
        render_vk: Arc<VkContext>,
        sink_vk: Arc<VkContext>,
        width: u32,
        height: u32,
        plan: CopiedSourcePlan,
    ) -> io::Result<Self> {
        let transport_on_renderer = allocate_copied_source_exact(
            &render_vk,
            width,
            height,
            vk::Format::B8G8R8A8_UNORM,
            plan,
        )
        .map_err(|result| scanout_vk_error("allocate exact copied source", result))?;
        let exported = super::dri3::export_backing(&render_vk, &transport_on_renderer)
            .map_err(|result| scanout_vk_error("export copied transport DMA-BUF", result))?;
        let imported_on_sink = DrawableImage::from_dmabuf_with_usage(
            Arc::clone(&sink_vk),
            exported.fd,
            width,
            height,
            vk::Format::B8G8R8A8_UNORM,
            exported.modifier,
            &[transport_on_renderer.offset],
            &[transport_on_renderer.stride],
            COPIED_SINK_IMPORT_USAGE,
        )
        .map_err(|error| copied_drawable_error("import copied transport on sink", error))?;
        let render_target =
            DrawableImage::new_server_owned_window(Arc::clone(&render_vk), width, height).map_err(
                |error| copied_drawable_error("allocate copied optimal render target", error),
            )?;

        let completion_semaphore = create_export_semaphore(&render_vk).map_err(|result| {
            scanout_vk_error("create copied source completion semaphore", result)
        })?;
        let transfer = match allocate_transfer_resources(&render_vk, width, height) {
            Ok(transfer) => transfer,
            Err(result) => {
                unsafe {
                    render_vk
                        .device
                        .destroy_semaphore(completion_semaphore, None);
                }
                return Err(scanout_vk_error(
                    "allocate copied source command resources",
                    result,
                ));
            }
        };

        Ok(Self {
            imported_on_sink: Some(imported_on_sink),
            transport_on_renderer: Some(transport_on_renderer),
            render_target: Some(render_target),
            completion_semaphore,
            completion_semaphore_reuse: ExportSemaphoreReuseState::Reusable,
            transfer,
            last_gpu_render_ns: None,
            render_vk,
            sink_vk,
            sink_wait_semaphore: None,
            renderer_wait_semaphore: None,
            renderer_return_completion: None,
            ownership: CopiedSourceOwnership::RendererFirstUse,
            render_target_contents: CopiedRenderTargetContents::Uninitialized,
            disarmed: false,
        })
    }

    #[must_use]
    pub(crate) fn image(&self) -> vk::Image {
        self.render_target
            .as_ref()
            .expect("live copied source has optimal render target")
            .vk_image
    }

    #[must_use]
    pub(crate) fn image_view(&self) -> vk::ImageView {
        self.render_target
            .as_ref()
            .expect("live copied source has optimal render target")
            .vk_image_view
    }

    #[must_use]
    fn transport_image(&self) -> vk::Image {
        self.transport_on_renderer
            .as_ref()
            .expect("live copied source has DMA-BUF transport")
            .image
    }

    #[must_use]
    pub(crate) fn width(&self) -> u32 {
        self.render_target
            .as_ref()
            .expect("live copied source has optimal render target")
            .extent
            .width
    }

    #[must_use]
    pub(crate) fn height(&self) -> u32 {
        self.render_target
            .as_ref()
            .expect("live copied source has optimal render target")
            .extent
            .height
    }

    pub(crate) fn export_render_completion(&mut self) -> Result<Option<OwnedFd>, vk::Result> {
        self.completion_semaphore_reuse.begin_post_submit_export();
        let info = vk::SemaphoreGetFdInfoKHR::default()
            .semaphore(self.completion_semaphore)
            .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
        let raw = unsafe {
            self.render_vk
                .external_semaphore_fd
                .get_semaphore_fd(&info)?
        };
        let completion =
            super::optional_sync_fd_from_vk(raw, "vkGetSemaphoreFdKHR(copied render SYNC_FD)")?;
        self.completion_semaphore_reuse.finish_successful_export();
        Ok(completion)
    }

    /// Replace a completion semaphore whose post-submit export failed.
    ///
    /// The caller must first prove the renderer submission completed. A fresh
    /// semaphore is created before destroying the dirty one so allocation
    /// failure leaves the source quarantinable rather than half-initialized.
    pub(crate) fn rearm_completion_semaphore_after_quiescence(&mut self) -> Result<(), vk::Result> {
        if !self.completion_semaphore_reuse.needs_rearm() {
            return Ok(());
        }
        let replacement = create_export_semaphore(&self.render_vk)?;
        unsafe {
            self.render_vk
                .device
                .destroy_semaphore(self.completion_semaphore, None);
        }
        self.completion_semaphore = replacement;
        self.completion_semaphore_reuse.finish_successful_export();
        Ok(())
    }

    /// Import renderer B's retained completion before recording an overwrite
    /// of the reused DMA-BUF transport. The subsequent preflight determines
    /// whether the command buffer records a FOREIGN -> A acquire barrier.
    pub(crate) fn prepare_renderer_acquire(&mut self) -> io::Result<()> {
        match self.ownership {
            CopiedSourceOwnership::RendererFirstUse | CopiedSourceOwnership::RendererDiscard => {
                Ok(())
            }
            CopiedSourceOwnership::ForeignAwaitingRenderer => {
                if self.renderer_return_completion.is_some()
                    && self.renderer_wait_semaphore.is_some()
                {
                    return Err(io::Error::other(
                        "copied source retained a new B completion before the prior A wait semaphore retired",
                    ));
                }
                if let Some(completion) = self.renderer_return_completion.take() {
                    let wait = super::sync::import_optional_sync_file(
                        &self.render_vk,
                        completion.into_optional(),
                    )
                    .map_err(|result| {
                        scanout_vk_error("import copied sink completion on renderer", result)
                    })?;
                    self.renderer_wait_semaphore = Some(wait);
                } else if self.renderer_wait_semaphore.is_none() {
                    return Err(io::Error::other(
                        "copied source has no retained B completion for renderer acquire",
                    ));
                }
                Ok(())
            }
            CopiedSourceOwnership::ForeignAwaitingSink => Err(io::Error::other(
                "copied source cannot return to renderer before sink handoff",
            )),
            CopiedSourceOwnership::ForeignReturnPending => Err(io::Error::other(
                "copied source B-to-A completion is not resolved",
            )),
        }
    }

    #[must_use]
    pub(crate) fn renderer_wait_semaphore(&self) -> Option<vk::Semaphore> {
        self.renderer_wait_semaphore
    }

    pub(crate) fn transport_preparation(&self) -> io::Result<CopiedTransportPreparation> {
        self.ownership.transport_preparation()
    }

    /// Confirm that renderer A's local optimal target contains a completed or
    /// queue-ordered compose suitable for readback. The target never crosses
    /// devices, so this deliberately does not consume B's retained completion
    /// or mutate transport ownership.
    pub(crate) fn validate_renderer_readback(&self) -> io::Result<()> {
        self.render_target_contents.validate_readback()
    }

    pub(crate) fn note_renderer_submit_succeeded(&mut self) {
        debug_assert!(matches!(
            self.ownership,
            CopiedSourceOwnership::RendererFirstUse
                | CopiedSourceOwnership::RendererDiscard
                | CopiedSourceOwnership::ForeignAwaitingRenderer
        ));
        self.render_target_contents.note_submit_succeeded();
        self.ownership = CopiedSourceOwnership::ForeignAwaitingSink;
        self.renderer_return_completion = None;
    }

    fn note_sink_submit_succeeded(&mut self) {
        debug_assert_eq!(self.ownership, CopiedSourceOwnership::ForeignAwaitingSink);
        self.ownership = CopiedSourceOwnership::ForeignReturnPending;
    }

    fn retain_sink_release_completion(&mut self, completion: Option<OwnedFd>) {
        debug_assert_eq!(self.ownership, CopiedSourceOwnership::ForeignReturnPending);
        self.renderer_return_completion = Some(RetainedSyncFile::from_optional(completion));
        self.ownership = CopiedSourceOwnership::ForeignAwaitingRenderer;
    }

    fn recover_before_sink_submit_after_quiescence(&mut self) -> io::Result<()> {
        match self.ownership {
            CopiedSourceOwnership::ForeignAwaitingSink => {
                // B never acquired/released the source, so there is no
                // legitimate foreign return to pair with. The scene waits A's
                // compose fence before reuse; the next full repaint discards
                // from UNDEFINED and implicitly reacquires instead.
                self.renderer_return_completion = None;
                self.ownership = CopiedSourceOwnership::RendererDiscard;
                Ok(())
            }
            CopiedSourceOwnership::ForeignAwaitingRenderer => Ok(()),
            CopiedSourceOwnership::ForeignReturnPending => {
                // B's queue is idle, so its recorded B->FOREIGN release is
                // complete even though exporting/duplicating the sync_file
                // failed. Retain Vulkan's already-signalled sentinel for the
                // matching A acquire.
                self.renderer_return_completion = Some(RetainedSyncFile::AlreadySignalled);
                self.ownership = CopiedSourceOwnership::ForeignAwaitingRenderer;
                Ok(())
            }
            CopiedSourceOwnership::RendererFirstUse => Err(io::Error::other(
                "copied sink failure observed before renderer ownership release",
            )),
            CopiedSourceOwnership::RendererDiscard => Ok(()),
        }
    }

    /// Make a failed copied cycle reusable after the scene successfully waited
    /// renderer A's compose fence. This covers both an A handoff failure
    /// (ownership is still awaiting B) and a later B/copy/KMS failure (B's
    /// recovery already retained a return completion).
    pub(crate) fn recover_failed_cycle_after_renderer_quiescence(&mut self) -> io::Result<()> {
        self.render_target_contents.invalidate();
        self.rearm_completion_semaphore_after_quiescence()
            .map_err(|result| {
                scanout_vk_error(
                    "rearm copied renderer completion semaphore after failed handoff",
                    result,
                )
            })?;
        self.release_renderer_wait_semaphore();
        match self.ownership {
            CopiedSourceOwnership::ForeignAwaitingSink => {
                self.renderer_return_completion = None;
                self.ownership = CopiedSourceOwnership::RendererDiscard;
                Ok(())
            }
            CopiedSourceOwnership::ForeignAwaitingRenderer
            | CopiedSourceOwnership::RendererDiscard => Ok(()),
            CopiedSourceOwnership::RendererFirstUse
            | CopiedSourceOwnership::ForeignReturnPending => Err(io::Error::other(
                "copied failed-cycle recovery found unsafe ownership",
            )),
        }
    }

    fn reset_after_lifecycle_quiescence(&mut self) -> io::Result<()> {
        self.render_target_contents.invalidate();
        self.rearm_completion_semaphore_after_quiescence()
            .map_err(|result| {
                scanout_vk_error(
                    "rearm copied renderer completion semaphore after lifecycle quiescence",
                    result,
                )
            })?;
        self.release_sink_wait_semaphore();
        self.release_renderer_wait_semaphore();
        self.renderer_return_completion = None;
        self.ownership = self.ownership.after_lifecycle_quiescence();
        Ok(())
    }

    fn release_sink_wait_semaphore(&mut self) {
        if let Some(semaphore) = self.sink_wait_semaphore.take() {
            unsafe { self.sink_vk.device.destroy_semaphore(semaphore, None) };
        }
    }

    fn release_renderer_wait_semaphore(&mut self) {
        if let Some(semaphore) = self.renderer_wait_semaphore.take() {
            unsafe { self.render_vk.device.destroy_semaphore(semaphore, None) };
        }
    }

    fn imported_sink_image(&self) -> vk::Image {
        self.imported_on_sink
            .as_ref()
            .expect("live copied source has sink import")
            .vk_image
    }

    /// Record renderer A's post-compose copy into the selected transport.
    ///
    /// Queue-family ownership barriers and local layout transitions are kept
    /// in separate commands because overlapping transitions for one image in
    /// a single dependency info are not sequential. Only the DMA-BUF transport
    /// crosses FOREIGN; the optimal target remains renderer-local in GENERAL.
    pub(crate) fn record_transport_copy(
        &self,
        command_buffer: vk::CommandBuffer,
        preparation: CopiedTransportPreparation,
    ) {
        let device = &self.render_vk.device;
        unsafe {
            if preparation.foreign_acquire {
                let acquire = [vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                    .src_access_mask(vk::AccessFlags2::MEMORY_WRITE)
                    .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                    .dst_access_mask(vk::AccessFlags2::MEMORY_READ | vk::AccessFlags2::MEMORY_WRITE)
                    .src_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
                    .dst_queue_family_index(self.render_vk.graphics_queue_family)
                    .old_layout(vk::ImageLayout::GENERAL)
                    .new_layout(vk::ImageLayout::GENERAL)
                    .image(self.transport_image())
                    .subresource_range(color_subresource_range())];
                device.cmd_pipeline_barrier2(
                    command_buffer,
                    &vk::DependencyInfo::default().image_memory_barriers(&acquire),
                );
            }

            let local_to_copy = [
                vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
                    .src_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
                    .dst_stage_mask(vk::PipelineStageFlags2::COPY)
                    .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
                    .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                    .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                    .image(self.image())
                    .subresource_range(color_subresource_range()),
                vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(if preparation.foreign_acquire {
                        vk::PipelineStageFlags2::ALL_COMMANDS
                    } else {
                        vk::PipelineStageFlags2::TOP_OF_PIPE
                    })
                    .src_access_mask(if preparation.foreign_acquire {
                        vk::AccessFlags2::MEMORY_READ | vk::AccessFlags2::MEMORY_WRITE
                    } else {
                        vk::AccessFlags2::empty()
                    })
                    .dst_stage_mask(vk::PipelineStageFlags2::COPY)
                    .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                    .old_layout(preparation.local_old_layout)
                    .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                    .image(self.transport_image())
                    .subresource_range(color_subresource_range()),
            ];
            device.cmd_pipeline_barrier2(
                command_buffer,
                &vk::DependencyInfo::default().image_memory_barriers(&local_to_copy),
            );

            let regions = [vk::ImageCopy2::default()
                .src_subresource(color_subresource_layers())
                .dst_subresource(color_subresource_layers())
                .extent(vk::Extent3D {
                    width: self.width(),
                    height: self.height(),
                    depth: 1,
                })];
            device.cmd_copy_image2(
                command_buffer,
                &vk::CopyImageInfo2::default()
                    .src_image(self.image())
                    .src_image_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                    .dst_image(self.transport_image())
                    .dst_image_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                    .regions(&regions),
            );

            let local_to_general = [
                vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::COPY)
                    .src_access_mask(vk::AccessFlags2::TRANSFER_READ)
                    .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                    .dst_access_mask(vk::AccessFlags2::MEMORY_READ)
                    .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                    .new_layout(vk::ImageLayout::GENERAL)
                    .image(self.image())
                    .subresource_range(color_subresource_range()),
                vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::COPY)
                    .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                    .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                    .dst_access_mask(vk::AccessFlags2::MEMORY_READ)
                    .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                    .new_layout(vk::ImageLayout::GENERAL)
                    .image(self.transport_image())
                    .subresource_range(color_subresource_range()),
            ];
            device.cmd_pipeline_barrier2(
                command_buffer,
                &vk::DependencyInfo::default().image_memory_barriers(&local_to_general),
            );

            let release = [vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                .src_access_mask(vk::AccessFlags2::MEMORY_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                .dst_access_mask(vk::AccessFlags2::empty())
                .src_queue_family_index(self.render_vk.graphics_queue_family)
                .dst_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
                .old_layout(vk::ImageLayout::GENERAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .image(self.transport_image())
                .subresource_range(color_subresource_range())];
            device.cmd_pipeline_barrier2(
                command_buffer,
                &vk::DependencyInfo::default().image_memory_barriers(&release),
            );
        }
    }

    /// Copy the renderer-local optimal target into this source's tightly
    /// packed host-visible probe buffer. The caller records this only after
    /// [`Self::record_transport_copy`], while the target is back in `GENERAL`.
    /// The DMA-BUF transport remains FOREIGN-owned and is deliberately not
    /// touched by this diagnostic readback.
    fn record_probe_readback(
        &self,
        command_buffer: vk::CommandBuffer,
        readback: CopiedProbeReadback<'_>,
    ) {
        let device = &self.render_vk.device;
        unsafe {
            let to_copy = [vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                .src_access_mask(vk::AccessFlags2::MEMORY_READ)
                .dst_stage_mask(vk::PipelineStageFlags2::COPY)
                .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
                .old_layout(vk::ImageLayout::GENERAL)
                .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .image(self.image())
                .subresource_range(color_subresource_range())];
            device.cmd_pipeline_barrier2(
                command_buffer,
                &vk::DependencyInfo::default().image_memory_barriers(&to_copy),
            );

            let regions = [tight_bgra_buffer_image_copy(self.width(), self.height())];
            crate::vk_count!(cmd_copy_image_to_buffer);
            device.cmd_copy_image_to_buffer(
                command_buffer,
                self.image(),
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                readback.destination_buffer(&self.transfer),
                &regions,
            );

            let image_to_general = [vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::COPY)
                .src_access_mask(vk::AccessFlags2::TRANSFER_READ)
                .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                .dst_access_mask(vk::AccessFlags2::MEMORY_READ)
                .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .image(self.image())
                .subresource_range(color_subresource_range())];
            match readback {
                CopiedProbeReadback::CpuExact => {
                    let buffer_to_host = [probe_buffer_to_host_barrier(&self.transfer)];
                    device.cmd_pipeline_barrier2(
                        command_buffer,
                        &vk::DependencyInfo::default()
                            .image_memory_barriers(&image_to_general)
                            .buffer_memory_barriers(&buffer_to_host),
                    );
                }
                CopiedProbeReadback::GpuDigest(digest) => {
                    device.cmd_pipeline_barrier2(
                        command_buffer,
                        &vk::DependencyInfo::default().image_memory_barriers(&image_to_general),
                    );
                    digest.record_after_transfer(command_buffer);
                }
            }
        }
    }

    fn probe_readback_bytes(&self) -> io::Result<&[u8]> {
        tight_mapped_bgra_bytes(&self.transfer, self.width(), self.height())
    }

    /// Preserve every Vulkan child and both owning contexts when GPU
    /// quiescence could not be proven. This is the copied-path analogue of
    /// [`ScanoutBo::disarm`]: leaking is safer than freeing referenced memory.
    fn disarm(&mut self) {
        if self.disarmed {
            return;
        }
        leak_owned_backing(&mut self.imported_on_sink);
        leak_owned_backing(&mut self.transport_on_renderer);
        leak_owned_backing(&mut self.render_target);
        std::mem::forget(Arc::clone(&self.render_vk));
        std::mem::forget(Arc::clone(&self.sink_vk));
        self.disarmed = true;
    }
}

impl Drop for CopiedRenderSource {
    fn drop(&mut self) {
        if self.disarmed {
            log::warn!(
                "copied source disarmed after failed GPU quiescence; leaking Vulkan resources"
            );
            return;
        }
        self.release_sink_wait_semaphore();
        self.release_renderer_wait_semaphore();
        unsafe {
            destroy_transfer_resources(&self.render_vk, &mut self.transfer);
            self.render_vk
                .device
                .destroy_semaphore(self.completion_semaphore, None);
        }
    }
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

impl CopiedScanoutPool {
    pub(crate) fn finish_disposable_probe(
        self,
        result: Result<(), DisposableProbeError>,
    ) -> Result<(), DisposableProbeError> {
        finish_disposable_probe_attempt(self, result)
    }

    /// Enumerate exact copied candidates in transport tiers. All mutually
    /// supported native transport modifiers are exhausted before explicit
    /// LINEAR; within each tier the established destination allocator order
    /// remains authoritative.
    #[must_use]
    pub(crate) fn exact_allocation_plans(
        render_vk: &VkContext,
        sink_vk: &VkContext,
        drm: &Rc<crate::drm::Device>,
        width: u32,
        scanout_modifiers: &[u64],
    ) -> Vec<CopiedScanoutPlan> {
        if render_vk.device_selector() == sink_vk.device_selector()
            || !render_vk.queue_family_foreign
            || !sink_vk.queue_family_foreign
        {
            return Vec::new();
        }
        let sources = exact_copied_source_plans(render_vk, sink_vk);
        let destinations =
            ScanoutBoPool::exact_allocation_plans(sink_vk, drm, width, scanout_modifiers);
        assemble_copied_scanout_plans(&destinations, &sources)
    }

    /// Allocate the complete copied pool using one exact plan for all slots.
    /// `route` is A->B; `destination_route` describes the sink Vulkan device
    /// writing the same B KMS endpoint and is stored on the destination pool.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn allocate_exact(
        render_vk: Arc<VkContext>,
        sink_vk: Arc<VkContext>,
        drm: Rc<crate::drm::Device>,
        route: ScanoutRoute,
        destination_route: ScanoutRoute,
        width: u32,
        height: u32,
        count: usize,
        scanout_modifiers: &[u64],
        plan: CopiedScanoutPlan,
    ) -> io::Result<Self> {
        Self::allocate_exact_with_policy(
            render_vk,
            sink_vk,
            drm,
            route,
            destination_route,
            width,
            height,
            count,
            scanout_modifiers,
            plan,
            AllocationCleanupPolicy::BestEffort,
        )
        .map_err(DisposableProbeError::into_io_error)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn allocate_exact_for_disposable_probe(
        render_vk: Arc<VkContext>,
        sink_vk: Arc<VkContext>,
        drm: Rc<crate::drm::Device>,
        route: ScanoutRoute,
        destination_route: ScanoutRoute,
        width: u32,
        height: u32,
        count: usize,
        scanout_modifiers: &[u64],
        plan: CopiedScanoutPlan,
    ) -> Result<Self, DisposableProbeError> {
        Self::allocate_exact_with_policy(
            render_vk,
            sink_vk,
            drm,
            route,
            destination_route,
            width,
            height,
            count,
            scanout_modifiers,
            plan,
            AllocationCleanupPolicy::StrictDisposable,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn allocate_exact_with_policy(
        render_vk: Arc<VkContext>,
        sink_vk: Arc<VkContext>,
        drm: Rc<crate::drm::Device>,
        route: ScanoutRoute,
        destination_route: ScanoutRoute,
        width: u32,
        height: u32,
        count: usize,
        scanout_modifiers: &[u64],
        plan: CopiedScanoutPlan,
        cleanup_policy: AllocationCleanupPolicy,
    ) -> Result<Self, DisposableProbeError> {
        validate_copied_route_pair(route, destination_route).map_err(DisposableProbeError::from)?;
        if render_vk.device_selector() == sink_vk.device_selector() {
            return Err(DisposableProbeError::from(io::Error::new(
                io::ErrorKind::InvalidInput,
                "copied scanout requires distinct renderer and sink Vulkan devices",
            )));
        }
        if !render_vk.queue_family_foreign || !sink_vk.queue_family_foreign {
            return Err(DisposableProbeError::from(io::Error::new(
                io::ErrorKind::Unsupported,
                "copied scanout requires VK_EXT_queue_family_foreign on renderer and sink",
            )));
        }

        let destinations = match cleanup_policy {
            AllocationCleanupPolicy::BestEffort => ScanoutBoPool::allocate_exact(
                Arc::clone(&sink_vk),
                drm,
                destination_route,
                width,
                height,
                count,
                scanout_modifiers,
                plan.destination,
            )
            .map_err(DisposableProbeError::from),
            AllocationCleanupPolicy::StrictDisposable => {
                ScanoutBoPool::allocate_exact_for_disposable_probe(
                    Arc::clone(&sink_vk),
                    drm,
                    destination_route,
                    width,
                    height,
                    count,
                    scanout_modifiers,
                    plan.destination,
                )
            }
        }
        .map_err(|error| {
            error.with_context(format!("exact copied {} destination pool", plan.describe()))
        })?;

        let initial_destination_ownership = match plan.destination.ownership() {
            ScanoutOwnership::Output => CopiedDestinationOwnership::ForeignImportedFirstUse,
            ScanoutOwnership::Renderer => CopiedDestinationOwnership::LocalFirstUse,
        };
        let mut sources = Vec::with_capacity(count);
        for index in 0..count {
            let source = CopiedRenderSource::allocate_exact(
                Arc::clone(&render_vk),
                Arc::clone(&sink_vk),
                width,
                height,
                plan.source,
            )
            .map_err(|error| {
                DisposableProbeError::from(scanout_io_context(
                    format!("exact copied {} source BO {index}", plan.describe()),
                    error,
                ))
            });
            match source {
                Ok(source) => sources.push(source),
                Err(error)
                    if matches!(cleanup_policy, AllocationCleanupPolicy::StrictDisposable) =>
                {
                    let partial_pool = Self {
                        sources,
                        destinations,
                        route,
                        plan,
                        sink_vk,
                        destination_ownership: vec![initial_destination_ownership; count],
                    };
                    return Err(partial_pool
                        .finish_disposable_probe(Err(error))
                        .expect_err("failed copied allocation cannot become a successful probe"));
                }
                Err(error) => return Err(error),
            }
        }

        Ok(Self {
            sources,
            destinations,
            route,
            plan,
            sink_vk,
            destination_ownership: vec![initial_destination_ownership; count],
        })
    }

    /// Submit B's copy after A's completion fd became readable. Readiness is
    /// scheduling only: B still imports and waits the synchronization payload.
    pub(crate) fn submit_copy(
        &mut self,
        bo_idx: usize,
        render_completion: Option<OwnedFd>,
    ) -> io::Result<Option<OwnedFd>> {
        self.submit_copy_with_fence(bo_idx, render_completion, vk::Fence::null(), None)
    }

    fn submit_copy_with_fence(
        &mut self,
        bo_idx: usize,
        render_completion: Option<OwnedFd>,
        fence: vk::Fence,
        probe_readback: Option<CopiedProbeReadback<'_>>,
    ) -> io::Result<Option<OwnedFd>> {
        let source = self
            .sources
            .get_mut(bo_idx)
            .ok_or_else(|| io::Error::other("copied scanout source index out of range"))?;
        let destination = self
            .destinations
            .bos
            .get_mut(bo_idx)
            .ok_or_else(|| io::Error::other("copied scanout destination index out of range"))?;
        let destination_ownership = self
            .destination_ownership
            .get_mut(bo_idx)
            .ok_or_else(|| io::Error::other("copied scanout ownership index out of range"))?;
        let destination_foreign_acquire = destination_ownership.foreign_acquire_layouts();
        let destination_local_old = destination_ownership.local_copy_old_layout()?;

        source.release_sink_wait_semaphore();
        // `None` is Vulkan's fd=-1 already-signalled SYNC_FD payload. It must
        // still be imported and waited: the semaphore wait is the external
        // memory dependency paired with renderer A's ownership release.
        let wait_semaphore =
            super::sync::import_optional_sync_file(&self.sink_vk, render_completion)
                .map_err(|result| scanout_vk_error("import renderer completion on sink", result))?;
        source.sink_wait_semaphore = Some(wait_semaphore);

        let command_buffer = destination.vk_transfer.command_buffer;
        unsafe {
            self.sink_vk
                .device
                .reset_command_buffer(command_buffer, vk::CommandBufferResetFlags::empty())
                .map_err(|result| scanout_vk_error("reset copied sink command buffer", result))?;
            self.sink_vk
                .device
                .begin_command_buffer(
                    command_buffer,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )
                .map_err(|result| scanout_vk_error("begin copied sink command buffer", result))?;

            // Ownership acquires and local layout transitions are deliberately
            // separate commands. Two overlapping barriers in one dependency
            // info do not sequence GENERAL->GENERAL before GENERAL->TRANSFER.
            let mut ownership_acquires = Vec::with_capacity(2);
            ownership_acquires.push(
                vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                    .src_access_mask(vk::AccessFlags2::MEMORY_WRITE)
                    .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                    .dst_access_mask(vk::AccessFlags2::MEMORY_READ)
                    .src_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
                    .dst_queue_family_index(self.sink_vk.graphics_queue_family)
                    .old_layout(vk::ImageLayout::GENERAL)
                    .new_layout(vk::ImageLayout::GENERAL)
                    .image(source.imported_sink_image())
                    .subresource_range(color_subresource_range()),
            );
            if let Some((old_layout, new_layout)) = destination_foreign_acquire {
                ownership_acquires.push(
                    vk::ImageMemoryBarrier2::default()
                        .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                        .src_access_mask(vk::AccessFlags2::MEMORY_READ)
                        .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                        .dst_access_mask(vk::AccessFlags2::MEMORY_WRITE)
                        .src_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
                        .dst_queue_family_index(self.sink_vk.graphics_queue_family)
                        .old_layout(old_layout)
                        .new_layout(new_layout)
                        .image(destination.vk_image)
                        .subresource_range(color_subresource_range()),
                );
            }
            self.sink_vk.device.cmd_pipeline_barrier2(
                command_buffer,
                &vk::DependencyInfo::default().image_memory_barriers(&ownership_acquires),
            );

            let local_to_copy = [
                vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                    .src_access_mask(vk::AccessFlags2::MEMORY_WRITE)
                    .dst_stage_mask(vk::PipelineStageFlags2::COPY)
                    .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
                    .old_layout(vk::ImageLayout::GENERAL)
                    .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                    .image(source.imported_sink_image())
                    .subresource_range(color_subresource_range()),
                vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                    .src_access_mask(vk::AccessFlags2::MEMORY_READ)
                    .dst_stage_mask(vk::PipelineStageFlags2::COPY)
                    .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                    .old_layout(destination_local_old)
                    .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                    .image(destination.vk_image)
                    .subresource_range(color_subresource_range()),
            ];
            self.sink_vk.device.cmd_pipeline_barrier2(
                command_buffer,
                &vk::DependencyInfo::default().image_memory_barriers(&local_to_copy),
            );
            let regions = [vk::ImageCopy2::default()
                .src_subresource(color_subresource_layers())
                .dst_subresource(color_subresource_layers())
                .extent(vk::Extent3D {
                    width: source.width(),
                    height: source.height(),
                    depth: 1,
                })];
            self.sink_vk.device.cmd_copy_image2(
                command_buffer,
                &vk::CopyImageInfo2::default()
                    .src_image(source.imported_sink_image())
                    .src_image_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                    .dst_image(destination.vk_image)
                    .dst_image_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                    .regions(&regions),
            );
            let source_to_general = [vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::COPY)
                .src_access_mask(vk::AccessFlags2::TRANSFER_READ)
                .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                .dst_access_mask(vk::AccessFlags2::empty())
                .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .image(source.imported_sink_image())
                .subresource_range(color_subresource_range())];
            let destination_after_copy = [vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::COPY)
                .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .dst_stage_mask(if probe_readback.is_some() {
                    vk::PipelineStageFlags2::COPY
                } else {
                    vk::PipelineStageFlags2::ALL_COMMANDS
                })
                .dst_access_mask(if probe_readback.is_some() {
                    vk::AccessFlags2::TRANSFER_READ
                } else {
                    vk::AccessFlags2::MEMORY_READ
                })
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(if probe_readback.is_some() {
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL
                } else {
                    vk::ImageLayout::GENERAL
                })
                .image(destination.vk_image)
                .subresource_range(color_subresource_range())];
            self.sink_vk.device.cmd_pipeline_barrier2(
                command_buffer,
                &vk::DependencyInfo::default().image_memory_barriers(&source_to_general),
            );
            self.sink_vk.device.cmd_pipeline_barrier2(
                command_buffer,
                &vk::DependencyInfo::default().image_memory_barriers(&destination_after_copy),
            );

            if let Some(readback) = probe_readback {
                let regions = [tight_bgra_buffer_image_copy(
                    source.width(),
                    source.height(),
                )];
                crate::vk_count!(cmd_copy_image_to_buffer);
                self.sink_vk.device.cmd_copy_image_to_buffer(
                    command_buffer,
                    destination.vk_image,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    readback.destination_buffer(&destination.vk_transfer),
                    &regions,
                );

                let destination_to_general = [vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::COPY)
                    .src_access_mask(vk::AccessFlags2::TRANSFER_READ)
                    .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                    .dst_access_mask(vk::AccessFlags2::MEMORY_READ)
                    .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                    .new_layout(vk::ImageLayout::GENERAL)
                    .image(destination.vk_image)
                    .subresource_range(color_subresource_range())];
                match readback {
                    CopiedProbeReadback::CpuExact => {
                        let buffer_to_host =
                            [probe_buffer_to_host_barrier(&destination.vk_transfer)];
                        self.sink_vk.device.cmd_pipeline_barrier2(
                            command_buffer,
                            &vk::DependencyInfo::default()
                                .image_memory_barriers(&destination_to_general)
                                .buffer_memory_barriers(&buffer_to_host),
                        );
                    }
                    CopiedProbeReadback::GpuDigest(digest) => {
                        self.sink_vk.device.cmd_pipeline_barrier2(
                            command_buffer,
                            &vk::DependencyInfo::default()
                                .image_memory_barriers(&destination_to_general),
                        );
                        digest.record_after_transfer(command_buffer);
                    }
                }
            }

            let ownership_releases = [
                vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                    .src_access_mask(vk::AccessFlags2::MEMORY_READ)
                    .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                    .dst_access_mask(vk::AccessFlags2::empty())
                    .src_queue_family_index(self.sink_vk.graphics_queue_family)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
                    .old_layout(vk::ImageLayout::GENERAL)
                    .new_layout(vk::ImageLayout::GENERAL)
                    .image(source.imported_sink_image())
                    .subresource_range(color_subresource_range()),
                vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                    .src_access_mask(vk::AccessFlags2::MEMORY_WRITE)
                    .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                    .dst_access_mask(vk::AccessFlags2::empty())
                    .src_queue_family_index(self.sink_vk.graphics_queue_family)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
                    .old_layout(vk::ImageLayout::GENERAL)
                    .new_layout(vk::ImageLayout::GENERAL)
                    .image(destination.vk_image)
                    .subresource_range(color_subresource_range()),
            ];
            self.sink_vk.device.cmd_pipeline_barrier2(
                command_buffer,
                &vk::DependencyInfo::default().image_memory_barriers(&ownership_releases),
            );
            self.sink_vk
                .device
                .end_command_buffer(command_buffer)
                .map_err(|result| scanout_vk_error("end copied sink command buffer", result))?;

            let waits = [vk::SemaphoreSubmitInfo::default()
                .semaphore(wait_semaphore)
                .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)];
            let commands = [vk::CommandBufferSubmitInfo::default().command_buffer(command_buffer)];
            let signals = [vk::SemaphoreSubmitInfo::default()
                .semaphore(destination.vk_semaphore)
                .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)];
            let submits = [vk::SubmitInfo2::default()
                .wait_semaphore_infos(&waits)
                .command_buffer_infos(&commands)
                .signal_semaphore_infos(&signals)];
            self.sink_vk
                .device
                .queue_submit2(self.sink_vk.graphics_queue, &submits, fence)
                .map_err(|result| scanout_vk_error("submit copied sink transfer", result))?;
        }
        source.note_sink_submit_succeeded();
        *destination_ownership = CopiedDestinationOwnership::ForeignPendingKmsFromSink;
        let completion = destination
            .export_signaled_fd()
            .map_err(|result| scanout_vk_error("export copied sink completion", result))?;
        let renderer_completion = completion
            .as_ref()
            .map(OwnedFd::try_clone)
            .transpose()
            .map_err(|error| {
                scanout_io_context("retain copied sink completion for renderer acquire", error)
            })?;
        source.retain_sink_release_completion(renderer_completion);
        Ok(completion)
    }

    /// Probe all exact slots with a spatially unique A render, external
    /// semaphore handoff, B copy, and CPU-visible content validation. The
    /// normal path compares compact GPU-computed block digests; unsupported
    /// compute queues retain the exact full-image CPU fallback.
    /// Each slot runs twice: cycle two consumes B's retained
    /// completion on A, covering both directions of the source ownership
    /// protocol rather than admitting a route after only its first A -> B
    /// handoff. TEST_ONLY does not perform a KMS ownership release, so the
    /// destination is explicitly abandoned after cycle one and cycle two
    /// full-discards from UNDEFINED; GENERAL KMS -> B reuse requires a live
    /// two-flip hardware check. Each submitted renderer and sink batch gets a
    /// fresh bounded fence wait. Pipeline creation, command recording, and CPU
    /// validation are not compatibility failures merely because they take
    /// longer than that GPU-liveness timeout. An actual fence timeout or other
    /// uncertain post-submit failure consumes and quarantines this disposable
    /// pool so no GPU-referenced object reaches normal teardown.
    pub(crate) fn probe_copy_all(self, timeout_ns: u64) -> Result<(), DisposableProbeError> {
        let probe_started = Instant::now();
        let mut attempt = CopiedDisposableProbeAttempt {
            pool: self,
            pattern: None,
            render_digest: None,
            sink_digest: None,
        };
        let render_vk = match attempt.pool.sources.first() {
            Some(source) => Arc::clone(&source.render_vk),
            None => {
                return finish_disposable_probe_attempt(
                    attempt,
                    Err(DisposableProbeError::from(io::Error::other(
                        "copied probe pool has no source slots",
                    ))),
                );
            }
        };
        let pipeline_started = Instant::now();
        let pattern = match CopiedProbePatternPipeline::new(Arc::clone(&render_vk)) {
            Ok(pattern) => pattern,
            Err(result) => {
                // Pipeline creation performs no queue submission. The pool's
                // disposable contexts are therefore safe to destroy directly
                // even though generic context Drop remains conservative.
                return finish_disposable_probe_attempt(
                    attempt,
                    Err(DisposableProbeError::from(scanout_vk_error(
                        "create copied content-probe pipeline",
                        result,
                    ))),
                );
            }
        };
        attempt.pattern = Some(pattern);

        let sink_vk = Arc::clone(&attempt.pool.sink_vk);
        if ProbeDigestPipeline::is_supported(
            &render_vk,
            attempt.pool.destinations.width,
            attempt.pool.destinations.height,
        ) && ProbeDigestPipeline::is_supported(
            &sink_vk,
            attempt.pool.destinations.width,
            attempt.pool.destinations.height,
        ) {
            match ProbeDigestPipeline::new(
                Arc::clone(&render_vk),
                attempt.pool.destinations.width,
                attempt.pool.destinations.height,
            ) {
                Ok(digest) => attempt.render_digest = Some(digest),
                Err(vk::Result::ERROR_DEVICE_LOST) => {
                    return finish_disposable_probe_attempt(
                        attempt,
                        Err(DisposableProbeError::from(scanout_vk_error(
                            "create copied renderer GPU digest",
                            vk::Result::ERROR_DEVICE_LOST,
                        ))),
                    );
                }
                Err(result) => log::warn!(
                    "copied content probe could not create renderer GPU digest ({result:?}); \
                     falling back to exact CPU validation"
                ),
            }
            if attempt.render_digest.is_some() {
                match ProbeDigestPipeline::new(
                    sink_vk,
                    attempt.pool.destinations.width,
                    attempt.pool.destinations.height,
                ) {
                    Ok(digest) => attempt.sink_digest = Some(digest),
                    Err(vk::Result::ERROR_DEVICE_LOST) => {
                        return finish_disposable_probe_attempt(
                            attempt,
                            Err(DisposableProbeError::from(scanout_vk_error(
                                "create copied sink GPU digest",
                                vk::Result::ERROR_DEVICE_LOST,
                            ))),
                        );
                    }
                    Err(result) => log::warn!(
                        "copied content probe could not create sink GPU digest ({result:?}); \
                         falling back to exact CPU validation"
                    ),
                }
            }
        } else {
            log::warn!(
                "copied content probe selected queue or extent cannot run compact GPU digest; \
                 falling back to exact CPU validation"
            );
        }

        let pipeline_elapsed = pipeline_started.elapsed();
        if let Some((render_digest, sink_digest)) = attempt
            .render_digest
            .as_ref()
            .zip(attempt.sink_digest.as_ref())
        {
            debug_assert_eq!(render_digest.grid_width(), sink_digest.grid_width());
            debug_assert_eq!(render_digest.grid_height(), sink_digest.grid_height());
            debug_assert_eq!(
                render_digest.summary_word_count(),
                sink_digest.summary_word_count()
            );
            log::info!(
                "copied content probe pipelines ready in {} ms; validator=gpu-block-digest \
                 grid={}x{} summary={} bytes/device; each renderer/sink fence wait has {:?}",
                pipeline_elapsed.as_millis(),
                render_digest.grid_width(),
                render_digest.grid_height(),
                render_digest.summary_word_count() * std::mem::size_of::<u32>(),
                Duration::from_nanos(timeout_ns),
            );
        } else {
            log::info!(
                "copied content probe pipeline ready in {} ms; validator=cpu-exact; each \
                 renderer/sink fence wait has {:?}",
                pipeline_elapsed.as_millis(),
                Duration::from_nanos(timeout_ns),
            );
        }

        let result = attempt.pool.probe_copy_all_inner(
            &render_vk,
            attempt.pattern.as_ref().expect("probe pattern was created"),
            attempt
                .render_digest
                .as_ref()
                .zip(attempt.sink_digest.as_ref()),
            timeout_ns,
        );
        if result.is_ok() {
            log::info!(
                "copied content probe completed {} slots x 2 cycles in {} ms; per-fence timeout {:?}",
                attempt.pool.sources.len(),
                probe_started.elapsed().as_millis(),
                Duration::from_nanos(timeout_ns),
            );
        }
        if result
            .as_ref()
            .is_err_and(DisposableProbeError::bypass_normal_teardown)
        {
            log::error!(
                "copied content probe timed out or left GPU completion uncertain; retaining the \
                 disposable A/B pool and pipeline without vkDeviceWaitIdle"
            );
        }
        finish_disposable_probe_attempt(attempt, result)
    }

    fn probe_copy_all_inner(
        &mut self,
        render_vk: &Arc<VkContext>,
        pattern: &CopiedProbePatternPipeline,
        digests: Option<(&ProbeDigestPipeline, &ProbeDigestPipeline)>,
        timeout_ns: u64,
    ) -> Result<(), DisposableProbeError> {
        for bo_idx in 0..self.sources.len() {
            let mut previous_renderer_hash = None;
            let mut previous_renderer_digest: Option<Vec<u32>> = None;
            for cycle in 0..2 {
                let cycle_started = Instant::now();
                let sink_vk = Arc::clone(&self.sink_vk);
                let (renderer_readback, sink_readback) = match digests {
                    Some((renderer, sink)) => (
                        CopiedProbeReadback::GpuDigest(renderer),
                        CopiedProbeReadback::GpuDigest(sink),
                    ),
                    None => (CopiedProbeReadback::CpuExact, CopiedProbeReadback::CpuExact),
                };
                let frame_token = u32::try_from(bo_idx)
                    .ok()
                    .and_then(|index| index.checked_mul(2))
                    .and_then(|base| base.checked_add(cycle))
                    .ok_or_else(|| io::Error::other("copied probe frame token overflow"))?;
                let render_fence = create_probe_fence(render_vk)?;
                let mut render_fence = ProbeFence::new(&render_vk.device, render_fence);
                let sink_fence = match create_probe_fence(&sink_vk) {
                    Ok(fence) => fence,
                    Err(error) => {
                        render_fence.destroy_known_idle();
                        return Err(DisposableProbeError::from(error).with_context(format!(
                            "BO {bo_idx} cycle {cycle} copied sink fence creation"
                        )));
                    }
                };
                let mut sink_fence = ProbeFence::new(&sink_vk.device, sink_fence);
                let render_completion = match submit_copied_source_probe(
                    &mut self.sources[bo_idx],
                    pattern,
                    renderer_readback,
                    frame_token,
                    render_fence.handle(),
                ) {
                    Ok(completion) => completion,
                    Err(error) => {
                        let pending = if error.requires_quarantine() {
                            PendingProbeSubmissions::Render
                        } else {
                            PendingProbeSubmissions::None
                        };
                        return Err(finish_pending_probe_failure(
                            pending,
                            error,
                            &mut render_fence,
                            &mut sink_fence,
                        )
                        .with_context(format!(
                            "BO {bo_idx} cycle {cycle} copied renderer submission"
                        )));
                    }
                };
                let renderer_submitted = Instant::now();

                // The sink helper may fail either side of its queue-submit
                // call. A is already outstanding, so conservatively retain
                // both fence handles and the aggregate attempt on any error.
                let copy_completion = match self.submit_copy_with_fence(
                    bo_idx,
                    render_completion,
                    sink_fence.handle(),
                    Some(sink_readback),
                ) {
                    Ok(completion) => completion,
                    Err(error) => {
                        return Err(finish_pending_probe_failure(
                            PendingProbeSubmissions::RenderAndSink,
                            DisposableProbeError::from(error),
                            &mut render_fence,
                            &mut sink_fence,
                        )
                        .with_context(format!(
                            "BO {bo_idx} cycle {cycle} copied sink submission"
                        )));
                    }
                };
                drop(copy_completion);
                let sink_submitted = Instant::now();

                let fence_waits =
                    wait_copied_probe_fence_pair(&mut render_fence, &mut sink_fence, timeout_ns)
                        .map_err(|error| {
                            error.with_context(format!(
                                "BO {bo_idx} cycle {cycle} copied fence completion"
                            ))
                        })?;
                let sink_completed = Instant::now();
                let validation = (|| -> Result<(), DisposableProbeError> {
                    self.release_completed_source(bo_idx);
                    match digests {
                        Some((renderer, sink)) => {
                            let renderer_summary = renderer.read_summary().map_err(|result| {
                                copied_probe_digest_readback_error(
                                    "read copied renderer GPU digest",
                                    result,
                                )
                            })?;
                            let sink_summary = sink.read_summary().map_err(|result| {
                                copied_probe_digest_readback_error(
                                    "read copied sink GPU digest",
                                    result,
                                )
                            })?;
                            validate_copied_probe_digest_fiducials(
                                &renderer_summary,
                                bo_idx,
                                cycle,
                                frame_token,
                            )?;
                            verify_copied_probe_digests(
                                &renderer_summary,
                                &sink_summary,
                                renderer.grid_width(),
                                renderer.grid_height(),
                                bo_idx,
                                cycle,
                                frame_token,
                            )?;
                            validate_copied_probe_digest_freshness(
                                previous_renderer_digest.as_deref(),
                                &renderer_summary,
                                bo_idx,
                                cycle,
                                frame_token,
                            )?;
                            previous_renderer_digest = Some(renderer_summary);
                        }
                        None => {
                            let renderer_pixels = self.sources[bo_idx].probe_readback_bytes()?;
                            let sink_pixels = tight_mapped_bgra_bytes(
                                &self.destinations.bos[bo_idx].vk_transfer,
                                self.sources[bo_idx].width(),
                                self.sources[bo_idx].height(),
                            )?;
                            validate_copied_probe_fiducials(
                                renderer_pixels,
                                self.sources[bo_idx].width(),
                                self.sources[bo_idx].height(),
                                bo_idx,
                                cycle,
                                frame_token,
                            )?;
                            let renderer_hash = verify_copied_probe_pixels(
                                renderer_pixels,
                                sink_pixels,
                                self.sources[bo_idx].width(),
                                self.sources[bo_idx].height(),
                                bo_idx,
                                cycle,
                                frame_token,
                            )?;
                            validate_copied_probe_freshness(
                                previous_renderer_hash,
                                renderer_hash,
                                bo_idx,
                                cycle,
                                frame_token,
                            )?;
                            previous_renderer_hash = Some(renderer_hash);
                        }
                    }
                    if cycle == 0 {
                        // No real KMS commit acquired/released the destination.
                        // After B is proven idle, recover it as an atomic reject
                        // and let the next full copy discard from UNDEFINED rather
                        // than fabricating an external GENERAL return.
                        self.recover_copy_failure_after_quiescence(bo_idx)?;
                    }
                    Ok(())
                })();
                let validation_completed = Instant::now();
                let validation_verdict = match validation.as_ref() {
                    Ok(()) => "match",
                    Err(error) if scanout_error_is_device_lost(error.as_io_error()) => {
                        "device-lost"
                    }
                    Err(error) if error.abort_candidate_search() => "indeterminate",
                    Err(_) => "reject",
                };
                log::info!(
                    "copied content probe cycle: bo={bo_idx} cycle={cycle} verdict={} \
                     validator={} \
                     renderer-submit={}ms sink-submit={}ms renderer-wait={}ms sink-wait={}ms \
                     validation={}ms total={}ms per-fence-timeout={:?}",
                    validation_verdict,
                    if digests.is_some() {
                        "gpu-block-digest"
                    } else {
                        "cpu-exact"
                    },
                    renderer_submitted.duration_since(cycle_started).as_millis(),
                    sink_submitted
                        .duration_since(renderer_submitted)
                        .as_millis(),
                    fence_waits.renderer.as_millis(),
                    fence_waits.sink.as_millis(),
                    validation_completed
                        .duration_since(sink_completed)
                        .as_millis(),
                    validation_completed
                        .duration_since(cycle_started)
                        .as_millis(),
                    Duration::from_nanos(timeout_ns),
                );
                completed_probe_validation(validation)?;
            }
        }
        Ok(())
    }

    pub(crate) fn release_completed_source(&mut self, bo_idx: usize) {
        if let Some(source) = self.sources.get_mut(bo_idx) {
            source.release_sink_wait_semaphore();
            // B completion (and therefore its wait on A) has retired. The
            // temporary B->A wait from the previous A submission is no longer
            // GPU-referenced and must be destroyed before importing the next
            // cycle's retained completion.
            source.release_renderer_wait_semaphore();
        }
    }

    fn mark_disposable_probe_quiescent(&self) {
        self.sink_vk.mark_disposable_probe_quiescent();
        for source in &self.sources {
            source.render_vk.mark_disposable_probe_quiescent();
        }
    }

    /// Record that a later flip retired this destination from KMS. Only this
    /// replacement boundary makes the slot eligible for a subsequent B
    /// ownership acquire and write.
    pub(crate) fn note_kms_retired(&mut self, bo_idx: usize) -> io::Result<()> {
        let ownership = self
            .destination_ownership
            .get_mut(bo_idx)
            .ok_or_else(|| io::Error::other("copied scanout ownership index out of range"))?;
        *ownership = ownership.after_kms_retirement(bo_idx)?;
        Ok(())
    }

    /// A synchronous modeset may install a fresh destination without a prior
    /// B submission. Once it succeeds, KMS is nevertheless the external
    /// owner, so the next B write must acquire from FOREIGN.
    pub(crate) fn note_kms_modeset_installed(&mut self, bo_idx: usize) -> io::Result<()> {
        let ownership = self
            .destination_ownership
            .get_mut(bo_idx)
            .ok_or_else(|| io::Error::other("copied scanout ownership index out of range"))?;
        // Preserve whether GENERAL was established by a prior real B release.
        // A fresh/directly-modeset image remains layout-uninitialized even
        // after KMS has displayed it.
        *ownership = ownership.after_kms_modeset();
        Ok(())
    }

    /// Quiesce a failed B submission before returning the pair to the free
    /// list. Other slots, including the one currently scanned out, retain
    /// their state.
    pub(crate) fn recover_copy_failure(&mut self, bo_idx: usize) -> io::Result<()> {
        copied_quiescence_result("quiesce sink after copied scanout failure", unsafe {
            self.sink_vk.device.device_wait_idle()
        })?;
        self.recover_copy_failure_after_quiescence(bo_idx)
    }

    /// Recover a disposable copied-probe cycle after its explicit A and B
    /// fences have both signalled. Unlike live failure recovery, this proven
    /// boundary needs no device-wide idle wait.
    fn recover_copy_failure_after_quiescence(&mut self, bo_idx: usize) -> io::Result<()> {
        let source = self
            .sources
            .get_mut(bo_idx)
            .ok_or_else(|| io::Error::other("copied scanout source index out of range"))?;
        source.release_sink_wait_semaphore();
        source.recover_before_sink_submit_after_quiescence()?;
        let destination_ownership = self
            .destination_ownership
            .get_mut(bo_idx)
            .ok_or_else(|| io::Error::other("copied scanout ownership index out of range"))?;
        if *destination_ownership == CopiedDestinationOwnership::ForeignPendingKmsFromSink {
            // B did release to FOREIGN, but KMS never accepted/acquired it.
            // The next guaranteed-full copy discards from UNDEFINED rather
            // than inventing a matching external release.
            *destination_ownership = CopiedDestinationOwnership::ReleasedButAtomicRejected;
        }
        if let Some(destination) = self.destinations.bos.get_mut(bo_idx) {
            destination
                .rearm_export_semaphore_after_quiescence()
                .map_err(|result| {
                    scanout_vk_error(
                        "rearm copied sink completion semaphore after failed export",
                        result,
                    )
                })?;
        }
        Ok(())
    }

    pub(crate) fn drain_all_pending(&mut self) -> io::Result<()> {
        copied_quiescence_result("quiesce copied scanout sink", unsafe {
            self.sink_vk.device.device_wait_idle()
        })?;
        if let Some(render_vk) = self
            .sources
            .first()
            .map(|source| Arc::clone(&source.render_vk))
        {
            copied_quiescence_result("quiesce copied scanout renderer", unsafe {
                render_vk.device.device_wait_idle()
            })?;
        }
        for destination in &mut self.destinations.bos {
            destination
                .rearm_export_semaphore_after_quiescence()
                .map_err(|result| {
                    scanout_vk_error(
                        "rearm copied destination semaphore after lifecycle quiescence",
                        result,
                    )
                })?;
            close_modeset_released(destination.state.transition_to_free_after_modeset_reset());
        }
        for ownership in &mut self.destination_ownership {
            *ownership = ownership.after_lifecycle_quiescence();
        }
        for source in &mut self.sources {
            source.reset_after_lifecycle_quiescence()?;
        }
        Ok(())
    }

    fn disarm_uncertain_resources(&mut self) {
        for source in &mut self.sources {
            source.disarm();
        }
        for destination in &mut self.destinations.bos {
            destination.disarm();
        }
        // Destination BOs each drop one Arc, but the context itself must stay
        // alive because their disarmed raw handles still belong to it.
        std::mem::forget(Arc::clone(&self.sink_vk));
    }

    fn disarm_display_backing(&mut self) {
        for destination in &mut self.destinations.bos {
            destination.disarm();
        }
        // A copied sink context has no platform-global owner. Keep it alive so
        // disarmed destination VkImages remain backed while KMS may retain the
        // framebuffer after a failed final disable.
        std::mem::forget(Arc::clone(&self.sink_vk));
    }
}

impl Drop for CopiedScanoutPool {
    fn drop(&mut self) {
        let render_requires_idle = self
            .sources
            .iter()
            .any(|source| source.render_vk.requires_drop_device_idle());
        if !self.sink_vk.requires_drop_device_idle() && !render_requires_idle {
            return;
        }
        if let Err(error) = self.drain_all_pending() {
            log::error!(
                "copied scanout drop could not prove GPU quiescence ({error}); \
                 disarming and leaking uncertain resources"
            );
            self.disarm_uncertain_resources();
        }
    }
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

const DRM_PRIME_CAP_IMPORT: u64 = 1 << 0;
const DRM_PRIME_CAP_EXPORT: u64 = 1 << 1;

fn support_from_prime_bits(bits: u64, required: u64) -> ScanoutMetadataSupport {
    if bits & required != 0 {
        ScanoutMetadataSupport::Supported
    } else {
        ScanoutMetadataSupport::Unsupported
    }
}

fn combine_required_metadata(
    first: ScanoutMetadataSupport,
    second: ScanoutMetadataSupport,
) -> ScanoutMetadataSupport {
    use ScanoutMetadataSupport::{Supported, Unknown, Unsupported};

    match (first, second) {
        (Unsupported, _) | (_, Unsupported) => Unsupported,
        (Supported, Supported) => Supported,
        (Supported | Unknown, Supported | Unknown) => Unknown,
    }
}

fn kms_linear_layout(kms_scanout_modifiers: &[u64]) -> KmsLinearLayout {
    if kms_scanout_modifiers.is_empty() {
        KmsLinearLayout::LegacyAddfb
    } else if kms_scanout_modifiers.contains(&super::dri3::DRM_FORMAT_MOD_LINEAR) {
        KmsLinearLayout::ExplicitModifier
    } else {
        KmsLinearLayout::NotAdvertised
    }
}

fn kms_linear_layout_support(layout: KmsLinearLayout) -> ScanoutMetadataSupport {
    match layout {
        KmsLinearLayout::ExplicitModifier => ScanoutMetadataSupport::Supported,
        KmsLinearLayout::LegacyAddfb => ScanoutMetadataSupport::Unknown,
        KmsLinearLayout::NotAdvertised => ScanoutMetadataSupport::Unsupported,
    }
}

fn build_linear_metadata(
    prime: ScanoutMetadataSupport,
    vulkan: ScanoutMetadataSupport,
    kms_layout: KmsLinearLayout,
) -> DmabufLinearMetadata {
    let path = combine_required_metadata(
        prime,
        combine_required_metadata(vulkan, kms_linear_layout_support(kms_layout)),
    );
    DmabufLinearMetadata {
        vulkan,
        kms_layout,
        path,
    }
}

fn classify_modifier_observations(
    kms_advertised_modifiers: bool,
    observations: &[(u64, ScanoutMetadataSupport)],
) -> (Vec<u64>, ScanoutMetadataSupport) {
    use ScanoutMetadataSupport::{Supported, Unknown, Unsupported};

    if !kms_advertised_modifiers {
        return (Vec::new(), Unknown);
    }

    let mut modifiers = Vec::new();
    let mut saw_unknown = false;
    for &(modifier, support) in observations {
        match support {
            Supported if !modifiers.contains(&modifier) => modifiers.push(modifier),
            Supported | Unsupported => {}
            Unknown => saw_unknown = true,
        }
    }

    let support = if modifiers.is_empty() {
        if saw_unknown { Unknown } else { Unsupported }
    } else {
        Supported
    };
    (modifiers, support)
}

fn probe_kms_prime_metadata(
    drm: &crate::drm::Device,
    route: ScanoutRoute,
) -> (ScanoutMetadataSupport, ScanoutMetadataSupport) {
    match drm.get_driver_capability(DriverCapability::Prime) {
        Ok(bits) => (
            support_from_prime_bits(bits, DRM_PRIME_CAP_IMPORT),
            support_from_prime_bits(bits, DRM_PRIME_CAP_EXPORT),
        ),
        Err(error) => {
            log::warn!(
                "dma-buf metadata for {route:?}: DRM_CAP_PRIME query failed: {error}; \
                 import/export support remains unknown"
            );
            (
                ScanoutMetadataSupport::Unknown,
                ScanoutMetadataSupport::Unknown,
            )
        }
    }
}

fn probe_directional_modifiers(
    vk: &VkContext,
    kms_scanout_modifiers: &[u64],
    feature: vk::ExternalMemoryFeatureFlags,
) -> (Vec<u64>, ScanoutMetadataSupport) {
    if kms_scanout_modifiers.is_empty() {
        return classify_modifier_observations(false, &[]);
    }

    let observations = kms_scanout_modifiers
        .iter()
        .copied()
        .map(|modifier| {
            (
                modifier,
                probe_scanout_modifier_single_plane_feature(vk, modifier, feature),
            )
        })
        .collect::<Vec<_>>();
    classify_modifier_observations(true, &observations)
}

fn build_dmabuf_scanout_metadata(
    external_memory_fd: ScanoutMetadataSupport,
    prime_import: ScanoutMetadataSupport,
    prime_export: ScanoutMetadataSupport,
    output_owned_modifiers: (Vec<u64>, ScanoutMetadataSupport),
    renderer_owned_modifiers: (Vec<u64>, ScanoutMetadataSupport),
    output_owned_linear: (ScanoutMetadataSupport, KmsLinearLayout),
    renderer_owned_linear: (ScanoutMetadataSupport, KmsLinearLayout),
) -> DmabufScanoutMetadata {
    let (output_owned_modifiers, output_owned_modifier_support) = output_owned_modifiers;
    let (renderer_owned_modifiers, renderer_owned_modifier_support) = renderer_owned_modifiers;
    let output_owned_modifier_path =
        combine_required_metadata(prime_export, output_owned_modifier_support);
    let renderer_owned_modifier_path =
        combine_required_metadata(prime_import, renderer_owned_modifier_support);
    let output_owned_linear =
        build_linear_metadata(prime_export, output_owned_linear.0, output_owned_linear.1);
    let renderer_owned_linear = build_linear_metadata(
        prime_import,
        renderer_owned_linear.0,
        renderer_owned_linear.1,
    );
    DmabufScanoutMetadata {
        vulkan_external_memory_fd: external_memory_fd,
        output_owned: DmabufDirectionMetadata {
            kms_prime: prime_export,
            vulkan_modifiers: output_owned_modifier_support,
            modifiers: output_owned_modifiers,
            modifier_path: output_owned_modifier_path,
            linear: output_owned_linear,
        },
        renderer_owned: DmabufDirectionMetadata {
            kms_prime: prime_import,
            vulkan_modifiers: renderer_owned_modifier_support,
            modifiers: renderer_owned_modifiers,
            modifier_path: renderer_owned_modifier_path,
            linear: renderer_owned_linear,
        },
    }
}

fn classify_direction_metadata(
    direction: DmabufAllocationDirection,
    metadata: &DmabufDirectionMetadata,
    output_owned_gbm: ScanoutMetadataSupport,
) -> DmabufDirectionVerdict {
    use ScanoutMetadataSupport::{Supported, Unknown, Unsupported};

    let (prime_unsupported, prime_unknown, layout_incomplete, no_shared_layout) = match direction {
        DmabufAllocationDirection::OutputOwned => (
            DmabufDirectionIncompatibility::OutputOwnedKmsPrimeExportUnsupported,
            DmabufScanoutUncertainty::OutputOwnedKmsPrimeExportUnknown,
            DmabufScanoutUncertainty::OutputOwnedLayoutMetadataIncomplete,
            DmabufScanoutUncertainty::OutputOwnedNoAdvertisedSharedLayout,
        ),
        DmabufAllocationDirection::RendererOwned => (
            DmabufDirectionIncompatibility::RendererOwnedKmsPrimeImportUnsupported,
            DmabufScanoutUncertainty::RendererOwnedKmsPrimeImportUnknown,
            DmabufScanoutUncertainty::RendererOwnedLayoutMetadataIncomplete,
            DmabufScanoutUncertainty::RendererOwnedNoAdvertisedSharedLayout,
        ),
    };

    match metadata.kms_prime {
        Unsupported => return DmabufDirectionVerdict::Unsupported(prime_unsupported),
        Unknown => return DmabufDirectionVerdict::Unknown(prime_unknown),
        Supported => {}
    }

    if direction == DmabufAllocationDirection::OutputOwned
        && output_owned_gbm != ScanoutMetadataSupport::Supported
    {
        return DmabufDirectionVerdict::Unknown(
            DmabufScanoutUncertainty::OutputOwnedGbmUnavailable,
        );
    }

    if metadata.modifier_path == Supported || metadata.linear.path == Supported {
        DmabufDirectionVerdict::Supported
    } else if metadata.modifier_path == Unknown || metadata.linear.path == Unknown {
        DmabufDirectionVerdict::Unknown(layout_incomplete)
    } else {
        // Even complete advertised metadata cannot prove that the historical
        // runtime fallback will fail. Preserve original 07's broad attempt.
        DmabufDirectionVerdict::Unknown(no_shared_layout)
    }
}

fn classify_route_from_direction_verdicts(
    relationship: RenderKmsRelationship,
    external_memory_fd: ScanoutMetadataSupport,
    output_owned: DmabufDirectionVerdict,
    renderer_owned: DmabufDirectionVerdict,
) -> DmabufScanoutVerdict {
    use DmabufDirectionVerdict::{Supported, Unknown, Unsupported};

    match relationship {
        RenderKmsRelationship::Same => return DmabufScanoutVerdict::Compatible,
        RenderKmsRelationship::Unknown => {
            return DmabufScanoutVerdict::Unknown(vec![
                DmabufScanoutUncertainty::RenderKmsRelationshipUnknown,
            ]);
        }
        RenderKmsRelationship::Different => {}
    }

    match external_memory_fd {
        ScanoutMetadataSupport::Unsupported => {
            return DmabufScanoutVerdict::Incompatible(
                DmabufScanoutIncompatibility::VulkanExternalMemoryFdUnavailable,
            );
        }
        ScanoutMetadataSupport::Unknown => {
            return DmabufScanoutVerdict::Unknown(vec![
                DmabufScanoutUncertainty::VulkanExternalMemoryFdUnknown,
            ]);
        }
        ScanoutMetadataSupport::Supported => {}
    }

    match (output_owned, renderer_owned) {
        (Supported, _) | (_, Supported) => DmabufScanoutVerdict::Compatible,
        (Unsupported(output_owned), Unsupported(renderer_owned)) => {
            DmabufScanoutVerdict::Incompatible(
                DmabufScanoutIncompatibility::BothAllocationDirectionsUnavailable {
                    output_owned,
                    renderer_owned,
                },
            )
        }
        (output_owned, renderer_owned) => {
            let mut uncertainty = Vec::with_capacity(2);
            if let Unknown(reason) = output_owned {
                uncertainty.push(reason);
            }
            if let Unknown(reason) = renderer_owned {
                uncertainty.push(reason);
            }
            debug_assert!(
                !uncertainty.is_empty(),
                "all non-unknown direction pairs were handled above"
            );
            DmabufScanoutVerdict::Unknown(uncertainty)
        }
    }
}

fn classify_dmabuf_scanout_route(
    route: ScanoutRoute,
    metadata: &DmabufScanoutMetadata,
    output_owned_gbm: ScanoutMetadataSupport,
) -> DmabufScanoutVerdict {
    classify_route_from_direction_verdicts(
        route.relationship,
        metadata.vulkan_external_memory_fd,
        classify_direction_metadata(
            DmabufAllocationDirection::OutputOwned,
            &metadata.output_owned,
            output_owned_gbm,
        ),
        classify_direction_metadata(
            DmabufAllocationDirection::RendererOwned,
            &metadata.renderer_owned,
            output_owned_gbm,
        ),
    )
}

fn probe_dmabuf_scanout_metadata(
    vk: &VkContext,
    drm: &crate::drm::Device,
    route: ScanoutRoute,
    kms_scanout_modifiers: &[u64],
) -> DmabufScanoutMetadata {
    let external_memory_fd = if vk.external_memory_fd.is_some() {
        ScanoutMetadataSupport::Supported
    } else {
        ScanoutMetadataSupport::Unsupported
    };
    let (prime_import, prime_export) = probe_kms_prime_metadata(drm, route);
    let (output_owned_modifiers, output_owned_modifier_support) = probe_directional_modifiers(
        vk,
        kms_scanout_modifiers,
        vk::ExternalMemoryFeatureFlags::IMPORTABLE,
    );
    let (renderer_owned_modifiers, renderer_owned_modifier_support) = probe_directional_modifiers(
        vk,
        kms_scanout_modifiers,
        vk::ExternalMemoryFeatureFlags::EXPORTABLE,
    );
    let linear_layout = kms_linear_layout(kms_scanout_modifiers);
    let output_owned_linear = probe_scanout_modifier_single_plane_feature(
        vk,
        super::dri3::DRM_FORMAT_MOD_LINEAR,
        vk::ExternalMemoryFeatureFlags::IMPORTABLE,
    );
    let renderer_owned_linear =
        probe_scanout_linear_feature(vk, vk::ExternalMemoryFeatureFlags::EXPORTABLE);
    let metadata = build_dmabuf_scanout_metadata(
        external_memory_fd,
        prime_import,
        prime_export,
        (output_owned_modifiers, output_owned_modifier_support),
        (renderer_owned_modifiers, renderer_owned_modifier_support),
        (output_owned_linear, linear_layout),
        (renderer_owned_linear, linear_layout),
    );
    log::info!(
        "dma-buf metadata for {route:?}: output-owned KMS-export={:?} \
         Vulkan-import={:?} modifiers={} linear={:?}; renderer-owned \
         Vulkan-export={:?} KMS-import={:?} modifiers={} linear={:?} \
         (observation only)",
        metadata.output_owned.kms_prime,
        metadata.output_owned.vulkan_modifiers,
        format_modifiers(&metadata.output_owned.modifiers),
        metadata.output_owned.linear,
        metadata.renderer_owned.vulkan_modifiers,
        metadata.renderer_owned.kms_prime,
        format_modifiers(&metadata.renderer_owned.modifiers),
        metadata.renderer_owned.linear,
    );
    metadata
}

#[derive(Clone, Copy)]
enum AllocationCleanupPolicy {
    BestEffort,
    StrictDisposable,
}

fn release_drm_handles_strict<Fb, Gem>(
    framebuffer: &mut Option<Fb>,
    gem: &mut Option<Gem>,
    mut destroy_framebuffer: impl FnMut(Fb) -> io::Result<()>,
    mut close_gem: impl FnMut(Gem) -> io::Result<()>,
) -> io::Result<()>
where
    Fb: Copy,
    Gem: Copy,
{
    if let Some(handle) = *framebuffer {
        destroy_framebuffer(handle)?;
        *framebuffer = None;
    }
    if let Some(handle) = *gem {
        close_gem(handle)?;
        *gem = None;
    }
    Ok(())
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

impl PartialScanoutBoAllocation {
    fn release_drm_strict(&mut self) -> io::Result<()> {
        release_drm_handles_strict(
            &mut self.framebuffer,
            &mut self.gem,
            |framebuffer| {
                self.drm.destroy_framebuffer(framebuffer).map_err(|error| {
                    scanout_io_context(
                        format!("destroy partial disposable framebuffer {framebuffer:?}"),
                        error,
                    )
                })
            },
            |gem| {
                self.drm.close_buffer(gem).map_err(|error| {
                    scanout_io_context(
                        format!("close partial disposable GEM handle {gem:?}"),
                        error,
                    )
                })
            },
        )
    }

    fn release_drm_best_effort(&mut self) {
        if let Some(framebuffer) = self.framebuffer.take()
            && let Err(error) = self.drm.destroy_framebuffer(framebuffer)
        {
            log::warn!("drm destroy partial framebuffer failed: {error}");
        }
        if let Some(gem) = self.gem.take()
            && let Err(error) = self.drm.close_buffer(gem)
        {
            log::warn!("drm close partial GEM handle failed: {error}");
        }
    }

    fn destroy_backing(mut self) {
        unsafe {
            if let Some(mut transfer) = self.transfer.take() {
                destroy_transfer_resources(&self.vk, &mut transfer);
            }
            if let Some(image_view) = self.image_view.take() {
                self.vk.device.destroy_image_view(image_view, None);
            }
            if let Some(semaphore) = self.semaphore.take() {
                self.vk.device.destroy_semaphore(semaphore, None);
            }
        }
        destroy_scanout_image(&self.vk, self.image, self.memory);
    }

    fn rollback(
        mut self,
        original: io::Error,
        policy: AllocationCleanupPolicy,
    ) -> DisposableProbeError {
        match policy {
            AllocationCleanupPolicy::BestEffort => {
                self.release_drm_best_effort();
                self.destroy_backing();
                DisposableProbeError::from(original)
            }
            AllocationCleanupPolicy::StrictDisposable => match self.release_drm_strict() {
                Ok(()) => {
                    self.destroy_backing();
                    DisposableProbeError::from(original)
                }
                Err(cleanup) => {
                    let cleanup = io::Error::new(
                        cleanup.kind(),
                        format!(
                            "strict partial scanout rollback failed after {original}; retaining \
                            backing: {cleanup}"
                        ),
                    );
                    // `self` is the retention anchor: it owns the VkContext
                    // Arc, DRM Rc, optional GBM BO, and every raw Vulkan/KMS
                    // handle created so far. Forgetting the complete staged
                    // owner keeps backing alive until the isolated helper is
                    // killed; retaining only the raw handles would allow the
                    // VkDevice or GBM allocation to disappear underneath the
                    // parent-shared GEM/FB registration.
                    std::mem::forget(self);
                    DisposableProbeError::terminal_cleanup(cleanup)
                }
            },
        }
    }
}

impl ScanoutBo {
    /// Allocate one scanout bo: GBM-alloc or Vulkan-alloc dma-buf +
    /// DRM framebuffer registration. All steps must succeed; partial
    /// allocations are unwound on error so the returned `Err` leaves
    /// no resources leaked.
    pub fn allocate(
        vk: Arc<VkContext>,
        drm: Rc<crate::drm::Device>,
        gbm: Option<Rc<GbmDevice>>,
        width: u32,
        height: u32,
        scanout_modifiers: &[u64],
    ) -> io::Result<Self> {
        let output_owned_modifiers =
            scanout_modifier_candidates(&vk, scanout_modifiers, ScanoutOwnership::Output);
        let renderer_owned_modifiers =
            scanout_modifier_candidates(&vk, scanout_modifiers, ScanoutOwnership::Renderer);
        let plans = scanout_allocation_plans(
            &vk,
            &output_owned_modifiers,
            &renderer_owned_modifiers,
            width,
            gbm.is_some(),
        );
        let mut errors = Vec::new();

        for plan in plans {
            match Self::allocate_with_plan(
                Arc::clone(&vk),
                Rc::clone(&drm),
                gbm.as_ref().map(Rc::clone),
                width,
                height,
                plan,
            ) {
                Ok(bo) => {
                    log::info!(
                        "scanout bo: {} succeeded ({}x{}, pitch {})",
                        plan.describe(),
                        width,
                        height,
                        bo.pitch,
                    );
                    return Ok(bo);
                }
                Err(e) => {
                    // Log every rejected plan, not just the winner. Which
                    // plans a card silently falls THROUGH is the thing that
                    // distinguishes one NVIDIA generation from another (an
                    // Ampere box on driver 595 fails GBM-LINEAR and scans out
                    // block-linear tiled; a Pascal GTX 1050 takes LINEAR), and
                    // it was invisible in every user log until now because
                    // `errors` only ever surfaced when EVERY plan failed.
                    // INFO, not WARN: falling through is normal operation —
                    // the aggregate failure below is the actual error.
                    log::info!("scanout bo: {} failed: {e}", plan.describe());
                    errors.push(format!("{}: {e}", plan.describe()));
                }
            }
        }

        Err(io::Error::other(format!(
            "scanout allocation failed for every path: {}",
            errors.join("; ")
        )))
    }

    fn allocate_with_plan(
        vk: Arc<VkContext>,
        drm: Rc<crate::drm::Device>,
        gbm: Option<Rc<GbmDevice>>,
        width: u32,
        height: u32,
        plan: ScanoutAllocationPlan,
    ) -> io::Result<Self> {
        Self::allocate_with_plan_policy(
            vk,
            drm,
            gbm,
            width,
            height,
            plan,
            AllocationCleanupPolicy::BestEffort,
        )
        .map_err(DisposableProbeError::into_io_error)
    }

    fn allocate_with_plan_for_disposable_probe(
        vk: Arc<VkContext>,
        drm: Rc<crate::drm::Device>,
        gbm: Option<Rc<GbmDevice>>,
        width: u32,
        height: u32,
        plan: ScanoutAllocationPlan,
    ) -> Result<Self, DisposableProbeError> {
        Self::allocate_with_plan_policy(
            vk,
            drm,
            gbm,
            width,
            height,
            plan,
            AllocationCleanupPolicy::StrictDisposable,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn allocate_with_plan_policy(
        vk: Arc<VkContext>,
        drm: Rc<crate::drm::Device>,
        gbm: Option<Rc<GbmDevice>>,
        width: u32,
        height: u32,
        plan: ScanoutAllocationPlan,
        cleanup_policy: AllocationCleanupPolicy,
    ) -> Result<Self, DisposableProbeError> {
        // 1. Allocate the source dma-buf + import into Vulkan
        //    (GBM plans) OR allocate the `VkImage` and export as
        //    dma-buf (Vulkan-alloc plans).
        let img = match plan {
            ScanoutAllocationPlan::GbmModifier(modifier) => {
                let gbm_device = gbm.as_ref().ok_or_else(|| {
                    DisposableProbeError::from(io::Error::other(
                        "gbm plan requested but pool has no gbm_device",
                    ))
                })?;
                allocate_gbm_scanout_image(&vk, gbm_device, width, height, modifier).map_err(
                    |error| match error {
                        GbmScanoutError::Vk(result) => DisposableProbeError::from(
                            scanout_vk_error("gbm scanout Vulkan import", result),
                        ),
                        error => DisposableProbeError::from(io::Error::other(format!(
                            "gbm scanout image: {error}"
                        ))),
                    },
                )?
            }
            _ => allocate_vk_scanout_image(&vk, width, height, plan).map_err(|result| {
                DisposableProbeError::from(scanout_vk_error(
                    "Vulkan scanout image allocation",
                    result,
                ))
            })?,
        };
        let VkScanoutImage {
            image,
            memory,
            dmabuf,
            pitch,
            offset,
            modifier,
            gbm_bo,
        } = img;

        // 2. PRIME_FD_TO_HANDLE on the DRM device. Same DRM fd the
        //    GBM device (if any) was created on, so this returns the
        //    existing GEM handle rather than creating a new one when
        //    the source is a gbm_bo — kernel refcounts the underlying
        //    dma-buf either way.
        let gem_handle = match drm.prime_fd_to_buffer(dmabuf.as_fd()) {
            Ok(h) => h,
            Err(e) => {
                destroy_scanout_image(&vk, image, memory);
                return Err(DisposableProbeError::from(io::Error::other(format!(
                    "drm prime_fd_to_buffer: {e}"
                ))));
            }
        };
        // The GEM handle holds its own reference; close the dma-buf
        // fd we no longer need.
        drop(dmabuf);

        let mut partial = PartialScanoutBoAllocation {
            vk,
            drm,
            image,
            memory,
            image_view: None,
            semaphore: None,
            transfer: None,
            framebuffer: None,
            gem: Some(gem_handle),
            gbm_bo,
        };

        // 3. add_fb2. Modifier-backed paths must pass the MODIFIERS
        // flag even for DRM_FORMAT_MOD_LINEAR; the legacy fallback
        // deliberately keeps the old untagged shape.
        let fb_handle = match partial.drm.add_planar_framebuffer(
            &VkScanoutFb {
                gem_handle,
                width,
                height,
                pitch,
                offset,
                modifier,
            },
            addfb_flags_for_modifier(modifier),
        ) {
            Ok(h) => h,
            Err(e) => {
                return Err(
                    partial.rollback(io::Error::other(format!("drm add_fb: {e}")), cleanup_policy)
                );
            }
        };
        partial.framebuffer = Some(fb_handle);

        // 4. Long-lived export semaphore.
        let vk_semaphore = match create_export_semaphore(&partial.vk) {
            Ok(s) => s,
            Err(result) => {
                return Err(partial.rollback(
                    scanout_vk_error("Vulkan scanout semaphore", result),
                    cleanup_policy,
                ));
            }
        };
        partial.semaphore = Some(vk_semaphore);

        // 5. Per-bo transfer resources (always present now —
        //    every bo has a live VkImage to upload into).
        let vk_transfer = match allocate_transfer_resources(&partial.vk, width, height) {
            Ok(t) => t,
            Err(result) => {
                return Err(partial.rollback(
                    scanout_vk_error("Vulkan scanout transfer resources", result),
                    cleanup_policy,
                ));
            }
        };
        partial.transfer = Some(vk_transfer);

        // 6. Color image view used by the 4.1.3.4 composite pass
        //    `vkCmdBeginRendering` as the color attachment.
        let view_info = vk::ImageViewCreateInfo::default()
            .image(partial.image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(vk::Format::B8G8R8A8_UNORM)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            );
        let vk_image_view = match unsafe { partial.vk.device.create_image_view(&view_info, None) } {
            Ok(v) => v,
            Err(result) => {
                return Err(partial.rollback(
                    scanout_vk_error("Vulkan scanout image view", result),
                    cleanup_policy,
                ));
            }
        };
        partial.image_view = Some(vk_image_view);

        let PartialScanoutBoAllocation {
            vk,
            drm,
            image,
            memory,
            image_view,
            semaphore,
            transfer,
            framebuffer,
            gem,
            gbm_bo,
        } = partial;

        Ok(Self {
            state: BoState::default(),
            width,
            height,
            is_alien: false,
            pitch,
            last_gpu_render_ns: None,
            vk_image: image,
            vk_memory: memory,
            vk_image_view: image_view.expect("completed allocation has an image view"),
            vk_semaphore: semaphore.expect("completed allocation has a semaphore"),
            export_semaphore_reuse: ExportSemaphoreReuseState::Reusable,
            fb_handle: framebuffer,
            gem_handle: gem,
            vk_transfer: transfer.expect("completed allocation has transfer resources"),
            drm,
            vk,
            disarmed: false,
            gbm_bo,
        })
    }

    /// Submit a real color-attachment clear through this BO on a disposable
    /// Vulkan context.
    ///
    /// Import/export and framebuffer creation alone cannot prove that a
    /// foreign allocation is renderable. The probe follows the first-frame
    /// layout path, gives this submitted batch one bounded fence wait, and
    /// leaves the image in `GENERAL` for scanout validation.
    fn probe_renderer_access(&self, timeout_ns: u64) -> Result<(), DisposableProbeError> {
        let device = &self.vk.device;
        let command_buffer = self.vk_transfer.command_buffer;

        unsafe {
            device
                .reset_command_buffer(command_buffer, vk::CommandBufferResetFlags::empty())
                .map_err(|result| {
                    scanout_vk_error("reset disposable scanout probe command buffer", result)
                })?;
            let begin = vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            crate::vk_count!(begin_command_buffer);
            device
                .begin_command_buffer(command_buffer, &begin)
                .map_err(|result| {
                    scanout_vk_error("begin disposable scanout probe command buffer", result)
                })?;

            let to_color = [vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::TOP_OF_PIPE)
                .src_access_mask(vk::AccessFlags2::empty())
                .dst_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
                .dst_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .image(self.vk_image)
                .subresource_range(
                    vk::ImageSubresourceRange::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .level_count(1)
                        .layer_count(1),
                )];
            device.cmd_pipeline_barrier2(
                command_buffer,
                &vk::DependencyInfo::default().image_memory_barriers(&to_color),
            );

            let color_attachment = [vk::RenderingAttachmentInfo::default()
                .image_view(self.vk_image_view)
                .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .load_op(vk::AttachmentLoadOp::CLEAR)
                .store_op(vk::AttachmentStoreOp::STORE)
                .clear_value(vk::ClearValue {
                    color: vk::ClearColorValue {
                        float32: [0.0, 0.0, 0.0, 1.0],
                    },
                })];
            let rendering = vk::RenderingInfo::default()
                .render_area(vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: self.width,
                        height: self.height,
                    },
                })
                .layer_count(1)
                .color_attachments(&color_attachment);
            crate::vk_count!(cmd_begin_rendering);
            device.cmd_begin_rendering(command_buffer, &rendering);
            crate::vk_count!(cmd_end_rendering);
            device.cmd_end_rendering(command_buffer);

            let to_scanout = [vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
                .src_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                .dst_access_mask(vk::AccessFlags2::empty())
                .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .image(self.vk_image)
                .subresource_range(
                    vk::ImageSubresourceRange::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .level_count(1)
                        .layer_count(1),
                )];
            device.cmd_pipeline_barrier2(
                command_buffer,
                &vk::DependencyInfo::default().image_memory_barriers(&to_scanout),
            );

            crate::vk_count!(end_command_buffer);
            device
                .end_command_buffer(command_buffer)
                .map_err(|result| {
                    scanout_vk_error("end disposable scanout probe command buffer", result)
                })?;

            let fence = device
                .create_fence(&vk::FenceCreateInfo::default(), None)
                .map_err(|result| {
                    scanout_vk_error("create disposable scanout probe fence", result)
                })?;
            let mut fence = ProbeFence::new(device, fence);
            let command_buffers =
                [vk::CommandBufferSubmitInfo::default().command_buffer(command_buffer)];
            let submits = [vk::SubmitInfo2::default().command_buffer_infos(&command_buffers)];
            crate::vk_count!(queue_submit2);
            crate::vk_count!(submit_other);
            if let Err(result) =
                device.queue_submit2(self.vk.graphics_queue, &submits, fence.handle())
            {
                fence.abandon_pending();
                return Err(DisposableProbeError::quarantined(scanout_vk_error(
                    "submit disposable scanout rendering probe",
                    result,
                )));
            }

            wait_copy_free_probe_fence(&mut fence, timeout_ns)
        }
    }

    /// Export a SYNC_FD payload from this bo's signal semaphore. Call
    /// this after `vkQueueSubmit2` with `signalSemaphore = vk_semaphore`
    /// — it returns the freshly-payloaded fd to hand KMS as
    /// `IN_FENCE_FD`. `None` maps to the KMS `-1` no-fence sentinel.
    #[allow(dead_code)] // wired in by Task 2.5 (atomic-commit fence path).
    pub fn export_signaled_fd(&mut self) -> Result<Option<OwnedFd>, vk::Result> {
        self.export_semaphore_reuse.begin_post_submit_export();
        let ext = self.vk.external_semaphore_fd.clone();
        let info = vk::SemaphoreGetFdInfoKHR::default()
            .semaphore(self.vk_semaphore)
            .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
        let raw_fd = unsafe { ext.get_semaphore_fd(&info)? };
        let completion = super::optional_sync_fd_from_vk(raw_fd, "vkGetSemaphoreFdKHR(SYNC_FD)")?;
        self.export_semaphore_reuse.finish_successful_export();
        Ok(completion)
    }

    /// Replace a binary export semaphore whose submitted payload could not be
    /// exported. The caller must first prove the queue submission completed.
    pub(crate) fn rearm_export_semaphore_after_quiescence(&mut self) -> Result<(), vk::Result> {
        if !self.export_semaphore_reuse.needs_rearm() {
            return Ok(());
        }
        let replacement = create_export_semaphore(&self.vk)?;
        unsafe {
            self.vk.device.destroy_semaphore(self.vk_semaphore, None);
        }
        self.vk_semaphore = replacement;
        self.export_semaphore_reuse.finish_successful_export();
        Ok(())
    }

    /// Strictly remove helper-created KMS registrations while their backing is
    /// still alive. Handles are cleared only after the corresponding ioctl
    /// succeeds, so a caller can retain the complete object graph when cleanup
    /// fails instead of letting ordinary Drop free still-referenced backing.
    fn release_disposable_drm_resources(&mut self) -> io::Result<()> {
        release_drm_handles_strict(
            &mut self.fb_handle,
            &mut self.gem_handle,
            |framebuffer| {
                self.drm.destroy_framebuffer(framebuffer).map_err(|error| {
                    scanout_io_context(
                        format!("destroy disposable framebuffer {framebuffer:?}"),
                        error,
                    )
                })
            },
            |gem| {
                self.drm.close_buffer(gem).map_err(|error| {
                    scanout_io_context(format!("close disposable GEM handle {gem:?}"), error)
                })
            },
        )
    }

    /// Mark this BO as "let process-exit clean up." Subsequent
    /// `Drop` is a no-op. Idempotent.
    /// **Only valid at final process exit** — see field doc.
    pub fn disarm(&mut self) {
        // `Drop::drop` returning early does not suppress automatic field
        // drops. A GBM BO is an owning RAII handle, so leaving it in the
        // field would free output-owned storage while KMS may still retain
        // the framebuffer. Leak it deliberately with the other raw handles.
        leak_owned_backing(&mut self.gbm_bo);
        self.disarmed = true;
    }
}

impl Drop for ScanoutBo {
    fn drop(&mut self) {
        if self.disarmed {
            // Disarmed by shutdown-failed-disable path; let DRM-fd
            // close (process exit) reap GEM/FB and VkDevice teardown
            // releases the userspace handles. We are DELIBERATELY
            // leaking: this Drop deliberately skips vkDestroyImage,
            // vkFreeMemory, destroy_framebuffer, close_buffer(gem),
            // etc. — because touching them while KMS may still hold
            // the FB produces the `atomic remove_fb failed with -22`
            // warning that strands Wayland host sessions.
            log::warn!(
                "ScanoutBo disarmed (atomic disable_output failed); \
                 leaking FB/GEM/Vk to be reaped by DRM-fd close"
            );
            return;
        }
        // Defensive fence-fd cleanup. If the bo was Submitted /
        // Pending / OnScreen / Retiring at drop time (mid-flight
        // shutdown, or modeset that didn't go through the explicit
        // drain path), close any held fence fds so they don't leak.
        // Kernel-side sync_file refs survive our fd close until the
        // DRM device closes — atomic flip will still complete or
        // fail safely on its own.
        let released = self.state.transition_to_free_after_modeset_reset();
        if let Some(fd) = released.in_fence {
            // SAFETY: fd was inserted by transition_to_submitted; we
            // are the unique owner.
            drop(unsafe { OwnedFd::from_raw_fd(fd) });
        }
        if let Some(fd) = released.release_fence {
            drop(unsafe { OwnedFd::from_raw_fd(fd) });
        }

        // DRM-side teardown next: framebuffer references the GEM
        // handle; both must be released before we free the underlying
        // memory the dma-buf was exported from.
        if let Some(fb) = self.fb_handle.take()
            && let Err(e) = self.drm.destroy_framebuffer(fb)
        {
            log::warn!("drm destroy_framebuffer failed: {e}");
        }
        if let Some(h) = self.gem_handle.take()
            && let Err(e) = self.drm.close_buffer(h)
        {
            log::warn!("drm close_buffer (gem) failed: {e}");
        }

        unsafe {
            // Transfer resources (staging mapping must release before
            // memory is freed; command pool releases its CB).
            let t = std::mem::replace(
                &mut self.vk_transfer,
                TransferResources {
                    command_pool: vk::CommandPool::null(),
                    command_buffer: vk::CommandBuffer::null(),
                    staging_buffer: vk::Buffer::null(),
                    staging_memory: vk::DeviceMemory::null(),
                    staging_mapped: std::ptr::NonNull::dangling(),
                    staging_size: 0,
                    timestamp_pool: vk::QueryPool::null(),
                },
            );
            if t.command_pool != vk::CommandPool::null() {
                self.vk.device.unmap_memory(t.staging_memory);
                self.vk.device.destroy_buffer(t.staging_buffer, None);
                self.vk.device.free_memory(t.staging_memory, None);
                self.vk.device.destroy_command_pool(t.command_pool, None);
                if t.timestamp_pool != vk::QueryPool::null() {
                    self.vk.device.destroy_query_pool(t.timestamp_pool, None);
                }
            }

            // Image view before image, image before memory, then
            // semaphore.
            if self.vk_image_view != vk::ImageView::null() {
                self.vk.device.destroy_image_view(self.vk_image_view, None);
            }
            self.vk.device.destroy_image(self.vk_image, None);
            self.vk.device.free_memory(self.vk_memory, None);
            if self.vk_semaphore != vk::Semaphore::null() {
                self.vk.device.destroy_semaphore(self.vk_semaphore, None);
            }
        }
    }
}

/// Handle returned by [`ScanoutBoPool::register_alien`] — index into
/// `pool.bos` plus a generation token so a stale handle can't access
/// a re-used slot. Phase 4.2.4 design §3.3.2.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AlienBoHandle {
    pub index: u32,
}

impl ScanoutBoPool {
    fn release_disposable_drm_resources(&mut self) -> io::Result<()> {
        for (index, bo) in self.bos.iter_mut().enumerate() {
            bo.release_disposable_drm_resources().map_err(|error| {
                scanout_io_context(format!("disposable scanout BO {index}"), error)
            })?;
        }
        Ok(())
    }

    pub(crate) fn finish_disposable_probe(
        self,
        result: Result<(), DisposableProbeError>,
    ) -> Result<(), DisposableProbeError> {
        finish_disposable_probe_attempt(self, result)
    }

    /// Register a client-imported `DrawableImage` as an alien BO in
    /// the pool. The DrawableImage's underlying `VkDeviceMemory` is
    /// already allocated; we run the same `add_fb2` framebuffer
    /// registration the pool's owned BOs use, with the imported
    /// memory's GEM handle plus its DRM modifier.
    ///
    /// Phase 4.2.4 first-cut: returns `Err` because the
    /// VkDeviceMemory → GEM handle bridge is non-trivial and lives
    /// behind the live KMS Flip integration. The wire surface is in
    /// place so the dispatcher's choose_path Flip / DirectScanout
    /// branches plumb correctly; live registration arrives with the
    /// vng + Venus smoke for §5.5 hardware coverage.
    pub fn register_alien(
        &mut self,
        _drawable: &super::target::DrawableImage,
    ) -> io::Result<AlienBoHandle> {
        Err(io::Error::other(
            "ScanoutBoPool::register_alien: live KMS Flip integration not yet wired \
             (Phase 4.2.4 design §5.5 hardware coverage smoke)",
        ))
    }

    /// Drop a previously registered alien BO. Releases the framebuffer
    /// registration and removes the entry from `bos`. No-op if the
    /// handle's index is out of range.
    #[allow(dead_code)]
    pub fn unregister_alien(&mut self, _handle: AlienBoHandle) -> io::Result<()> {
        // Counterpart to register_alien — unimplemented for the same
        // reason. The plan's Task 29 test covers the round-trip once
        // both halves land.
        Ok(())
    }

    /// Reset every bo in the pool to `Free`, draining any in-flight
    /// fence fds. Used by the modeset / hot-config path (resize, mode
    /// change, hotplug — design §2 "Modeset / hot-config events").
    ///
    /// Order of operations:
    ///
    /// 1. `vkDeviceWaitIdle` on the device — heavy hammer that waits
    ///    for any in-flight `vkQueueSubmit2` work to complete. Cheap
    ///    in steady state (no work) and conservatively correct for
    ///    `Submitted`-phase bos which would otherwise have GPU work
    ///    racing the DRM tear-down.
    /// 2. For each bo, advance state machine to `Free` via
    ///    `transition_to_free_after_modeset_reset` and close any
    ///    returned fence fds.
    ///
    /// Pool dimensions stay the same; this is "reset state machine,
    /// keep the bos." Re-allocating bos with new dimensions is the
    /// caller's responsibility (drop the pool, allocate a fresh one
    /// with `ScanoutBoPool::allocate`).
    // Consumers: Drop, and `PlatformBackend::reset_scanout_bos_for_suspend`
    // (VT-switch suspend reclaims orphaned scanout BOs after master loss).
    pub fn drain_all_pending(&mut self, vk: &VkContext) {
        if let Err(e) = unsafe { vk.device.device_wait_idle() } {
            log::warn!("scanout pool drain: vkDeviceWaitIdle: {e}");
        }
        for bo in &mut self.bos {
            let released = bo.state.transition_to_free_after_modeset_reset();
            if let Some(fd) = released.in_fence {
                // SAFETY: fd inserted by transition_to_submitted; unique owner.
                drop(unsafe { OwnedFd::from_raw_fd(fd) });
            }
            if let Some(fd) = released.release_fence {
                drop(unsafe { OwnedFd::from_raw_fd(fd) });
            }
        }
    }

    /// True if any bo in this pool is in `BoPhase::Pending` —
    /// i.e. an atomic flip was accepted by KMS and the kernel
    /// hasn't yet emitted its pageflip-complete event for that
    /// flip. Used by the shutdown sequence to wait until KMS
    /// quiesces before issuing `disable_output`. Calling
    /// `disable_output` while a Pending bo exists is what
    /// produces the `atomic remove_fb failed with -22` kernel
    /// warning that leaves Wayland host compositors stranded.
    pub fn has_pending_pageflip(&self) -> bool {
        self.bos.iter().any(|b| b.state.phase == BoPhase::Pending)
    }

    /// Prove that every BO in this exact pool can complete a real Vulkan
    /// color-attachment write. Callers use only disposable contexts here, so
    /// a rejected foreign-memory submission cannot poison the live renderer.
    pub(crate) fn probe_renderer_access(self, timeout_ns: u64) -> Result<(), DisposableProbeError> {
        let result = (|| {
            for (index, bo) in self.bos.iter().enumerate() {
                let probe_started = Instant::now();
                bo.probe_renderer_access(timeout_ns).map_err(|error| {
                    error.with_context(format!("BO {index} disposable renderer-access probe"))
                })?;
                log::info!(
                    "copy-free rendering probe: bo={index} completed in {} ms; fence timeout {:?}",
                    probe_started.elapsed().as_millis(),
                    Duration::from_nanos(timeout_ns),
                );
            }
            Ok(())
        })();
        if result
            .as_ref()
            .is_err_and(DisposableProbeError::bypass_normal_teardown)
        {
            log::error!(
                "copy-free rendering probe timed out or left GPU completion uncertain; retaining \
                 the disposable pool without vkDeviceWaitIdle"
            );
        }
        finish_disposable_probe_attempt(self, result)
    }

    /// Allocate `count` bos for one output. Phase 4.1.2 uses 3 bos
    /// per pool (design §2). Opens the per-pool `gbm_device` on the
    /// KMS DRM fd so BOs can go through the GBM-first path. GBM
    /// device open failure is non-fatal — bos fall back to the
    /// Vulkan-first legacy allocator. On BO allocation failure the
    /// partial pool is dropped (each successful bo cleans up via
    /// `ScanoutBo::Drop`).
    pub(crate) fn allocate(
        vk: Arc<VkContext>,
        drm: Rc<crate::drm::Device>,
        route: ScanoutRoute,
        width: u32,
        height: u32,
        count: usize,
        scanout_modifiers: &[u64],
    ) -> io::Result<Self> {
        let metadata = probe_dmabuf_scanout_metadata(&vk, &drm, route, scanout_modifiers);
        let gbm_device = open_scanout_gbm_device(&drm);
        let output_owned_gbm = if gbm_device.is_some() {
            ScanoutMetadataSupport::Supported
        } else {
            ScanoutMetadataSupport::Unknown
        };
        let verdict = classify_dmabuf_scanout_route(route, &metadata, output_owned_gbm);
        log::info!(
            "dma-buf scanout observation for {route:?}: gbm={output_owned_gbm:?} \
             verdict={verdict:?} (diagnostic only)"
        );

        let plans =
            exact_scanout_allocation_plans(&vk, width, scanout_modifiers, gbm_device.is_some());
        let mut errors = Vec::new();
        for plan in plans {
            match Self::allocate_exact_observed(
                Arc::clone(&vk),
                Rc::clone(&drm),
                gbm_device.as_ref().map(Rc::clone),
                route,
                width,
                height,
                count,
                metadata.clone(),
                verdict.clone(),
                plan,
            ) {
                Ok(pool) => return Ok(pool),
                Err(error) if scanout_error_is_device_lost(&error) => return Err(error),
                Err(error) => {
                    log::info!("scanout pool: exact {} failed: {error}", plan.describe());
                    errors.push(format!("{}: {error}", plan.describe()));
                }
            }
        }

        Err(io::Error::other(format!(
            "scanout allocation failed for every exact full-pool plan: {}",
            errors.join("; ")
        )))
    }

    /// Enumerate exact full-pool candidates in the allocator's established
    /// total order. Output-owned candidates require Vulkan IMPORTABLE DMA-BUF
    /// modifiers; renderer-owned candidates require EXPORTABLE modifiers.
    #[must_use]
    pub(crate) fn exact_allocation_plans(
        vk: &VkContext,
        drm: &Rc<crate::drm::Device>,
        width: u32,
        scanout_modifiers: &[u64],
    ) -> Vec<ScanoutAllocationPlan> {
        let gbm_available = open_scanout_gbm_device(drm).is_some();
        exact_scanout_allocation_plans(vk, width, scanout_modifiers, gbm_available)
    }

    /// Allocate every BO with one exact representation. No BO may fall
    /// through to a different plan, so `ownership` and `allocation_plan`
    /// remain truthful for the complete pool.
    pub(crate) fn allocate_exact(
        vk: Arc<VkContext>,
        drm: Rc<crate::drm::Device>,
        route: ScanoutRoute,
        width: u32,
        height: u32,
        count: usize,
        scanout_modifiers: &[u64],
        plan: ScanoutAllocationPlan,
    ) -> io::Result<Self> {
        let metadata = probe_dmabuf_scanout_metadata(&vk, &drm, route, scanout_modifiers);
        let gbm_device = open_scanout_gbm_device(&drm);
        let output_owned_gbm = if gbm_device.is_some() {
            ScanoutMetadataSupport::Supported
        } else {
            ScanoutMetadataSupport::Unknown
        };
        let verdict = classify_dmabuf_scanout_route(route, &metadata, output_owned_gbm);
        log::info!(
            "dma-buf exact scanout observation for {route:?}: plan={} \
             gbm={output_owned_gbm:?} verdict={verdict:?} (diagnostic only)",
            plan.describe(),
        );
        Self::allocate_exact_observed(
            vk, drm, gbm_device, route, width, height, count, metadata, verdict, plan,
        )
    }

    /// Helper-only exact allocation. Any partially-created KMS registration is
    /// rolled back strictly; a cleanup failure is terminal and retains the
    /// backing rather than returning through live best-effort Drop.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn allocate_exact_for_disposable_probe(
        vk: Arc<VkContext>,
        drm: Rc<crate::drm::Device>,
        route: ScanoutRoute,
        width: u32,
        height: u32,
        count: usize,
        scanout_modifiers: &[u64],
        plan: ScanoutAllocationPlan,
    ) -> Result<Self, DisposableProbeError> {
        let metadata = probe_dmabuf_scanout_metadata(&vk, &drm, route, scanout_modifiers);
        let gbm_device = open_scanout_gbm_device(&drm);
        let output_owned_gbm = if gbm_device.is_some() {
            ScanoutMetadataSupport::Supported
        } else {
            ScanoutMetadataSupport::Unknown
        };
        let verdict = classify_dmabuf_scanout_route(route, &metadata, output_owned_gbm);
        log::info!(
            "disposable dma-buf exact scanout observation for {route:?}: plan={} \
             gbm={output_owned_gbm:?} verdict={verdict:?} (diagnostic only)",
            plan.describe(),
        );
        Self::allocate_exact_observed_with_policy(
            vk,
            drm,
            gbm_device,
            route,
            width,
            height,
            count,
            metadata,
            verdict,
            plan,
            AllocationCleanupPolicy::StrictDisposable,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn allocate_exact_observed(
        vk: Arc<VkContext>,
        drm: Rc<crate::drm::Device>,
        gbm_device: Option<Rc<GbmDevice>>,
        route: ScanoutRoute,
        width: u32,
        height: u32,
        count: usize,
        metadata: DmabufScanoutMetadata,
        verdict: DmabufScanoutVerdict,
        plan: ScanoutAllocationPlan,
    ) -> io::Result<Self> {
        Self::allocate_exact_observed_with_policy(
            vk,
            drm,
            gbm_device,
            route,
            width,
            height,
            count,
            metadata,
            verdict,
            plan,
            AllocationCleanupPolicy::BestEffort,
        )
        .map_err(DisposableProbeError::into_io_error)
    }

    #[allow(clippy::too_many_arguments)]
    fn allocate_exact_observed_with_policy(
        vk: Arc<VkContext>,
        drm: Rc<crate::drm::Device>,
        gbm_device: Option<Rc<GbmDevice>>,
        route: ScanoutRoute,
        width: u32,
        height: u32,
        count: usize,
        metadata: DmabufScanoutMetadata,
        verdict: DmabufScanoutVerdict,
        plan: ScanoutAllocationPlan,
        cleanup_policy: AllocationCleanupPolicy,
    ) -> Result<Self, DisposableProbeError> {
        if plan.ownership() == ScanoutOwnership::Output && gbm_device.is_none() {
            return Err(DisposableProbeError::from(io::Error::other(format!(
                "exact {} requires a GBM device on the KMS fd",
                plan.describe()
            ))));
        }

        let mut bos = Vec::with_capacity(count);
        for index in 0..count {
            let allocation = match cleanup_policy {
                AllocationCleanupPolicy::BestEffort => ScanoutBo::allocate_with_plan(
                    Arc::clone(&vk),
                    Rc::clone(&drm),
                    gbm_device.as_ref().map(Rc::clone),
                    width,
                    height,
                    plan,
                )
                .map_err(DisposableProbeError::from),
                AllocationCleanupPolicy::StrictDisposable => {
                    ScanoutBo::allocate_with_plan_for_disposable_probe(
                        Arc::clone(&vk),
                        Rc::clone(&drm),
                        gbm_device.as_ref().map(Rc::clone),
                        width,
                        height,
                        plan,
                    )
                }
            }
            .map_err(|error| {
                error.with_context(format!("exact {} BO {index} allocation", plan.describe()))
            });
            match allocation {
                Ok(bo) => bos.push(bo),
                Err(error)
                    if matches!(cleanup_policy, AllocationCleanupPolicy::StrictDisposable)
                        && !bos.is_empty() =>
                {
                    let partial_pool = Self {
                        bos,
                        width,
                        height,
                        route,
                        ownership: plan.ownership(),
                        allocation_plan: plan,
                        metadata,
                        verdict,
                        gbm_device,
                    };
                    return Err(partial_pool
                        .finish_disposable_probe(Err(error))
                        .expect_err("failed allocation cannot become a successful probe"));
                }
                Err(error) => return Err(error),
            }
        }
        Ok(Self {
            bos,
            width,
            height,
            route,
            ownership: plan.ownership(),
            allocation_plan: plan,
            metadata,
            verdict,
            gbm_device,
        })
    }
}

fn open_scanout_gbm_device(drm: &Rc<crate::drm::Device>) -> Option<Rc<GbmDevice>> {
    match GbmDevice::new(Rc::clone(drm)) {
        Ok(device) => Some(Rc::new(device)),
        Err(error) => {
            log::warn!(
                "gbm_create_device failed on KMS fd ({error}); scanout allocation will \
                 fall back to Vulkan-alloc, where NVIDIA/Intel take LINEAR \
                 (see scanout_prefers_linear) because Vulkan-allocated tiled \
                 scanout garbles there"
            );
            None
        }
    }
}

fn exact_scanout_allocation_plans(
    vk: &VkContext,
    width: u32,
    scanout_modifiers: &[u64],
    gbm_available: bool,
) -> Vec<ScanoutAllocationPlan> {
    let output_owned_modifiers =
        scanout_modifier_candidates(vk, scanout_modifiers, ScanoutOwnership::Output);
    let renderer_owned_modifiers =
        scanout_modifier_candidates(vk, scanout_modifiers, ScanoutOwnership::Renderer);
    scanout_allocation_plans(
        vk,
        &output_owned_modifiers,
        &renderer_owned_modifiers,
        width,
        gbm_available,
    )
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

impl DisposableProbeError {
    fn quarantined(source: io::Error) -> Self {
        Self {
            source,
            quarantine: true,
            abort_candidate_search: true,
        }
    }

    pub(crate) fn terminal_cleanup(source: io::Error) -> Self {
        Self::terminal_known_quiescent(source)
    }

    fn terminal_known_quiescent(source: io::Error) -> Self {
        Self {
            source,
            quarantine: false,
            abort_candidate_search: true,
        }
    }

    fn with_quarantine(mut self, quarantine: bool) -> Self {
        self.quarantine |= quarantine;
        self.abort_candidate_search |= quarantine;
        self
    }

    fn with_context(self, context: impl Into<String>) -> Self {
        Self {
            source: scanout_io_context(context, self.source),
            quarantine: self.quarantine,
            abort_candidate_search: self.abort_candidate_search,
        }
    }

    #[must_use]
    pub(crate) fn requires_quarantine(&self) -> bool {
        self.quarantine
    }

    /// Whether returning this failure through normal RAII could enter an
    /// unbounded device-wide idle or release backing still referenced by a
    /// failed strict DRM cleanup. Submission uncertainty and strict cleanup
    /// failure require quarantine; a pre-submit error and a completed content
    /// mismatch with successful cleanup are safe to tear down normally.
    #[must_use]
    fn bypass_normal_teardown(&self) -> bool {
        self.quarantine
    }

    #[must_use]
    pub(crate) fn abort_candidate_search(&self) -> bool {
        self.abort_candidate_search
    }

    #[must_use]
    #[cfg(test)]
    fn kind(&self) -> io::ErrorKind {
        self.source.kind()
    }

    #[must_use]
    pub(crate) fn as_io_error(&self) -> &io::Error {
        &self.source
    }

    pub(crate) fn into_io_error_with_context(self, context: impl Into<String>) -> io::Error {
        scanout_io_context(context, self.source)
    }

    fn into_io_error(self) -> io::Error {
        self.source
    }
}

impl From<io::Error> for DisposableProbeError {
    fn from(source: io::Error) -> Self {
        Self {
            source,
            quarantine: false,
            abort_candidate_search: false,
        }
    }
}

fn completed_probe_validation<T, E>(validation: Result<T, E>) -> Result<T, DisposableProbeError>
where
    DisposableProbeError: From<E>,
{
    // Once both submitted fences have signalled, content is authoritative.
    // Host-side validation time must never be reclassified as a GPU timeout.
    validation.map_err(DisposableProbeError::from)
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

fn finish_disposable_probe_attempt<A>(
    mut attempt: A,
    result: Result<(), DisposableProbeError>,
) -> Result<(), DisposableProbeError>
where
    A: DisposableProbeAttempt,
{
    if result
        .as_ref()
        .is_err_and(DisposableProbeError::bypass_normal_teardown)
    {
        attempt.retain_uncertain();
        return result;
    }

    attempt.mark_known_quiescent();
    if let Err(cleanup) = attempt.release_strict_drm_resources() {
        let prior = result
            .as_ref()
            .err()
            .map_or_else(|| "successful probe".to_string(), ToString::to_string);
        let cleanup = io::Error::new(
            cleanup.kind(),
            format!("strict disposable DRM cleanup failed after {prior}: {cleanup}"),
        );
        attempt.retain_uncertain();
        return Err(DisposableProbeError::quarantined(cleanup));
    }
    drop(attempt);
    result
}

impl DisposableProbeAttempt for ScanoutBoPool {
    fn mark_known_quiescent(&self) {
        for bo in &self.bos {
            bo.vk.mark_disposable_probe_quiescent();
        }
    }

    fn release_strict_drm_resources(&mut self) -> io::Result<()> {
        self.release_disposable_drm_resources()
    }
}

impl DisposableProbeAttempt for CopiedScanoutPool {
    fn mark_known_quiescent(&self) {
        self.mark_disposable_probe_quiescent();
    }

    fn release_strict_drm_resources(&mut self) -> io::Result<()> {
        self.destinations.release_disposable_drm_resources()
    }
}

struct CopiedDisposableProbeAttempt {
    pool: CopiedScanoutPool,
    pattern: Option<CopiedProbePatternPipeline>,
    render_digest: Option<ProbeDigestPipeline>,
    sink_digest: Option<ProbeDigestPipeline>,
}

impl DisposableProbeAttempt for CopiedDisposableProbeAttempt {
    fn mark_known_quiescent(&self) {
        // Mark both contexts before either pool or pipeline Drop observes the
        // policy. Their per-submit fences already prove all child resources are
        // idle, so both destructors may destroy directly.
        self.pool.mark_disposable_probe_quiescent();
    }

    fn release_strict_drm_resources(&mut self) -> io::Result<()> {
        self.pool.destinations.release_disposable_drm_resources()
    }
}

fn scanout_vk_error(operation: &'static str, result: vk::Result) -> io::Error {
    io::Error::other(ScanoutVkOperationError { operation, result })
}

fn copied_probe_digest_readback_error(
    operation: &'static str,
    result: vk::Result,
) -> DisposableProbeError {
    let source = scanout_vk_error(operation, result);
    if result == vk::Result::ERROR_DEVICE_LOST {
        // Preserve the structured source chain so the qualification layer can
        // promote this to DeviceLost rather than an indeterminate route.
        DisposableProbeError::from(source)
    } else {
        // Both fences are already complete, so ordinary teardown is safe, but
        // a host mapping/invalidation failure says nothing about route
        // compatibility. Stop candidate search as Indeterminate.
        DisposableProbeError::terminal_known_quiescent(source)
    }
}

#[cfg(test)]
pub(crate) fn device_lost_scanout_error_for_tests() -> io::Error {
    scanout_vk_error("test scanout operation", vk::Result::ERROR_DEVICE_LOST)
}

fn scanout_io_context(context: impl Into<String>, source: io::Error) -> io::Error {
    io::Error::new(
        source.kind(),
        ScanoutIoContext {
            context: context.into(),
            source,
        },
    )
}

fn copied_drawable_error(operation: &'static str, error: DrawableImageError) -> io::Error {
    match error {
        DrawableImageError::Vk(result) => scanout_vk_error(operation, result),
        error => io::Error::other(format!("{operation}: {error}")),
    }
}

fn copied_quiescence_result(
    operation: &'static str,
    result: Result<(), vk::Result>,
) -> io::Result<()> {
    result.map_err(|result| scanout_vk_error(operation, result))
}

fn close_modeset_released(released: ModesetReleased) {
    if let Some(fd) = released.in_fence {
        // SAFETY: the state machine returned unique ownership of this fd.
        drop(unsafe { OwnedFd::from_raw_fd(fd) });
    }
    if let Some(fd) = released.release_fence {
        // SAFETY: the state machine returned unique ownership of this fd.
        drop(unsafe { OwnedFd::from_raw_fd(fd) });
    }
}

fn leak_owned_backing<T>(slot: &mut Option<T>) {
    if let Some(backing) = slot.take() {
        std::mem::forget(backing);
    }
}

/// Whether an error from exact scanout allocation or a disposable rendering
/// probe contains `VK_ERROR_DEVICE_LOST` in its preserved source chain.
#[must_use]
pub(crate) fn scanout_error_is_device_lost(error: &io::Error) -> bool {
    fn contains_device_lost(error: &(dyn std::error::Error + 'static)) -> bool {
        if error
            .downcast_ref::<ScanoutVkOperationError>()
            .is_some_and(|error| error.result == vk::Result::ERROR_DEVICE_LOST)
        {
            return true;
        }

        // `io::Error` exposes its custom payload through `get_ref`; relying
        // only on `Error::source` loses that payload on some std versions.
        if let Some(io_error) = error.downcast_ref::<io::Error>()
            && let Some(inner) = io_error.get_ref()
        {
            return contains_device_lost(inner);
        }

        error.source().is_some_and(contains_device_lost)
    }

    contains_device_lost(error)
}

fn probe_teardown_wait_completed(result: Result<(), vk::Result>) -> bool {
    matches!(result, Ok(()) | Err(vk::Result::ERROR_DEVICE_LOST))
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

impl<'a> ProbeFence<'a> {
    fn new(device: &'a ash::Device, handle: vk::Fence) -> Self {
        Self { device, handle }
    }

    fn handle(&self) -> vk::Fence {
        self.handle
    }

    fn destroy_known_idle(&mut self) {
        if self.handle == vk::Fence::null() {
            return;
        }
        unsafe { self.device.destroy_fence(self.handle, None) };
        self.handle = vk::Fence::null();
    }

    /// Relinquish userspace ownership of a fence whose submission may still
    /// reference it. The aggregate disposable probe attempt keeps the owning
    /// device and every submitted child alive until process exit.
    fn abandon_pending(&mut self) {
        self.handle = vk::Fence::null();
    }
}

trait DisposableProbeFence {
    fn abandon(&mut self);
    fn destroy_idle(&mut self);
    fn wait_bounded(&mut self, timeout_ns: u64, operation: &'static str) -> io::Result<()>;
}

impl DisposableProbeFence for ProbeFence<'_> {
    fn abandon(&mut self) {
        self.abandon_pending();
    }

    fn destroy_idle(&mut self) {
        self.destroy_known_idle();
    }

    fn wait_bounded(&mut self, timeout_ns: u64, operation: &'static str) -> io::Result<()> {
        match unsafe {
            self.device
                .wait_for_fences(&[self.handle()], true, timeout_ns)
        } {
            Ok(()) => Ok(()),
            Err(vk::Result::TIMEOUT) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "{operation} timed out after {:?}",
                    Duration::from_nanos(timeout_ns),
                ),
            )),
            Err(result) => Err(scanout_vk_error(operation, result)),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PendingProbeSubmissions {
    None,
    Render,
    RenderAndSink,
}

/// Resolve both fence guards without ever waiting device-wide. Returns true
/// when at least one submission remains uncertain and the owning aggregate
/// attempt must also be retained.
#[must_use]
fn dispose_probe_fences_after_failure<R, S>(
    pending: PendingProbeSubmissions,
    render: &mut R,
    sink: &mut S,
) -> bool
where
    R: DisposableProbeFence,
    S: DisposableProbeFence,
{
    match pending {
        PendingProbeSubmissions::None => {
            render.destroy_idle();
            sink.destroy_idle();
            false
        }
        PendingProbeSubmissions::Render => {
            render.abandon();
            sink.destroy_idle();
            true
        }
        PendingProbeSubmissions::RenderAndSink => {
            render.abandon();
            sink.abandon();
            true
        }
    }
}

fn finish_pending_probe_failure<R, S>(
    pending: PendingProbeSubmissions,
    error: DisposableProbeError,
    render: &mut R,
    sink: &mut S,
) -> DisposableProbeError
where
    R: DisposableProbeFence,
    S: DisposableProbeFence,
{
    let quarantine = dispose_probe_fences_after_failure(pending, render, sink);
    error.with_quarantine(quarantine)
}

fn wait_copy_free_probe_fence<F>(fence: &mut F, timeout_ns: u64) -> Result<(), DisposableProbeError>
where
    F: DisposableProbeFence,
{
    match fence.wait_bounded(timeout_ns, "disposable scanout rendering probe") {
        Ok(()) => {
            fence.destroy_idle();
            Ok(())
        }
        Err(error) => {
            fence.abandon();
            Err(DisposableProbeError::quarantined(error))
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct CopiedProbeFenceWaitDurations {
    renderer: Duration,
    sink: Duration,
}

fn wait_copied_probe_fence_pair<R, S>(
    render: &mut R,
    sink: &mut S,
    timeout_ns: u64,
) -> Result<CopiedProbeFenceWaitDurations, DisposableProbeError>
where
    R: DisposableProbeFence,
    S: DisposableProbeFence,
{
    let renderer_started = Instant::now();
    if let Err(error) = render.wait_bounded(timeout_ns, "copied renderer probe") {
        return Err(finish_pending_probe_failure(
            PendingProbeSubmissions::RenderAndSink,
            DisposableProbeError::from(error),
            render,
            sink,
        ));
    }
    render.destroy_idle();
    let renderer_elapsed = renderer_started.elapsed();

    let sink_started = Instant::now();
    if let Err(error) = sink.wait_bounded(timeout_ns, "copied sink probe") {
        sink.abandon();
        return Err(DisposableProbeError::from(error).with_quarantine(true));
    }
    sink.destroy_idle();
    Ok(CopiedProbeFenceWaitDurations {
        renderer: renderer_elapsed,
        sink: sink_started.elapsed(),
    })
}

impl Drop for ProbeFence<'_> {
    fn drop(&mut self) {
        if self.handle == vk::Fence::null() {
            return;
        }
        let wait = unsafe { self.device.device_wait_idle() };
        if !probe_teardown_wait_completed(wait) {
            log::warn!(
                "disposable scanout probe: vkDeviceWaitIdle failed during teardown: {wait:?}; \
                 leaking the uncertain fence"
            );
            self.handle = vk::Fence::null();
            return;
        }
        unsafe { self.device.destroy_fence(self.handle, None) };
        self.handle = vk::Fence::null();
    }
}

fn create_probe_fence(vk: &VkContext) -> io::Result<vk::Fence> {
    unsafe {
        vk.device
            .create_fence(&vk::FenceCreateInfo::default(), None)
    }
    .map_err(|result| scanout_vk_error("create copied scanout probe fence", result))
}

fn submit_copied_source_probe(
    source: &mut CopiedRenderSource,
    pattern: &CopiedProbePatternPipeline,
    readback: CopiedProbeReadback<'_>,
    frame_token: u32,
    fence: vk::Fence,
) -> Result<Option<OwnedFd>, DisposableProbeError> {
    source.prepare_renderer_acquire()?;
    let transport_preparation = source.transport_preparation()?;
    let device = &source.render_vk.device;
    let command_buffer = source.transfer.command_buffer;
    unsafe {
        device
            .reset_command_buffer(command_buffer, vk::CommandBufferResetFlags::empty())
            .map_err(|result| {
                scanout_vk_error("reset copied renderer probe command buffer", result)
            })?;
        device
            .begin_command_buffer(
                command_buffer,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
            .map_err(|result| {
                scanout_vk_error("begin copied renderer probe command buffer", result)
            })?;

        let to_color = [vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::TOP_OF_PIPE)
            .src_access_mask(vk::AccessFlags2::empty())
            .dst_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
            .dst_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .image(source.image())
            .subresource_range(color_subresource_range())];
        device.cmd_pipeline_barrier2(
            command_buffer,
            &vk::DependencyInfo::default().image_memory_barriers(&to_color),
        );
        let attachments = [vk::RenderingAttachmentInfo::default()
            .image_view(source.image_view())
            .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .store_op(vk::AttachmentStoreOp::STORE)
            .clear_value(vk::ClearValue {
                color: vk::ClearColorValue {
                    float32: [0.0, 0.0, 0.0, 1.0],
                },
            })];
        device.cmd_begin_rendering(
            command_buffer,
            &vk::RenderingInfo::default()
                .render_area(vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: source.width(),
                        height: source.height(),
                    },
                })
                .layer_count(1)
                .color_attachments(&attachments),
        );
        pattern.record(command_buffer, source.width(), source.height(), frame_token);
        device.cmd_end_rendering(command_buffer);
        source.record_transport_copy(command_buffer, transport_preparation);
        source.record_probe_readback(command_buffer, readback);
        device
            .end_command_buffer(command_buffer)
            .map_err(|result| {
                scanout_vk_error("end copied renderer probe command buffer", result)
            })?;

        let waits = source.renderer_wait_semaphore().map(|semaphore| {
            [vk::SemaphoreSubmitInfo::default()
                .semaphore(semaphore)
                .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)]
        });
        let commands = [vk::CommandBufferSubmitInfo::default().command_buffer(command_buffer)];
        let signals = [vk::SemaphoreSubmitInfo::default()
            .semaphore(source.completion_semaphore)
            .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)];
        let mut submit = vk::SubmitInfo2::default()
            .command_buffer_infos(&commands)
            .signal_semaphore_infos(&signals);
        if let Some(waits) = waits.as_ref() {
            submit = submit.wait_semaphore_infos(waits);
        }
        let submits = [submit];
        crate::vk_count!(queue_submit2);
        crate::vk_count!(submit_other);
        device
            .queue_submit2(source.render_vk.graphics_queue, &submits, fence)
            .map_err(|result| {
                DisposableProbeError::quarantined(scanout_vk_error(
                    "submit copied renderer probe",
                    result,
                ))
            })?;
    }

    source.note_renderer_submit_succeeded();

    source.export_render_completion().map_err(|result| {
        DisposableProbeError::quarantined(scanout_vk_error(
            "export copied renderer probe completion",
            result,
        ))
    })
}

fn tight_bgra_len(width: u32, height: u32) -> io::Result<usize> {
    let bytes = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| io::Error::other("copied probe BGRA byte length overflow"))?;
    usize::try_from(bytes)
        .map_err(|_| io::Error::other("copied probe BGRA byte length exceeds usize"))
}

fn tight_mapped_bgra_bytes(
    transfer: &TransferResources,
    width: u32,
    height: u32,
) -> io::Result<&[u8]> {
    let len = tight_bgra_len(width, height)?;
    if transfer.staging_size < len as u64 {
        return Err(io::Error::other(format!(
            "copied probe staging buffer is too small: have {} bytes, need {len}",
            transfer.staging_size,
        )));
    }
    // SAFETY: `staging_mapped` points to `staging_size` live mapped bytes for
    // the lifetime of `transfer`; the checked slice is no larger than that
    // mapping. Callers read only after the corresponding probe fence signals.
    Ok(unsafe { std::slice::from_raw_parts(transfer.staging_mapped.as_ptr(), len) })
}

fn tight_bgra_buffer_image_copy(width: u32, height: u32) -> vk::BufferImageCopy {
    vk::BufferImageCopy::default()
        .buffer_offset(0)
        .buffer_row_length(0)
        .buffer_image_height(0)
        .image_subresource(color_subresource_layers())
        .image_extent(vk::Extent3D {
            width,
            height,
            depth: 1,
        })
}

fn probe_buffer_to_host_barrier(transfer: &TransferResources) -> vk::BufferMemoryBarrier2<'_> {
    vk::BufferMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::COPY)
        .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
        .dst_stage_mask(vk::PipelineStageFlags2::HOST)
        .dst_access_mask(vk::AccessFlags2::HOST_READ)
        .buffer(transfer.staging_buffer)
        .offset(0)
        .size(transfer.staging_size)
}

/// Stable FNV-1a digest used only for concise probe diagnostics. Successful
/// validation also compares every byte, so hash collisions cannot admit a
/// corrupt route.
fn copied_probe_hash(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn copied_probe_digest_hash(words: &[u32]) -> u64 {
    words.iter().fold(0xcbf2_9ce4_8422_2325, |hash, word| {
        (hash ^ u64::from(*word)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn copied_probe_marker_word(rgb: [u8; 3], frame_token: u32) -> u32 {
    u32::from_le_bytes(copied_probe_marker_bgra(rgb, frame_token))
}

fn validate_copied_probe_digest_fiducials(
    renderer: &[u32],
    bo_idx: usize,
    cycle: u32,
    frame_token: u32,
) -> io::Result<()> {
    let expected = [
        copied_probe_marker_word([241, 37, 83], frame_token),
        copied_probe_marker_word([29, 211, 71], frame_token),
        copied_probe_marker_word([47, 91, 233], frame_token),
        copied_probe_marker_word([223, 173, 19], frame_token),
    ];
    let actual = renderer.get(..expected.len()).ok_or_else(|| {
        io::Error::other(format!(
            "copied content probe BO {bo_idx} cycle {cycle} token {frame_token}: compact GPU \
             digest omitted its four corner fiducials"
        ))
    })?;
    if actual != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "copied content probe BO {bo_idx} cycle {cycle} token {frame_token}: compact GPU \
                 digest has corner BGRA words {actual:08x?}, expected {expected:08x?}"
            ),
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn verify_copied_probe_digests(
    renderer: &[u32],
    sink: &[u32],
    grid_width: u32,
    grid_height: u32,
    bo_idx: usize,
    cycle: u32,
    frame_token: u32,
) -> io::Result<u64> {
    if grid_width == 0 || grid_height == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "copied GPU digest grid must be non-empty",
        ));
    }
    let expected_words = usize::try_from(grid_width)
        .ok()
        .and_then(|width| {
            usize::try_from(grid_height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|blocks| blocks.checked_mul(4))
        .and_then(|digest_words| digest_words.checked_add(4))
        .ok_or_else(|| io::Error::other("copied GPU digest word count overflow"))?;
    if renderer.len() != expected_words || sink.len() != expected_words {
        return Err(io::Error::other(format!(
            "copied content probe BO {bo_idx} cycle {cycle} token {frame_token}: unexpected \
             compact GPU digest lengths renderer={} sink={} expected={expected_words}",
            renderer.len(),
            sink.len(),
        )));
    }

    let renderer_hash = copied_probe_digest_hash(renderer);
    let sink_hash = copied_probe_digest_hash(sink);
    if renderer == sink {
        log::debug!(
            "copied content probe compact GPU digest matched: BO {bo_idx} cycle {cycle} token \
             {frame_token} grid={grid_width}x{grid_height} words={expected_words} \
             hash=fnv1a64:{renderer_hash:016x}"
        );
        return Ok(renderer_hash);
    }

    let mismatch = renderer
        .iter()
        .zip(sink)
        .position(|(renderer, sink)| renderer != sink)
        .unwrap_or(0);
    let location = if mismatch < 4 {
        format!("corner={mismatch}")
    } else {
        let digest_word = mismatch - 4;
        let block = digest_word / 4;
        let lane = digest_word % 4;
        let grid_width = usize::try_from(grid_width)
            .map_err(|_| io::Error::other("copied GPU digest grid width exceeds usize"))?;
        format!(
            "block=({}, {}) lane={lane}",
            block % grid_width,
            block / grid_width,
        )
    };
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "copied content probe compact GPU digest mismatch: BO {bo_idx} cycle {cycle} token \
             {frame_token} grid={grid_width}x{grid_height} renderer_hash=fnv1a64:{renderer_hash:016x} \
             sink_hash=fnv1a64:{sink_hash:016x}; first difference at {location}"
        ),
    ))
}

fn validate_copied_probe_digest_freshness(
    previous_renderer: Option<&[u32]>,
    renderer: &[u32],
    bo_idx: usize,
    cycle: u32,
    frame_token: u32,
) -> io::Result<()> {
    if previous_renderer.is_some_and(|previous| previous == renderer) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "copied content probe stale compact GPU digest: BO {bo_idx} cycle {cycle} token \
                 {frame_token} repeated the prior renderer frame"
            ),
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_copied_probe_fiducials(
    renderer: &[u8],
    width: u32,
    height: u32,
    bo_idx: usize,
    cycle: u32,
    frame_token: u32,
) -> io::Result<()> {
    if width < 2 || height < 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "copied content probe requires an image at least 2x2",
        ));
    }
    let expected_len = tight_bgra_len(width, height)?;
    if renderer.len() != expected_len {
        return Err(io::Error::other(format!(
            "copied content probe BO {bo_idx} cycle {cycle} token {frame_token}: renderer \
             readback length {} does not match expected {expected_len}",
            renderer.len(),
        )));
    }

    // The fragment shader writes exact tokenized RGB corner fiducials.
    // Readback is tightly packed B8G8R8A8, so the byte order below is BGRA.
    // Besides making screenshots orientable, this ensures a failed/no-op draw
    // cannot pass merely because both devices copied the same uniform clear.
    let corners = [
        (0, 0, copied_probe_marker_bgra([241, 37, 83], frame_token)),
        (
            width - 1,
            0,
            copied_probe_marker_bgra([29, 211, 71], frame_token),
        ),
        (
            0,
            height - 1,
            copied_probe_marker_bgra([47, 91, 233], frame_token),
        ),
        (
            width - 1,
            height - 1,
            copied_probe_marker_bgra([223, 173, 19], frame_token),
        ),
    ];
    for (x, y, expected) in corners {
        let start = ((y as usize * width as usize) + x as usize) * 4;
        let actual = &renderer[start..start + 4];
        if actual != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "copied content probe BO {bo_idx} cycle {cycle} token {frame_token}: \
                     renderer fiducial at ({x},{y}) is BGRA {actual:?}, expected {expected:?}"
                ),
            ));
        }
    }
    Ok(())
}

fn copied_probe_marker_bgra(rgb: [u8; 3], frame_token: u32) -> [u8; 4] {
    let token = frame_token as u8;
    let rgb_mask = [token, token.wrapping_mul(17), token.wrapping_mul(31)];
    [
        rgb[2] ^ rgb_mask[2],
        rgb[1] ^ rgb_mask[1],
        rgb[0] ^ rgb_mask[0],
        255,
    ]
}

#[allow(clippy::too_many_arguments)]
fn verify_copied_probe_pixels(
    renderer: &[u8],
    sink: &[u8],
    width: u32,
    height: u32,
    bo_idx: usize,
    cycle: u32,
    frame_token: u32,
) -> io::Result<u64> {
    let expected_len = tight_bgra_len(width, height)?;
    if renderer.len() != expected_len || sink.len() != expected_len {
        return Err(io::Error::other(format!(
            "copied content probe BO {bo_idx} cycle {cycle} token {frame_token}: unexpected \
             readback lengths renderer={} sink={} expected={expected_len}",
            renderer.len(),
            sink.len(),
        )));
    }

    let renderer_hash = copied_probe_hash(renderer);
    let sink_hash = copied_probe_hash(sink);
    if renderer_hash == sink_hash && renderer == sink {
        log::debug!(
            "copied content probe matched: BO {bo_idx} cycle {cycle} token {frame_token} \
             {width}x{height} bytes={expected_len} hash=fnv1a64:{renderer_hash:016x}"
        );
        return Ok(renderer_hash);
    }

    let mismatch = renderer
        .iter()
        .zip(sink)
        .position(|(renderer, sink)| renderer != sink)
        .unwrap_or(0);
    let pixel = mismatch / 4;
    let x = pixel % width as usize;
    let y = pixel / width as usize;
    let channel = ["B", "G", "R", "A"][mismatch % 4];
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "copied content probe mismatch: BO {bo_idx} cycle {cycle} token {frame_token} \
             {width}x{height} renderer_hash=fnv1a64:{renderer_hash:016x} \
             sink_hash=fnv1a64:{sink_hash:016x}; first difference at ({x},{y}) channel={channel} \
             renderer={} sink={}",
            renderer[mismatch], sink[mismatch],
        ),
    ))
}

#[allow(clippy::too_many_arguments)]
fn validate_copied_probe_freshness(
    previous_renderer_hash: Option<u64>,
    renderer_hash: u64,
    bo_idx: usize,
    cycle: u32,
    frame_token: u32,
) -> io::Result<()> {
    // The shader tokenizes its corner fiducials as well as the radial field,
    // so even the smallest admitted 2x2 extent must change between cycles.
    if previous_renderer_hash == Some(renderer_hash) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "copied content probe BO {bo_idx} produced stale renderer pixels in cycle \
                 {cycle}: frame token {frame_token} repeated fnv1a64:{renderer_hash:016x}"
            ),
        ));
    }
    Ok(())
}

fn color_subresource_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1)
}

fn color_subresource_layers() -> vk::ImageSubresourceLayers {
    vk::ImageSubresourceLayers::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .layer_count(1)
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

impl ScanoutAllocationPlan {
    #[must_use]
    pub(crate) const fn ownership(self) -> ScanoutOwnership {
        match self {
            Self::GbmModifier(_) => ScanoutOwnership::Output,
            Self::DrmModifier(_)
            | Self::PaddedExplicitLinear { .. }
            | Self::ExplicitLinear
            | Self::LegacyLinear => ScanoutOwnership::Renderer,
        }
    }

    pub(crate) fn describe(self) -> String {
        match self {
            Self::GbmModifier(modifier) => format!("gbm-modifier=0x{modifier:x}"),
            Self::DrmModifier(modifier) => format!("modifier=0x{modifier:x}"),
            Self::PaddedExplicitLinear { row_pitch } => {
                format!("padded-explicit-linear(pitch={row_pitch})")
            }
            Self::ExplicitLinear => "explicit-linear".to_string(),
            Self::LegacyLinear => "legacy-linear".to_string(),
        }
    }
}

fn scanout_allocation_plans(
    vk: &VkContext,
    output_owned_modifier_candidates: &[u64],
    renderer_owned_modifier_candidates: &[u64],
    width: u32,
    gbm_available: bool,
) -> Vec<ScanoutAllocationPlan> {
    assemble_scanout_allocation_plans(
        vk.image_drm_format_modifier,
        scanout_prefers_linear(vk.driver_id),
        output_owned_modifier_candidates,
        renderer_owned_modifier_candidates,
        width,
        gbm_available,
    )
}

fn assemble_scanout_allocation_plans(
    image_drm_format_modifier: bool,
    prefer_linear: bool,
    output_owned_modifier_candidates: &[u64],
    renderer_owned_modifier_candidates: &[u64],
    width: u32,
    gbm_available: bool,
) -> Vec<ScanoutAllocationPlan> {
    let mut plans = Vec::new();
    // GBM-allocated modifiers go FIRST — that's the ecosystem-standard path and
    // the only one that produces correct tiled scanout on NVIDIA. Per-modifier
    // Vulkan-import gating is checked at allocation time (IMPORTABLE, not the
    // Vulkan-alloc EXPORTABLE gate) so unsupported entries fall through cleanly
    // to the next plan rather than being pruned here. LINEAR (padded/legacy)
    // remains as an automatic fallback below when GBM can't produce a scanout
    // BO (e.g. modifier-less Polaris → legacy-linear).
    if gbm_available && image_drm_format_modifier {
        plans.extend(
            output_owned_modifier_candidates
                .iter()
                .copied()
                .map(ScanoutAllocationPlan::GbmModifier),
        );
    }
    // On drivers that prefer LINEAR (NVIDIA/Intel — the tiled/block-linear
    // scanout path renders garbled there via Vulkan-alloc), a tight LINEAR
    // pitch that isn't 256-aligned is rejected by the display engine at atomic
    // commit (EINVAL → device lost). Keep LINEAR but force an aligned (padded)
    // pitch via an explicit DRM-modifier layout. Only meaningful when the
    // modifier extension is present (explicit-layout create needs it). See
    // [`SCANOUT_PITCH_ALIGN`].
    //
    // Deliberately the RAW driver policy, not [`resolve_prefer_linear`]: this
    // is a fallback plan, not a preference. Someone running
    // `YSERVER_SCANOUT_MODIFIER=tiled-first` on an unaligned-pitch NVIDIA
    // width (3440 ultrawide) must still keep the known-good padded-LINEAR
    // plan behind the tiled attempts, or a garbling tiled modifier leaves no
    // survivable path to a display.
    if image_drm_format_modifier && prefer_linear && !linear_scanout_stride_aligned(width) {
        plans.push(ScanoutAllocationPlan::PaddedExplicitLinear {
            row_pitch: padded_linear_pitch(width),
        });
    }
    if image_drm_format_modifier {
        plans.extend(
            renderer_owned_modifier_candidates
                .iter()
                .copied()
                .map(ScanoutAllocationPlan::DrmModifier),
        );
    }
    plans.push(ScanoutAllocationPlan::ExplicitLinear);
    plans.push(ScanoutAllocationPlan::LegacyLinear);
    plans
}

/// Whether scanout BO allocation should try `LINEAR` before the tiled
/// DRM modifiers (see [`order_scanout_modifier_candidates`]).
///
/// **Scope: the Vulkan-alloc plans only, in practice.** This policy predates
/// the GBM-first path (`5fdb56eb`, 2026-07-22). On NVIDIA the GBM-LINEAR plan
/// now fails with `EINVAL` before this ordering can matter, so NVIDIA runs
/// GBM block-linear tiled and reaches the plans this policy governs only when
/// `gbm_create_device` itself failed. Do NOT read a `prefer_linear=true` log
/// line as "this card is scanning out LINEAR" — check which plan *succeeded*.
/// See the module header for the measurements.
///
/// Driver-split policy, each entry HW-confirmed against a real dithered/
/// corrupted scanout **on the Vulkan-alloc path**:
/// - **NVIDIA proprietary** (GTX 1050/Pascal): a Vulkan-allocated
///   BLOCK_LINEAR_2D image produces a dithered display, for every gob-height
///   variant. The same modifier allocated through GBM is clean — the driver
///   applies a display-engine layout Vulkan-alloc doesn't reproduce.
/// - **Intel Mesa (ANV)** (Kaby Lake i5-7200U): the I915 Y_TILED modifier
///   (`0x0100000000000002`) selected first produces the same dithering.
///
/// AMD (RADV) is deliberately NOT in this set: RDNA4/gfx12 *requires* the
/// tiled modifier — a LINEAR scanout buffer corrupts there (issue #48) —
/// and RDNA2 scans out tiled fine. Other drivers default to tiled-first;
/// add them here only after a confirmed dithering report, not speculatively
/// (e.g. Asahi/M1 has not shown the problem and stays tiled-first).
fn scanout_prefers_linear(driver_id: vk::DriverId) -> bool {
    matches!(
        driver_id,
        vk::DriverId::NVIDIA_PROPRIETARY | vk::DriverId::INTEL_OPEN_SOURCE_MESA
    )
}

/// Byte alignment the KMS scanout pitch must satisfy on the display engines
/// that otherwise prefer LINEAR (NVIDIA/Intel). NVIDIA's display controller
/// requires a 256-byte-aligned scanout stride; a Vulkan `LINEAR` image has a
/// TIGHT pitch (`width * 4` bytes for B8G8R8A8), so at widths whose byte-pitch
/// isn't 256-aligned the LINEAR framebuffer is rejected at atomic commit
/// (`EINVAL` → BO invalidated → `ERROR_DEVICE_LOST` → respawn loop).
///
/// HW-confirmed **2026-07-20, before the GBM-first path** (`5fdb56eb`,
/// 2026-07-22): GTX 1050 @ 2560 wide → pitch 10240 = 256×40 (OK, scanned out
/// LINEAR); GTX 1060 @ 3440 ultrawide → tight pitch 13760 (mod 256 = 192,
/// rejected at atomic commit → device lost). Same driver — only the stride
/// alignment differed; both 2560 and 1920 (aligned) rendered clean via LINEAR
/// on the 1060. So when the tight LINEAR pitch is unaligned we keep LINEAR but
/// allocate it with an explicit padded (aligned) pitch — see
/// [`padded_linear_pitch`] / `ScanoutAllocationPlan::PaddedExplicitLinear`.
///
/// Two corrections since those measurements, both from the module header's
/// 2026-07-30 data:
///
/// 1. The old claim here that "the tiled (block-linear) modifier is NOT a
///    usable escape — yserver's tiled scanout renders garbled on NVIDIA" was
///    true only of the *Vulkan-alloc* tiled path. GBM-allocated block-linear
///    displays correctly on NVIDIA, including on that same GTX 1050.
/// 2. Consequently the 1050 no longer "scans out LINEAR" at all: GBM-LINEAR
///    fails with `EINVAL` and it runs GBM tiled.
///
/// 3. This plan is consequently UNREACHABLE whenever GBM works. The 1060 was
///    re-measured 2026-07-26 (yserver 1.3.0 `46439bc67d89`, XFCE, 3440x1440 on
///    HDMI-1) and took `gbm-modifier=0x3000000004fe015` at **pitch 13760** —
///    the very unaligned pitch this constant exists to avoid — for a healthy
///    91-second session with zero device-lost, EINVAL or respawn signatures.
///    The unaligned-pitch rejection is specific to a *LINEAR* framebuffer; a
///    block-linear one at the same width is fine.
///
/// So the padded-pitch plan now guards only the no-GBM fallback. It stays:
/// unreachable costs nothing, and `gbm_create_device` failing on an ultrawide
/// NVIDIA box without it costs a respawn loop.
const SCANOUT_PITCH_ALIGN: u32 = 256;
/// Scanout format is `B8G8R8A8_UNORM` → 4 bytes/pixel.
const SCANOUT_BYTES_PER_PIXEL: u32 = 4;

/// True if a tight `LINEAR` scanout buffer `width` px wide has a display-engine-
/// acceptable (256-byte-aligned) pitch. See [`SCANOUT_PITCH_ALIGN`].
fn linear_scanout_stride_aligned(width: u32) -> bool {
    width
        .checked_mul(SCANOUT_BYTES_PER_PIXEL)
        .is_some_and(|pitch| pitch.is_multiple_of(SCANOUT_PITCH_ALIGN))
}

/// Pad a tight LINEAR scanout pitch up to [`SCANOUT_PITCH_ALIGN`]. Used to give
/// the display engine an aligned stride at widths (e.g. 3440 ultrawide) whose
/// tight `width*4` pitch it would otherwise reject at atomic commit.
fn padded_linear_pitch(width: u32) -> u32 {
    let tight = width.saturating_mul(SCANOUT_BYTES_PER_PIXEL);
    tight
        .div_ceil(SCANOUT_PITCH_ALIGN)
        .saturating_mul(SCANOUT_PITCH_ALIGN)
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

impl ScanoutModifierOverride {
    fn describe(self) -> String {
        match self {
            Self::TiledFirst => "tiled-first".to_string(),
            Self::LinearFirst => "linear-first".to_string(),
            Self::First(modifier) => format!("0x{modifier:x}"),
        }
    }
}

/// Parse one `YSERVER_SCANOUT_MODIFIER` value. `None` for anything
/// unrecognised — a typo in a diagnostic env var must not keep the display
/// server from starting, so the caller warns and falls back to the driver
/// policy. Pure so the accepted spellings are unit-testable.
fn parse_scanout_modifier_override(raw: &str) -> Option<ScanoutModifierOverride> {
    let token = raw.trim();
    if token.is_empty() {
        return None;
    }
    match token.to_ascii_lowercase().replace('_', "-").as_str() {
        "tiled-first" => return Some(ScanoutModifierOverride::TiledFirst),
        "linear-first" => return Some(ScanoutModifierOverride::LinearFirst),
        _ => {}
    }
    // Modifier values are logged as `0x…` by `format_modifiers`, so accept
    // that spelling verbatim; also accept bare hex and `_` digit grouping.
    let hex = token
        .strip_prefix("0x")
        .or_else(|| token.strip_prefix("0X"))
        .unwrap_or(token);
    u64::from_str_radix(&hex.replace('_', ""), 16)
        .ok()
        .map(ScanoutModifierOverride::First)
}

/// The process-wide `YSERVER_SCANOUT_MODIFIER` setting. Read once: scanout
/// pools are re-allocated on every modeset, and re-warning per BO would bury
/// the log the override exists to produce.
fn scanout_modifier_override() -> Option<ScanoutModifierOverride> {
    static OVERRIDE: OnceLock<Option<ScanoutModifierOverride>> = OnceLock::new();
    *OVERRIDE.get_or_init(|| {
        let raw = std::env::var("YSERVER_SCANOUT_MODIFIER").ok()?;
        let parsed = parse_scanout_modifier_override(&raw);
        match parsed {
            Some(over) => log::warn!(
                "YSERVER_SCANOUT_MODIFIER={raw} — overriding the per-driver scanout \
                 modifier policy with {}. This is a diagnostic knob; a wrong choice \
                 shows up as a garbled or dithered display, not as an error.",
                over.describe()
            ),
            None => log::warn!(
                "YSERVER_SCANOUT_MODIFIER={raw} is not a recognised value \
                 (expected tiled-first, linear-first, or a 0x<hex> modifier) — \
                 ignoring it and keeping the per-driver policy"
            ),
        }
        parsed
    })
}

/// Whether to order LINEAR ahead of the tiled modifiers, combining the
/// per-driver policy with any [`ScanoutModifierOverride`].
///
/// `First(_)` deliberately leaves the policy alone: it pins ONE modifier at
/// the front, and the rest of the list stays in the order the driver policy
/// asks for, so a failed pin degrades to normal behaviour.
fn resolve_prefer_linear(driver_id: vk::DriverId, over: Option<ScanoutModifierOverride>) -> bool {
    match over {
        Some(ScanoutModifierOverride::TiledFirst) => false,
        Some(ScanoutModifierOverride::LinearFirst) => true,
        Some(ScanoutModifierOverride::First(_)) | None => scanout_prefers_linear(driver_id),
    }
}

/// Move `modifier` to the front of `candidates`, inserting it if absent.
///
/// Absent is legal on purpose: the GBM plan checks Vulkan importability at
/// allocation time and falls through to the next plan when it fails (see
/// [`scanout_allocation_plans`]), so pinning a modifier the Vulkan side did
/// not advertise is a survivable experiment — and one worth running, since
/// GBM can allocate layouts Vulkan declines to export.
fn hoist_modifier_first(candidates: &mut Vec<u64>, modifier: u64) {
    candidates.retain(|&m| m != modifier);
    candidates.insert(0, modifier);
}

fn scanout_modifier_candidates(
    vk: &VkContext,
    kms_scanout_modifiers: &[u64],
    ownership: ScanoutOwnership,
) -> Vec<u64> {
    if kms_scanout_modifiers.is_empty() {
        return Vec::new();
    }

    // Probe each KMS candidate in the allocation direction with the scanout
    // image's actual usage (color attachment and transfers, never sampled).
    // `dri3::supported_modifiers` is intentionally import-only, so using it as
    // a common pre-filter here would silently discard export-only modifiers
    // from renderer-owned plans.
    let vulkan = kms_scanout_modifiers
        .iter()
        .copied()
        .filter(|&modifier| match ownership {
            ScanoutOwnership::Output => scanout_modifier_is_single_plane_importable(vk, modifier),
            ScanoutOwnership::Renderer => scanout_modifier_is_single_plane_exportable(vk, modifier),
        })
        .collect::<Vec<_>>();
    let over = scanout_modifier_override();
    let prefer_linear = resolve_prefer_linear(vk.driver_id, over);
    let mut candidates =
        order_scanout_modifier_candidates(kms_scanout_modifiers, &vulkan, prefer_linear, |_| true);
    if let Some(ScanoutModifierOverride::First(modifier)) = over {
        if !candidates.contains(&modifier) {
            log::warn!(
                "YSERVER_SCANOUT_MODIFIER pins 0x{modifier:x}, which is not in the \
                 KMS/Vulkan intersection — trying it first anyway (GBM may still \
                 allocate it); allocation falls through to the normal order if it fails"
            );
        }
        hoist_modifier_first(&mut candidates, modifier);
    }
    // Diagnostic for scanout-corruption reports (issue #48): show what
    // the plane offered vs. what survived the Vulkan/exportable filter,
    // so a card that simply has no tiled scanout modifier on offer is
    // distinguishable from one whose tiled modifier we rejected. `override`
    // is included so a log captured during a triage round can't be mistaken
    // for the shipped policy's behaviour.
    log::info!(
        "scanout modifier select: ownership={ownership:?} kms_plane={} vulkan_supports={} \
         prefer_linear={prefer_linear} override={} -> candidates={}",
        format_modifiers(kms_scanout_modifiers),
        format_modifiers(&vulkan),
        over.map_or_else(|| "none".to_string(), ScanoutModifierOverride::describe),
        format_modifiers(&candidates),
    );
    candidates
}

fn format_modifiers(modifiers: &[u64]) -> String {
    if modifiers.is_empty() {
        return "[]".to_string();
    }
    let joined = modifiers
        .iter()
        .map(|m| format!("0x{m:x}"))
        .collect::<Vec<_>>()
        .join(",");
    format!("[{joined}]")
}

/// Order the KMS/Vulkan modifier intersection into the sequence the
/// allocator tries.
///
/// Default (`prefer_linear = false`): **tiled modifiers first, `LINEAR` last.**
/// This fixes corruption on RDNA4/gfx12 (RX 9070 XT, issue #48) where a
/// Vulkan-rendered linear scanout buffer produces horizontal tiling
/// artifacts. RADV on gfx8/Polaris doesn't expose
/// `VK_EXT_image_drm_format_modifier`, so those cards never reach this
/// path and allocation falls through to the untagged-linear plan.
///
/// `prefer_linear = true`: **LINEAR first, tiled as fallback.**
/// Used for `NVIDIA_PROPRIETARY` where a *Vulkan-allocated* BLOCK_LINEAR_2D
/// image produces a dithered/scrambled display on Pascal hardware (GTX 1050,
/// GP107) even though allocation and KMS import succeed.
///
/// Note this does NOT mirror what GBM does — the opposite is true, and the
/// earlier claim here that "GBM implicitly selects LINEAR for scanout on
/// those cards" was wrong. NVIDIA's GBM *refuses* LINEAR for a
/// `RENDERING|SCANOUT` BO (`EINVAL`), so on the GBM path this ordering only
/// costs one guaranteed-failed attempt before a tiled variant wins. See
/// [`scanout_prefers_linear`] and the module header.
///
/// Pure (no Vulkan calls of its own) so the ordering policy is unit
/// testable; `supports_direction` is IMPORTABLE for output ownership and
/// EXPORTABLE for renderer ownership.
fn order_scanout_modifier_candidates(
    kms_scanout_modifiers: &[u64],
    vulkan_supported: &[u64],
    prefer_linear: bool,
    supports_direction: impl Fn(u64) -> bool,
) -> Vec<u64> {
    let mut candidates = Vec::new();

    // When LINEAR is preferred (NVIDIA), add it first if both sides advertise it.
    if prefer_linear
        && kms_scanout_modifiers.contains(&super::dri3::DRM_FORMAT_MOD_LINEAR)
        && vulkan_supported.contains(&super::dri3::DRM_FORMAT_MOD_LINEAR)
        && supports_direction(super::dri3::DRM_FORMAT_MOD_LINEAR)
    {
        candidates.push(super::dri3::DRM_FORMAT_MOD_LINEAR);
    }

    // Non-LINEAR modifiers in KMS-advertised order.
    for &modifier in kms_scanout_modifiers {
        if modifier == super::dri3::DRM_FORMAT_MOD_LINEAR {
            continue;
        }
        if vulkan_supported.contains(&modifier)
            && supports_direction(modifier)
            && !candidates.contains(&modifier)
        {
            candidates.push(modifier);
        }
    }

    // When tiled is preferred (default), LINEAR comes last.
    if !prefer_linear
        && kms_scanout_modifiers.contains(&super::dri3::DRM_FORMAT_MOD_LINEAR)
        && vulkan_supported.contains(&super::dri3::DRM_FORMAT_MOD_LINEAR)
        && supports_direction(super::dri3::DRM_FORMAT_MOD_LINEAR)
        && !candidates.contains(&super::dri3::DRM_FORMAT_MOD_LINEAR)
    {
        candidates.push(super::dri3::DRM_FORMAT_MOD_LINEAR);
    }

    candidates
}

fn scanout_modifier_is_single_plane_importable(vk: &VkContext, modifier: u64) -> bool {
    scanout_modifier_single_plane_supports_feature(
        vk,
        modifier,
        vk::ExternalMemoryFeatureFlags::IMPORTABLE,
    )
}

fn scanout_modifier_is_single_plane_exportable(vk: &VkContext, modifier: u64) -> bool {
    scanout_modifier_single_plane_supports_feature(
        vk,
        modifier,
        vk::ExternalMemoryFeatureFlags::EXPORTABLE,
    )
}

fn scanout_modifier_single_plane_supports_feature(
    vk: &VkContext,
    modifier: u64,
    feature: vk::ExternalMemoryFeatureFlags,
) -> bool {
    modifier_single_plane_supports_feature(vk, modifier, scanout_image_usage(), feature)
}

fn modifier_single_plane_supports_feature(
    vk: &VkContext,
    modifier: u64,
    usage: vk::ImageUsageFlags,
    feature: vk::ExternalMemoryFeatureFlags,
) -> bool {
    use std::ffi::c_void;

    if !vk.image_drm_format_modifier || vk.external_memory_fd.is_none() {
        return false;
    }

    let mut modifier_info = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
        .drm_format_modifier(modifier)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let mut external_info = vk::PhysicalDeviceExternalImageFormatInfo::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    external_info.p_next = std::ptr::from_mut(&mut modifier_info).cast::<c_void>();

    let mut format_info = vk::PhysicalDeviceImageFormatInfo2::default()
        .format(vk::Format::B8G8R8A8_UNORM)
        .ty(vk::ImageType::TYPE_2D)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(usage);
    format_info.p_next = std::ptr::from_mut(&mut external_info).cast::<c_void>();

    let mut external_props = vk::ExternalImageFormatProperties::default();
    let mut props2 = vk::ImageFormatProperties2::default().push_next(&mut external_props);
    if unsafe {
        vk.instance.get_physical_device_image_format_properties2(
            vk.physical_device,
            &format_info,
            &mut props2,
        )
    }
    .is_err()
    {
        return false;
    }

    external_props
        .external_memory_properties
        .external_memory_features
        .contains(feature)
        && external_props
            .external_memory_properties
            .compatible_handle_types
            .contains(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
        && drm_modifier_plane_count(vk, modifier) == Some(1)
}

fn advertised_drm_modifiers(vk: &VkContext) -> Vec<u64> {
    if !vk.image_drm_format_modifier {
        return Vec::new();
    }

    let modifier_count = {
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
        let mut format_props = vk::FormatProperties2::default().push_next(&mut list);
        unsafe {
            vk.instance.get_physical_device_format_properties2(
                vk.physical_device,
                vk::Format::B8G8R8A8_UNORM,
                &mut format_props,
            );
        }
        list.drm_format_modifier_count
    };
    if modifier_count == 0 {
        return Vec::new();
    }

    let mut props_storage =
        vec![vk::DrmFormatModifierPropertiesEXT::default(); modifier_count as usize];
    let mut list = vk::DrmFormatModifierPropertiesListEXT::default()
        .drm_format_modifier_properties(&mut props_storage);
    let mut format_props = vk::FormatProperties2::default().push_next(&mut list);
    unsafe {
        vk.instance.get_physical_device_format_properties2(
            vk.physical_device,
            vk::Format::B8G8R8A8_UNORM,
            &mut format_props,
        );
    }

    let entries = list.drm_format_modifier_count as usize;
    let mut modifiers = Vec::new();
    for property in props_storage.iter().take(entries) {
        if !modifiers.contains(&property.drm_format_modifier) {
            modifiers.push(property.drm_format_modifier);
        }
    }
    modifiers
}

fn order_copied_source_plans(
    renderer_modifiers: &[u64],
    mut supports_pair: impl FnMut(u64) -> bool,
) -> Vec<CopiedSourcePlan> {
    let linear = super::dri3::DRM_FORMAT_MOD_LINEAR;
    let mut plans: Vec<CopiedSourcePlan> = Vec::new();

    // Native modifiers retain renderer A's advertised order. Modifier 0 is
    // deliberately excluded from this tier even if the driver lists it amid
    // native layouts.
    for &modifier in renderer_modifiers {
        if modifier != linear
            && !plans.iter().any(|plan| plan.modifier() == modifier)
            && supports_pair(modifier)
        {
            plans.push(CopiedSourcePlan::DrmModifier(modifier));
        }
    }

    // Query explicit modifier 0 independently and append it exactly once.
    // This keeps LINEAR out of the native ordering regardless of where the
    // driver places it in the advertised list.
    if supports_pair(linear) {
        plans.push(CopiedSourcePlan::DrmModifier(linear));
    }
    plans
}

fn exact_copied_source_plans(render_vk: &VkContext, sink_vk: &VkContext) -> Vec<CopiedSourcePlan> {
    let advertised = advertised_drm_modifiers(render_vk);
    let plans = order_copied_source_plans(&advertised, |modifier| {
        modifier_single_plane_supports_feature(
            render_vk,
            modifier,
            COPIED_TRANSPORT_IMAGE_USAGE,
            vk::ExternalMemoryFeatureFlags::EXPORTABLE,
        ) && modifier_single_plane_supports_feature(
            sink_vk,
            modifier,
            COPIED_SINK_IMPORT_USAGE,
            vk::ExternalMemoryFeatureFlags::IMPORTABLE,
        )
    });
    let candidates = plans.iter().map(|plan| plan.modifier()).collect::<Vec<_>>();
    log::info!(
        "copied transport modifier select: renderer_advertised={} -> native_then_linear={}",
        format_modifiers(&advertised),
        format_modifiers(&candidates),
    );
    plans
}

fn assemble_copied_scanout_plans(
    destinations: &[ScanoutAllocationPlan],
    sources: &[CopiedSourcePlan],
) -> Vec<CopiedScanoutPlan> {
    let mut plans = Vec::new();
    for linear_tier in [false, true] {
        for &destination in destinations {
            for &source in sources
                .iter()
                .filter(|source| source.is_linear() == linear_tier)
            {
                plans.push(CopiedScanoutPlan {
                    source,
                    destination,
                });
            }
        }
    }
    plans
}

fn validate_copied_route_pair(
    route: ScanoutRoute,
    destination_route: ScanoutRoute,
) -> io::Result<()> {
    if route.kms_device_key != destination_route.kms_device_key {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "copied outer and destination routes name different KMS devices",
        ));
    }
    if destination_route.relationship != RenderKmsRelationship::Same {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "copied destination route must be sink-local",
        ));
    }
    if route.render_device_id == destination_route.render_device_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "copied source and destination routes name the same renderer",
        ));
    }
    Ok(())
}

/// Observe one explicit-modifier external-memory feature without collapsing
/// failed or missing metadata into `Unsupported`.
///
/// This intentionally does not replace
/// [`scanout_modifier_single_plane_supports_feature`]: the latter is part of
/// the established allocator's candidate construction. Keeping the runtime
/// predicate separate guarantees that adding diagnostics cannot prune or
/// reorder any allocation plan.
fn probe_scanout_modifier_single_plane_feature(
    vk: &VkContext,
    modifier: u64,
    feature: vk::ExternalMemoryFeatureFlags,
) -> ScanoutMetadataSupport {
    use ScanoutMetadataSupport::{Supported, Unknown, Unsupported};
    use std::ffi::c_void;

    if !vk.image_drm_format_modifier || vk.external_memory_fd.is_none() {
        return Unsupported;
    }

    let mut modifier_info = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
        .drm_format_modifier(modifier)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let mut external_info = vk::PhysicalDeviceExternalImageFormatInfo::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    external_info.p_next = std::ptr::from_mut(&mut modifier_info).cast::<c_void>();

    let mut format_info = vk::PhysicalDeviceImageFormatInfo2::default()
        .format(vk::Format::B8G8R8A8_UNORM)
        .ty(vk::ImageType::TYPE_2D)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(scanout_image_usage());
    format_info.p_next = std::ptr::from_mut(&mut external_info).cast::<c_void>();

    let mut external_props = vk::ExternalImageFormatProperties::default();
    let mut props2 = vk::ImageFormatProperties2::default().push_next(&mut external_props);
    if let Err(error) = unsafe {
        vk.instance.get_physical_device_image_format_properties2(
            vk.physical_device,
            &format_info,
            &mut props2,
        )
    } {
        return if error == vk::Result::ERROR_FORMAT_NOT_SUPPORTED {
            Unsupported
        } else {
            Unknown
        };
    }

    let external = external_props.external_memory_properties;
    if !external.external_memory_features.contains(feature)
        || !external
            .compatible_handle_types
            .contains(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
    {
        return Unsupported;
    }

    match drm_modifier_plane_count(vk, modifier) {
        Some(1) => Supported,
        Some(_) => Unsupported,
        None => Unknown,
    }
}

/// Observe Vulkan's DMA-BUF support for a plain
/// `VK_IMAGE_TILING_LINEAR` scanout image. This is the renderer-owned
/// ExplicitLinear/LegacyLinear evidence; padded explicit-linear remains part
/// of the modifier observation above.
fn probe_scanout_linear_feature(
    vk: &VkContext,
    feature: vk::ExternalMemoryFeatureFlags,
) -> ScanoutMetadataSupport {
    use ScanoutMetadataSupport::{Supported, Unknown, Unsupported};

    if vk.external_memory_fd.is_none() {
        return Unsupported;
    }

    let mut external_info = vk::PhysicalDeviceExternalImageFormatInfo::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let format_info = vk::PhysicalDeviceImageFormatInfo2::default()
        .format(vk::Format::B8G8R8A8_UNORM)
        .ty(vk::ImageType::TYPE_2D)
        .tiling(vk::ImageTiling::LINEAR)
        .usage(scanout_image_usage())
        .push_next(&mut external_info);
    let mut external_props = vk::ExternalImageFormatProperties::default();
    let mut props2 = vk::ImageFormatProperties2::default().push_next(&mut external_props);
    if let Err(error) = unsafe {
        vk.instance.get_physical_device_image_format_properties2(
            vk.physical_device,
            &format_info,
            &mut props2,
        )
    } {
        return if error == vk::Result::ERROR_FORMAT_NOT_SUPPORTED {
            Unsupported
        } else {
            Unknown
        };
    }

    let external = external_props.external_memory_properties;
    if external.external_memory_features.contains(feature)
        && external
            .compatible_handle_types
            .contains(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
    {
        Supported
    } else {
        Unsupported
    }
}

fn drm_modifier_plane_count(vk: &VkContext, modifier: u64) -> Option<u32> {
    let modifier_count = {
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
        let mut format_props = vk::FormatProperties2::default().push_next(&mut list);
        unsafe {
            vk.instance.get_physical_device_format_properties2(
                vk.physical_device,
                vk::Format::B8G8R8A8_UNORM,
                &mut format_props,
            );
        }
        list.drm_format_modifier_count
    };
    if modifier_count == 0 {
        return None;
    }

    let mut props_storage =
        vec![vk::DrmFormatModifierPropertiesEXT::default(); modifier_count as usize];
    let mut list = vk::DrmFormatModifierPropertiesListEXT::default()
        .drm_format_modifier_properties(&mut props_storage);
    let mut format_props = vk::FormatProperties2::default().push_next(&mut list);
    unsafe {
        vk.instance.get_physical_device_format_properties2(
            vk.physical_device,
            vk::Format::B8G8R8A8_UNORM,
            &mut format_props,
        );
    }
    let entries = list.drm_format_modifier_count as usize;
    props_storage
        .iter()
        .take(entries)
        .find(|p| p.drm_format_modifier == modifier)
        .map(|p| p.drm_format_modifier_plane_count)
}

fn scanout_image_usage() -> vk::ImageUsageFlags {
    // The scanout image is only ever a compose render target (color
    // attachment), a transfer destination (initial clear / upload), and a
    // transfer source (root screenshots and diagnostic scanout dumps).
    // It is NEVER sampled — RENDER PictOps target pixmap/window mirrors,
    // not the scanout BO, and the compose pass samples those mirrors and
    // blends into this attachment.
    vk::ImageUsageFlags::COLOR_ATTACHMENT
        | vk::ImageUsageFlags::TRANSFER_SRC
        | vk::ImageUsageFlags::TRANSFER_DST
}

fn addfb_flags_for_modifier(modifier: Option<u64>) -> FbCmd2Flags {
    if modifier.is_some() {
        FbCmd2Flags::MODIFIERS
    } else {
        FbCmd2Flags::empty()
    }
}

fn destroy_scanout_image(vk: &VkContext, image: vk::Image, memory: vk::DeviceMemory) {
    unsafe {
        vk.device.destroy_image(image, None);
        vk.device.free_memory(memory, None);
    }
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

/// Allocate a scanout `VkImage` whose memory is dma-buf-exportable;
/// bind memory; export the dma-buf; query the row pitch the driver
/// picked.
fn allocate_vk_scanout_image(
    vk: &VkContext,
    width: u32,
    height: u32,
    plan: ScanoutAllocationPlan,
) -> Result<VkScanoutImage, vk::Result> {
    // GbmModifier is routed via allocate_gbm_scanout_image; the
    // Vulkan-alloc path never sees it.
    debug_assert!(
        !matches!(plan, ScanoutAllocationPlan::GbmModifier(_)),
        "GbmModifier plans must be dispatched via allocate_gbm_scanout_image"
    );
    let ext_memory_fd = vk
        .external_memory_fd
        .as_ref()
        .ok_or(vk::Result::ERROR_EXTENSION_NOT_PRESENT)?;

    let drm_modifier = match plan {
        ScanoutAllocationPlan::DrmModifier(modifier) => Some(modifier),
        ScanoutAllocationPlan::PaddedExplicitLinear { .. }
        | ScanoutAllocationPlan::ExplicitLinear
        | ScanoutAllocationPlan::LegacyLinear => None,
        ScanoutAllocationPlan::GbmModifier(_) => unreachable!(),
    };
    let padded_pitch = match plan {
        ScanoutAllocationPlan::PaddedExplicitLinear { row_pitch } => Some(row_pitch),
        _ => None,
    };

    let mut external_info = vk::ExternalMemoryImageCreateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let modifier_storage = [drm_modifier.unwrap_or(super::dri3::DRM_FORMAT_MOD_LINEAR)];
    let mut modifier_list = vk::ImageDrmFormatModifierListCreateInfoEXT::default()
        .drm_format_modifiers(if drm_modifier.is_some() {
            &modifier_storage
        } else {
            &[]
        });
    // Explicit single-plane LINEAR layout carrying the padded (aligned) stride.
    // `size = 0` lets the implementation compute the plane size for the pitch.
    let explicit_plane_layouts = [vk::SubresourceLayout {
        offset: 0,
        size: 0,
        row_pitch: u64::from(padded_pitch.unwrap_or(0)),
        array_pitch: 0,
        depth_pitch: 0,
    }];
    let mut explicit_modifier_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
        .drm_format_modifier(super::dri3::DRM_FORMAT_MOD_LINEAR)
        .plane_layouts(&explicit_plane_layouts);

    let tiling = match plan {
        ScanoutAllocationPlan::DrmModifier(_)
        | ScanoutAllocationPlan::PaddedExplicitLinear { .. } => {
            vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT
        }
        ScanoutAllocationPlan::ExplicitLinear | ScanoutAllocationPlan::LegacyLinear => {
            vk::ImageTiling::LINEAR
        }
        ScanoutAllocationPlan::GbmModifier(_) => unreachable!(),
    };

    let image_info_base = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(vk::Format::B8G8R8A8_UNORM)
        .extent(vk::Extent3D {
            width,
            height,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(tiling)
        .usage(scanout_image_usage())
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED);

    let image_info = match plan {
        ScanoutAllocationPlan::DrmModifier(_) => image_info_base
            .push_next(&mut external_info)
            .push_next(&mut modifier_list),
        ScanoutAllocationPlan::PaddedExplicitLinear { .. } => image_info_base
            .push_next(&mut external_info)
            .push_next(&mut explicit_modifier_info),
        ScanoutAllocationPlan::ExplicitLinear | ScanoutAllocationPlan::LegacyLinear => {
            image_info_base.push_next(&mut external_info)
        }
        ScanoutAllocationPlan::GbmModifier(_) => unreachable!(),
    };

    let image = unsafe { vk.device.create_image(&image_info, None)? };

    // 3. Memory: dma-buf-exportable + dedicated to this image.
    let mem_reqs = unsafe { vk.device.get_image_memory_requirements(image) };
    let mem_props = unsafe {
        vk.instance
            .get_physical_device_memory_properties(vk.physical_device)
    };
    let memory_type_index = match pick_memory_type(
        &mem_props,
        mem_reqs.memory_type_bits,
        vk::MemoryPropertyFlags::DEVICE_LOCAL,
    )
    .or_else(|| {
        pick_memory_type(
            &mem_props,
            mem_reqs.memory_type_bits,
            vk::MemoryPropertyFlags::empty(),
        )
    }) {
        Some(i) => i,
        None => {
            unsafe { vk.device.destroy_image(image, None) };
            return Err(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY);
        }
    };

    let mut export_info = vk::ExportMemoryAllocateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(mem_reqs.size)
        .memory_type_index(memory_type_index)
        .push_next(&mut export_info)
        .push_next(&mut dedicated);

    let memory = match unsafe { vk.device.allocate_memory(&alloc_info, None) } {
        Ok(m) => m,
        Err(e) => {
            unsafe { vk.device.destroy_image(image, None) };
            return Err(e);
        }
    };

    if let Err(e) = unsafe { vk.device.bind_image_memory(image, memory, 0) } {
        unsafe {
            vk.device.free_memory(memory, None);
            vk.device.destroy_image(image, None);
        }
        return Err(e);
    }

    let selected_modifier = match plan {
        ScanoutAllocationPlan::DrmModifier(_) => {
            let Some(ext) = vk.image_drm_format_modifier_ext.as_ref() else {
                unsafe {
                    vk.device.free_memory(memory, None);
                    vk.device.destroy_image(image, None);
                }
                return Err(vk::Result::ERROR_EXTENSION_NOT_PRESENT);
            };
            let mut props = vk::ImageDrmFormatModifierPropertiesEXT::default();
            if let Err(e) =
                unsafe { ext.get_image_drm_format_modifier_properties(image, &mut props) }
            {
                unsafe {
                    vk.device.free_memory(memory, None);
                    vk.device.destroy_image(image, None);
                }
                return Err(e);
            }
            Some(props.drm_format_modifier)
        }
        // Created with an explicit LINEAR modifier — no need to re-query it.
        ScanoutAllocationPlan::PaddedExplicitLinear { .. } => {
            Some(super::dri3::DRM_FORMAT_MOD_LINEAR)
        }
        ScanoutAllocationPlan::ExplicitLinear => Some(super::dri3::DRM_FORMAT_MOD_LINEAR),
        ScanoutAllocationPlan::LegacyLinear => None,
        ScanoutAllocationPlan::GbmModifier(_) => unreachable!(),
    };

    // Row pitch from the driver. We need this for KMS addfb2.
    // Modifier-tiled images MUST be queried with a MEMORY_PLANE aspect;
    // COLOR is a validation error (the single-plane scanout buffer is
    // plane 0). LINEAR-tiled fallbacks keep the COLOR aspect. The
    // padded-explicit-LINEAR image is a DRM-modifier image too → MEMORY_PLANE_0.
    let layout_aspect = match plan {
        ScanoutAllocationPlan::DrmModifier(_)
        | ScanoutAllocationPlan::PaddedExplicitLinear { .. } => {
            vk::ImageAspectFlags::MEMORY_PLANE_0_EXT
        }
        ScanoutAllocationPlan::ExplicitLinear | ScanoutAllocationPlan::LegacyLinear => {
            vk::ImageAspectFlags::COLOR
        }
        ScanoutAllocationPlan::GbmModifier(_) => unreachable!(),
    };
    let layout = unsafe {
        vk.device.get_image_subresource_layout(
            image,
            vk::ImageSubresource {
                aspect_mask: layout_aspect,
                mip_level: 0,
                array_layer: 0,
            },
        )
    };
    let pitch = u32::try_from(layout.row_pitch).unwrap_or(u32::MAX);

    // 4. Export the bound memory as a dma-buf fd.
    let get_fd_info = vk::MemoryGetFdInfoKHR::default()
        .memory(memory)
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let raw_fd = match unsafe { ext_memory_fd.get_memory_fd(&get_fd_info) } {
        Ok(fd) => fd,
        Err(e) => {
            unsafe {
                vk.device.free_memory(memory, None);
                vk.device.destroy_image(image, None);
            }
            return Err(e);
        }
    };
    let dmabuf = super::owned_fd_from_vk(raw_fd, "vkGetMemoryFdKHR(DMA_BUF)")?;

    let offset = u32::try_from(layout.offset).unwrap_or(0);
    Ok(VkScanoutImage {
        image,
        memory,
        dmabuf,
        pitch,
        offset,
        modifier: selected_modifier,
        gbm_bo: None,
    })
}

/// GBM-allocate a single-plane scanout BO with the given DRM
/// modifier, then import its dma-buf into Vulkan as the compose
/// render target. This is the preferred allocation path — see the
/// module doc-comment for why.
///
/// The returned `VkScanoutImage.gbm_bo` MUST be kept alive at least
/// until the returned VkImage/VkDeviceMemory/GEM/framebuffer have all
/// been torn down. `ScanoutBo` handles that by declaring `gbm_bo`
/// last in its field list so Rust drops it after the explicit `Drop`
/// impl has released the derived resources.
fn allocate_gbm_scanout_image(
    vk: &VkContext,
    gbm: &Rc<GbmDevice>,
    width: u32,
    height: u32,
    modifier: u64,
) -> Result<VkScanoutImage, GbmScanoutError> {
    let ext_memory_fd = vk
        .external_memory_fd
        .as_ref()
        .ok_or(GbmScanoutError::MissingExtension(
            "VK_KHR_external_memory_fd",
        ))?;
    if vk.image_drm_format_modifier_ext.is_none() {
        return Err(GbmScanoutError::MissingExtension(
            "VK_EXT_image_drm_format_modifier",
        ));
    }

    // Codex gate: verify Vulkan can IMPORT (not just export) a
    // COLOR_ATTACHMENT image with this exact modifier as DMA_BUF.
    if !scanout_modifier_is_single_plane_importable(vk, modifier) {
        return Err(GbmScanoutError::NotImportable(modifier));
    }

    // 1. GBM allocation — driver-side scanout-layout buffer.
    let modifier_iter = std::iter::once(gbm::Modifier::from(modifier));
    let bo = gbm
        .create_buffer_object_with_modifiers2::<()>(
            width,
            height,
            gbm::Format::Xrgb8888,
            modifier_iter,
            gbm::BufferObjectFlags::RENDERING | gbm::BufferObjectFlags::SCANOUT,
        )
        .map_err(GbmScanoutError::GbmCreate)?;

    // Multi-plane modifiers (e.g. AMD DCC compression) are out of
    // scope for the first cut — see codex correction in the spec.
    let plane_count = bo.plane_count();
    if plane_count != 1 {
        return Err(GbmScanoutError::MultiPlane(plane_count));
    }
    let gbm_modifier: u64 = bo.modifier().into();
    if gbm_modifier != modifier {
        return Err(GbmScanoutError::UnexpectedModifier {
            requested: modifier,
            actual: gbm_modifier,
        });
    }
    let stride = bo.stride_for_plane(0);
    let offset = bo.offset(0);

    // 2. Bo dma-buf fd. Vulkan takes ownership of the fd we hand to
    //    ImportMemoryFdInfoKHR ONLY on vkAllocateMemory success —
    //    dup so we retain a copy for PRIME_FD_TO_HANDLE afterwards
    //    (matches the DRI3 importer's ownership rule at
    //    target.rs:355).
    let bo_fd = bo.fd().map_err(|_| GbmScanoutError::InvalidBoFd)?;
    let vk_fd_owned = bo_fd.try_clone().map_err(GbmScanoutError::FdDup)?;
    let vk_fd_raw = vk_fd_owned.into_raw_fd();

    // 3. Create the VkImage against GBM's stride/offset via the
    //    explicit-modifier layout struct. Same usage tuple as the
    //    IMPORTABLE gate above (`scanout_image_usage()` + DMA_BUF external
    //    memory + DRM_FORMAT_MODIFIER_EXT tiling).
    let plane_layouts = [vk::SubresourceLayout {
        offset: u64::from(offset),
        size: 0,
        row_pitch: u64::from(stride),
        array_pitch: 0,
        depth_pitch: 0,
    }];
    let mut explicit_modifier_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
        .drm_format_modifier(gbm_modifier)
        .plane_layouts(&plane_layouts);
    let mut external_info = vk::ExternalMemoryImageCreateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let image_info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(vk::Format::B8G8R8A8_UNORM)
        .extent(vk::Extent3D {
            width,
            height,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(scanout_image_usage())
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .push_next(&mut external_info)
        .push_next(&mut explicit_modifier_info);
    let image = match unsafe { vk.device.create_image(&image_info, None) } {
        Ok(i) => i,
        Err(e) => {
            unsafe { libc::close(vk_fd_raw) };
            return Err(GbmScanoutError::Vk(e));
        }
    };

    // 4. Memory-type selection intersects image requirements with
    //    the dma-buf's own compatible memory types
    //    (vkGetMemoryFdPropertiesKHR). Codex flagged that the DRI3
    //    importer at target.rs:371 skips this — it's mandated for
    //    robust external import and NVIDIA proprietary in particular
    //    exposes distinct memory types for imported vs local BOs.
    let mem_reqs = unsafe { vk.device.get_image_memory_requirements(image) };
    let mut fd_props = vk::MemoryFdPropertiesKHR::default();
    if let Err(e) = unsafe {
        ext_memory_fd.get_memory_fd_properties(
            vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
            vk_fd_raw,
            &mut fd_props,
        )
    } {
        unsafe {
            vk.device.destroy_image(image, None);
            libc::close(vk_fd_raw);
        }
        return Err(GbmScanoutError::Vk(e));
    }
    let mem_props = unsafe {
        vk.instance
            .get_physical_device_memory_properties(vk.physical_device)
    };
    let effective_type_bits = mem_reqs.memory_type_bits & fd_props.memory_type_bits;
    if effective_type_bits == 0 {
        unsafe {
            vk.device.destroy_image(image, None);
            libc::close(vk_fd_raw);
        }
        return Err(GbmScanoutError::NoImportableMemoryType);
    }
    let memory_type_index = pick_memory_type(
        &mem_props,
        effective_type_bits,
        vk::MemoryPropertyFlags::DEVICE_LOCAL,
    )
    .or_else(|| {
        pick_memory_type(
            &mem_props,
            effective_type_bits,
            vk::MemoryPropertyFlags::empty(),
        )
    });
    let Some(memory_type_index) = memory_type_index else {
        unsafe {
            vk.device.destroy_image(image, None);
            libc::close(vk_fd_raw);
        }
        return Err(GbmScanoutError::NoImportableMemoryType);
    };

    let mut import_info = vk::ImportMemoryFdInfoKHR::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
        .fd(vk_fd_raw);
    let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(mem_reqs.size)
        .memory_type_index(memory_type_index)
        .push_next(&mut import_info)
        .push_next(&mut dedicated);
    let memory = match unsafe { vk.device.allocate_memory(&alloc_info, None) } {
        Ok(m) => m,
        Err(e) => {
            unsafe {
                vk.device.destroy_image(image, None);
                // vkAllocateMemory consumes vk_fd_raw only on success.
                libc::close(vk_fd_raw);
            }
            return Err(GbmScanoutError::Vk(e));
        }
    };
    // On success `memory` owns `vk_fd_raw`; do NOT close it here.
    if let Err(e) = unsafe { vk.device.bind_image_memory(image, memory, 0) } {
        unsafe {
            vk.device.free_memory(memory, None);
            vk.device.destroy_image(image, None);
        }
        return Err(GbmScanoutError::Vk(e));
    }

    // Sanity-check what the GBM-imported layout looks like from
    // Vulkan's side. If GBM's stride/offset disagrees with what
    // Vulkan reports back for the same modifier, that's a driver
    // bug worth surfacing; the AddFB2 side always gets GBM's
    // numbers regardless (they came from the same driver that
    // laid out the BO).
    let layout = unsafe {
        vk.device.get_image_subresource_layout(
            image,
            vk::ImageSubresource {
                aspect_mask: vk::ImageAspectFlags::MEMORY_PLANE_0_EXT,
                mip_level: 0,
                array_layer: 0,
            },
        )
    };
    if layout.row_pitch != u64::from(stride) || layout.offset != u64::from(offset) {
        log::warn!(
            "scanout gbm import: layout mismatch — gbm(stride={stride},offset={offset}) \
             vk(row_pitch={},offset={}); using gbm values",
            layout.row_pitch,
            layout.offset,
        );
    }

    // We still need a dma-buf fd for PRIME_FD_TO_HANDLE. Vulkan
    // owns the dup we handed it; reuse the original bo_fd we kept
    // around.
    Ok(VkScanoutImage {
        image,
        memory,
        dmabuf: bo_fd,
        pitch: stride,
        offset,
        modifier: Some(gbm_modifier),
        gbm_bo: Some(bo),
    })
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

impl std::fmt::Display for GbmScanoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingExtension(name) => write!(f, "missing Vulkan extension: {name}"),
            Self::NotImportable(m) => write!(
                f,
                "modifier 0x{m:x} is not IMPORTABLE + DMA_BUF for COLOR_ATTACHMENT B8G8R8A8_UNORM"
            ),
            Self::GbmCreate(e) => write!(f, "gbm_bo_create_with_modifiers: {e}"),
            Self::MultiPlane(n) => write!(
                f,
                "multi-plane modifier not supported (plane_count={n}); first cut is \
                 single-plane only"
            ),
            Self::UnexpectedModifier { requested, actual } => write!(
                f,
                "GBM returned modifier 0x{actual:x} for exact requested modifier 0x{requested:x}"
            ),
            Self::InvalidBoFd => write!(f, "gbm_bo_get_fd returned an invalid fd"),
            Self::FdDup(e) => write!(f, "dup(gbm_bo_fd) failed: {e}"),
            Self::NoImportableMemoryType => write!(
                f,
                "no memory type satisfies image requirements ∩ dma-buf-import requirements"
            ),
            Self::Vk(r) => write!(f, "vk error: {r:?}"),
        }
    }
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

impl DrmPlanarBuffer for VkScanoutFb {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
    fn format(&self) -> DrmFourcc {
        DrmFourcc::Xrgb8888
    }
    fn modifier(&self) -> Option<DrmModifier> {
        self.modifier.map(DrmModifier::from)
    }
    fn pitches(&self) -> [u32; 4] {
        [self.pitch, 0, 0, 0]
    }
    fn handles(&self) -> [Option<DrmBufferHandle>; 4] {
        [Some(self.gem_handle), None, None, None]
    }
    fn offsets(&self) -> [u32; 4] {
        [self.offset, 0, 0, 0]
    }
}

/// Create a binary `VkSemaphore` whose payload can be exported as a
/// SYNC_FD via `vkGetSemaphoreFdKHR`. Reused for the bo's full
/// lifetime; the fd payload churns per submit.
fn create_export_semaphore(vk: &VkContext) -> Result<vk::Semaphore, vk::Result> {
    let mut export_info = vk::ExportSemaphoreCreateInfo::default()
        .handle_types(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
    let create_info = vk::SemaphoreCreateInfo::default().push_next(&mut export_info);
    unsafe { vk.device.create_semaphore(&create_info, None) }
}

/// Allocate per-bo transfer resources: command pool + 1 command
/// buffer; staging buffer + host-mapped device memory sized for one
/// XRGB8888 frame at (width × height).
fn allocate_transfer_resources(
    vk: &VkContext,
    width: u32,
    height: u32,
) -> Result<TransferResources, vk::Result> {
    let pool_info = vk::CommandPoolCreateInfo::default()
        .queue_family_index(vk.graphics_queue_family)
        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
    let command_pool = unsafe { vk.device.create_command_pool(&pool_info, None)? };

    let cb_info = vk::CommandBufferAllocateInfo::default()
        .command_pool(command_pool)
        .level(vk::CommandBufferLevel::PRIMARY)
        .command_buffer_count(1);
    let command_buffers = match unsafe { vk.device.allocate_command_buffers(&cb_info) } {
        Ok(cbs) => cbs,
        Err(e) => {
            unsafe { vk.device.destroy_command_pool(command_pool, None) };
            return Err(e);
        }
    };
    let command_buffer = command_buffers[0];

    let staging_size: u64 = u64::from(width) * u64::from(height) * 4;
    let buf_info = vk::BufferCreateInfo::default()
        .size(staging_size)
        .usage(transfer_staging_buffer_usage())
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let staging_buffer = match unsafe { vk.device.create_buffer(&buf_info, None) } {
        Ok(b) => b,
        Err(e) => {
            unsafe { vk.device.destroy_command_pool(command_pool, None) };
            return Err(e);
        }
    };

    let mem_reqs = unsafe { vk.device.get_buffer_memory_requirements(staging_buffer) };
    let mem_props = unsafe {
        vk.instance
            .get_physical_device_memory_properties(vk.physical_device)
    };
    let want_strict = vk::MemoryPropertyFlags::HOST_VISIBLE
        | vk::MemoryPropertyFlags::HOST_COHERENT
        | vk::MemoryPropertyFlags::DEVICE_LOCAL;
    let want_loose = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
    let memory_type_index = pick_memory_type(&mem_props, mem_reqs.memory_type_bits, want_strict)
        .or_else(|| pick_memory_type(&mem_props, mem_reqs.memory_type_bits, want_loose))
        .ok_or(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY);
    let memory_type_index = match memory_type_index {
        Ok(i) => i,
        Err(e) => {
            unsafe {
                vk.device.destroy_buffer(staging_buffer, None);
                vk.device.destroy_command_pool(command_pool, None);
            }
            return Err(e);
        }
    };

    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(mem_reqs.size)
        .memory_type_index(memory_type_index);
    let staging_memory = match unsafe { vk.device.allocate_memory(&alloc_info, None) } {
        Ok(m) => m,
        Err(e) => {
            unsafe {
                vk.device.destroy_buffer(staging_buffer, None);
                vk.device.destroy_command_pool(command_pool, None);
            }
            return Err(e);
        }
    };
    if let Err(e) = unsafe {
        vk.device
            .bind_buffer_memory(staging_buffer, staging_memory, 0)
    } {
        unsafe {
            vk.device.free_memory(staging_memory, None);
            vk.device.destroy_buffer(staging_buffer, None);
            vk.device.destroy_command_pool(command_pool, None);
        }
        return Err(e);
    }

    let mapped_ptr = match unsafe {
        vk.device
            .map_memory(staging_memory, 0, staging_size, vk::MemoryMapFlags::empty())
    } {
        Ok(p) => p,
        Err(e) => {
            unsafe {
                vk.device.free_memory(staging_memory, None);
                vk.device.destroy_buffer(staging_buffer, None);
                vk.device.destroy_command_pool(command_pool, None);
            }
            return Err(e);
        }
    };
    let staging_mapped =
        std::ptr::NonNull::new(mapped_ptr.cast::<u8>()).expect("vkMapMemory returned non-null");

    // 2-query TIMESTAMP pool for the compose GPU-render timer. Created
    // even if the device lacks timestamp support (creation succeeds
    // regardless); `record_composite_command_buffer` gates use on
    // `vk.timestamp_period > 0.0`. Created last so no earlier error path
    // needs to reap it.
    let timestamp_pool = {
        let info = vk::QueryPoolCreateInfo::default()
            .query_type(vk::QueryType::TIMESTAMP)
            .query_count(2);
        match unsafe { vk.device.create_query_pool(&info, None) } {
            Ok(pool) => pool,
            Err(error) => {
                unsafe {
                    vk.device.unmap_memory(staging_memory);
                    vk.device.destroy_buffer(staging_buffer, None);
                    vk.device.free_memory(staging_memory, None);
                    vk.device.destroy_command_pool(command_pool, None);
                }
                return Err(error);
            }
        }
    };

    Ok(TransferResources {
        command_pool,
        command_buffer,
        staging_buffer,
        staging_memory,
        staging_mapped,
        staging_size,
        timestamp_pool,
    })
}

fn transfer_staging_buffer_usage() -> vk::BufferUsageFlags {
    // Normal upload/readback code shares this per-BO allocation. Copied
    // compatibility probing additionally writes complete A/B images into the
    // mapping before the CPU validates their content.
    vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST
}

fn destroy_transfer_resources(vk: &VkContext, transfer: &mut TransferResources) {
    unsafe {
        vk.device.unmap_memory(transfer.staging_memory);
        vk.device.destroy_buffer(transfer.staging_buffer, None);
        vk.device.free_memory(transfer.staging_memory, None);
        if transfer.timestamp_pool != vk::QueryPool::null() {
            vk.device.destroy_query_pool(transfer.timestamp_pool, None);
        }
        vk.device.destroy_command_pool(transfer.command_pool, None);
    }
}

fn pick_memory_type(
    props: &vk::PhysicalDeviceMemoryProperties,
    type_bits: u32,
    required: vk::MemoryPropertyFlags,
) -> Option<u32> {
    (0..props.memory_type_count).find(|&i| {
        let candidate = type_bits & (1 << i) != 0;
        candidate
            && props.memory_types[i as usize]
                .property_flags
                .contains(required)
    })
}

// Compile-only check that `export_signaled_fd`'s call into
// `external_semaphore_fd::Device::get_semaphore_fd` keeps the same
// argument shape. If ash bumps and the signature changes, this
// function fails to compile and breaks the build before any
// integration test runs.
#[cfg(test)]
#[allow(dead_code)]
fn _compile_check_export_signature(
    ext: &ash::khr::external_semaphore_fd::Device,
    semaphore: vk::Semaphore,
) {
    let info = vk::SemaphoreGetFdInfoKHR::default()
        .semaphore(semaphore)
        .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
    let _: Result<i32, vk::Result> = unsafe { ext.get_semaphore_fd(&info) };
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINEAR: u64 = super::super::dri3::DRM_FORMAT_MOD_LINEAR;
    // Representative tiled modifiers (real AMD GFX9+ vendor-tiled values).
    const TILED_A: u64 = 0x0200_0000_0000_0008;
    const TILED_B: u64 = 0x0200_0000_0000_000a;

    #[derive(Default)]
    struct ProbeFenceSpy {
        abandoned: u8,
        destroyed_idle: u8,
        wait_error: Option<io::ErrorKind>,
        waits: Vec<(u64, &'static str)>,
    }

    impl ProbeFenceSpy {
        fn timing_out() -> Self {
            Self {
                wait_error: Some(io::ErrorKind::TimedOut),
                ..Self::default()
            }
        }
    }

    impl DisposableProbeFence for ProbeFenceSpy {
        fn abandon(&mut self) {
            self.abandoned += 1;
        }

        fn destroy_idle(&mut self) {
            self.destroyed_idle += 1;
        }

        fn wait_bounded(&mut self, timeout_ns: u64, operation: &'static str) -> io::Result<()> {
            self.waits.push((timeout_ns, operation));
            match self.wait_error {
                Some(kind) => Err(io::Error::new(kind, "scripted probe fence wait")),
                None => Ok(()),
            }
        }
    }

    fn test_route(relationship: RenderKmsRelationship) -> ScanoutRoute {
        ScanoutRoute::new(
            crate::kms::scanout_route::RenderDeviceId::DrmRender(
                crate::platform::drm::DrmDeviceKey {
                    major: 226,
                    minor: 128,
                },
            ),
            crate::platform::drm::DrmDeviceKey {
                major: 226,
                minor: 1,
            },
            relationship,
        )
    }

    fn test_sink_route() -> ScanoutRoute {
        ScanoutRoute::new(
            crate::kms::scanout_route::RenderDeviceId::DrmRender(
                crate::platform::drm::DrmDeviceKey {
                    major: 226,
                    minor: 129,
                },
            ),
            crate::platform::drm::DrmDeviceKey {
                major: 226,
                minor: 1,
            },
            RenderKmsRelationship::Same,
        )
    }

    fn test_direction_metadata(
        kms_prime: ScanoutMetadataSupport,
        modifier_path: ScanoutMetadataSupport,
        linear_path: ScanoutMetadataSupport,
    ) -> DmabufDirectionMetadata {
        let kms_layout = match linear_path {
            ScanoutMetadataSupport::Supported => KmsLinearLayout::ExplicitModifier,
            ScanoutMetadataSupport::Unsupported => KmsLinearLayout::NotAdvertised,
            ScanoutMetadataSupport::Unknown => KmsLinearLayout::LegacyAddfb,
        };
        DmabufDirectionMetadata {
            kms_prime,
            vulkan_modifiers: modifier_path,
            modifiers: (modifier_path == ScanoutMetadataSupport::Supported)
                .then_some(TILED_A)
                .into_iter()
                .collect(),
            modifier_path,
            linear: DmabufLinearMetadata {
                vulkan: linear_path,
                kms_layout,
                path: linear_path,
            },
        }
    }

    fn test_scanout_metadata(
        external_memory_fd: ScanoutMetadataSupport,
        output_owned: DmabufDirectionMetadata,
        renderer_owned: DmabufDirectionMetadata,
    ) -> DmabufScanoutMetadata {
        DmabufScanoutMetadata {
            vulkan_external_memory_fd: external_memory_fd,
            output_owned,
            renderer_owned,
        }
    }

    fn test_direction_verdict(
        status: ScanoutMetadataSupport,
        direction: DmabufAllocationDirection,
    ) -> DmabufDirectionVerdict {
        match (status, direction) {
            (ScanoutMetadataSupport::Supported, _) => DmabufDirectionVerdict::Supported,
            (ScanoutMetadataSupport::Unsupported, DmabufAllocationDirection::OutputOwned) => {
                DmabufDirectionVerdict::Unsupported(
                    DmabufDirectionIncompatibility::OutputOwnedKmsPrimeExportUnsupported,
                )
            }
            (ScanoutMetadataSupport::Unsupported, DmabufAllocationDirection::RendererOwned) => {
                DmabufDirectionVerdict::Unsupported(
                    DmabufDirectionIncompatibility::RendererOwnedKmsPrimeImportUnsupported,
                )
            }
            (ScanoutMetadataSupport::Unknown, DmabufAllocationDirection::OutputOwned) => {
                DmabufDirectionVerdict::Unknown(
                    DmabufScanoutUncertainty::OutputOwnedLayoutMetadataIncomplete,
                )
            }
            (ScanoutMetadataSupport::Unknown, DmabufAllocationDirection::RendererOwned) => {
                DmabufDirectionVerdict::Unknown(
                    DmabufScanoutUncertainty::RendererOwnedLayoutMetadataIncomplete,
                )
            }
        }
    }

    #[test]
    fn copied_source_candidates_dedupe_native_and_append_linear_once() {
        let advertised = [LINEAR, TILED_A, TILED_B, TILED_A, LINEAR];
        assert_eq!(
            order_copied_source_plans(&advertised, |_| true),
            vec![
                CopiedSourcePlan::DrmModifier(TILED_A),
                CopiedSourcePlan::DrmModifier(TILED_B),
                CopiedSourcePlan::DrmModifier(LINEAR),
            ]
        );
    }

    #[test]
    fn copied_source_candidates_filter_unsupported_pairs_and_keep_native_only() {
        let advertised = [TILED_A, TILED_B];
        assert_eq!(
            order_copied_source_plans(&advertised, |modifier| modifier == TILED_B),
            vec![CopiedSourcePlan::DrmModifier(TILED_B)]
        );
        assert_eq!(
            order_copied_source_plans(&[], |modifier| modifier == LINEAR),
            vec![CopiedSourcePlan::DrmModifier(LINEAR)]
        );
    }

    #[test]
    fn copied_plan_order_exhausts_native_tier_before_linear() {
        let destinations = [
            ScanoutAllocationPlan::GbmModifier(TILED_A),
            ScanoutAllocationPlan::DrmModifier(TILED_B),
        ];
        // Deliberately place LINEAR first: assembly must enforce tiers rather
        // than trusting its caller's input order.
        let sources = [
            CopiedSourcePlan::DrmModifier(LINEAR),
            CopiedSourcePlan::DrmModifier(TILED_A),
            CopiedSourcePlan::DrmModifier(TILED_B),
        ];

        assert_eq!(
            assemble_copied_scanout_plans(&destinations, &sources),
            vec![
                CopiedScanoutPlan {
                    source: CopiedSourcePlan::DrmModifier(TILED_A),
                    destination: ScanoutAllocationPlan::GbmModifier(TILED_A),
                },
                CopiedScanoutPlan {
                    source: CopiedSourcePlan::DrmModifier(TILED_B),
                    destination: ScanoutAllocationPlan::GbmModifier(TILED_A),
                },
                CopiedScanoutPlan {
                    source: CopiedSourcePlan::DrmModifier(TILED_A),
                    destination: ScanoutAllocationPlan::DrmModifier(TILED_B),
                },
                CopiedScanoutPlan {
                    source: CopiedSourcePlan::DrmModifier(TILED_B),
                    destination: ScanoutAllocationPlan::DrmModifier(TILED_B),
                },
                CopiedScanoutPlan {
                    source: CopiedSourcePlan::DrmModifier(LINEAR),
                    destination: ScanoutAllocationPlan::GbmModifier(TILED_A),
                },
                CopiedScanoutPlan {
                    source: CopiedSourcePlan::DrmModifier(LINEAR),
                    destination: ScanoutAllocationPlan::DrmModifier(TILED_B),
                },
            ]
        );
    }

    #[test]
    fn copied_plan_is_transport_not_a_third_allocation_owner() {
        let plan = CopiedScanoutPlan {
            source: CopiedSourcePlan::DrmModifier(TILED_A),
            destination: ScanoutAllocationPlan::GbmModifier(TILED_B),
        };

        assert_eq!(plan.destination.ownership(), ScanoutOwnership::Output);
        assert_eq!(
            plan.describe(),
            format!(
                "source-drm-modifier=0x{TILED_A:x}-native-transport -> destination-gbm-modifier=0x{TILED_B:x}"
            )
        );
    }

    #[test]
    fn copied_content_probe_accepts_exact_odd_extent_pixels() {
        let width = 3;
        let height = 5;
        let pixels: Vec<u8> = (0..tight_bgra_len(width, height).unwrap())
            .map(|index| index.wrapping_mul(37) as u8)
            .collect();

        verify_copied_probe_pixels(&pixels, &pixels, width, height, 1, 0, 2)
            .expect("identical renderer and sink pixels");
    }

    #[test]
    fn copied_content_probe_rejects_one_channel_corruption() {
        let width = 4;
        let height = 3;
        let renderer = vec![0x5a; tight_bgra_len(width, height).unwrap()];
        let mut sink = renderer.clone();
        let mismatch = ((2 * width + 1) * 4 + 2) as usize;
        sink[mismatch] ^= 0xff;

        let error =
            verify_copied_probe_pixels(&renderer, &sink, width, height, 0, 1, 1).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        let text = error.to_string();
        assert!(text.contains("first difference at (1,2) channel=R"));
        assert!(text.contains("renderer_hash=fnv1a64:"));
        assert!(text.contains("sink_hash=fnv1a64:"));
    }

    #[test]
    fn copied_content_probe_rejects_equal_stale_cycles() {
        let error = validate_copied_probe_freshness(
            Some(0xfeed_face_cafe_beef),
            0xfeed_face_cafe_beef,
            2,
            1,
            5,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("stale renderer pixels"));

        validate_copied_probe_freshness(
            Some(0xfeed_face_cafe_beef),
            0x0123_4567_89ab_cdef,
            2,
            1,
            5,
        )
        .expect("different tokenized renderer frames remain admissible");
    }

    #[test]
    fn copied_content_probe_requires_rendered_corner_fiducials() {
        let pixels = [
            83, 37, 241, 255, 71, 211, 29, 255, 233, 91, 47, 255, 19, 173, 223, 255,
        ];
        validate_copied_probe_fiducials(&pixels, 2, 2, 0, 0, 0).expect("four shader fiducials");

        let token_one = [
            76, 52, 240, 255, 88, 194, 28, 255, 246, 74, 46, 255, 12, 188, 222, 255,
        ];
        validate_copied_probe_fiducials(&token_one, 2, 2, 0, 1, 1)
            .expect("tokenized shader fiducials");

        let uniform = [0; 16];
        assert!(validate_copied_probe_fiducials(&uniform, 2, 2, 0, 0, 0).is_err());
    }

    #[test]
    fn copied_gpu_digest_accepts_matching_blocks_and_tokenized_corners() {
        let token = 1;
        let mut renderer = vec![
            copied_probe_marker_word([241, 37, 83], token),
            copied_probe_marker_word([29, 211, 71], token),
            copied_probe_marker_word([47, 91, 233], token),
            copied_probe_marker_word([223, 173, 19], token),
        ];
        renderer.extend([
            0x1020_3040,
            0x5060_7080,
            0x90a0_b0c0,
            0xd0e0_f001,
            0x1234_5678,
            0x9abc_def0,
            0x55aa_aa55,
            0x0f0f_f0f0,
        ]);
        let sink = renderer.clone();

        validate_copied_probe_digest_fiducials(&renderer, 0, 1, token)
            .expect("digest carries the current rendered corner words");
        verify_copied_probe_digests(&renderer, &sink, 2, 1, 0, 1, token)
            .expect("matching per-block GPU digests");
    }

    #[test]
    fn copied_gpu_digest_rejects_block_lane_corruption() {
        let token = 0;
        let mut renderer = vec![
            copied_probe_marker_word([241, 37, 83], token),
            copied_probe_marker_word([29, 211, 71], token),
            copied_probe_marker_word([47, 91, 233], token),
            copied_probe_marker_word([223, 173, 19], token),
        ];
        renderer.extend(0_u32..24);
        let mut sink = renderer.clone();
        // Header occupies four words; this is lane 2 of grid block (1, 1)
        // in a 3-wide grid.
        sink[4 + (4 * 4) + 2] ^= 1;

        let error = verify_copied_probe_digests(&renderer, &sink, 3, 2, 0, 0, token).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("block=(1, 1) lane=2"));
    }

    #[test]
    fn copied_gpu_digest_rejects_wrong_or_stale_frame() {
        let token = 1;
        let previous = vec![1, 2, 3, 4, 5, 6, 7, 8];
        let renderer = previous.clone();
        let stale = validate_copied_probe_digest_freshness(Some(&previous), &renderer, 2, 1, token)
            .unwrap_err();
        assert_eq!(stale.kind(), io::ErrorKind::InvalidData);
        assert!(stale.to_string().contains("stale compact GPU digest"));

        let wrong_corners = vec![0; 8];
        let wrong =
            validate_copied_probe_digest_fiducials(&wrong_corners, 2, 1, token).unwrap_err();
        assert_eq!(wrong.kind(), io::ErrorKind::InvalidData);
        assert!(wrong.to_string().contains("corner BGRA words"));
    }

    #[test]
    fn transfer_staging_buffer_supports_upload_and_probe_readback() {
        let usage = transfer_staging_buffer_usage();
        assert!(usage.contains(vk::BufferUsageFlags::TRANSFER_SRC));
        assert!(usage.contains(vk::BufferUsageFlags::TRANSFER_DST));
    }

    #[test]
    fn copied_route_keeps_outer_and_sink_local_identity_distinct() {
        let outer = test_route(RenderKmsRelationship::Different);
        let sink = test_sink_route();
        validate_copied_route_pair(outer, sink).expect("truthful copied route pair");

        let wrong_kms = ScanoutRoute::new(
            sink.render_device_id,
            crate::platform::drm::DrmDeviceKey {
                major: 226,
                minor: 2,
            },
            RenderKmsRelationship::Same,
        );
        assert!(validate_copied_route_pair(outer, wrong_kms).is_err());

        let nonlocal_sink = ScanoutRoute::new(
            sink.render_device_id,
            sink.kms_device_key,
            RenderKmsRelationship::Unknown,
        );
        assert!(validate_copied_route_pair(outer, nonlocal_sink).is_err());

        let same_renderer = ScanoutRoute::new(
            outer.render_device_id,
            outer.kms_device_key,
            RenderKmsRelationship::Same,
        );
        assert!(validate_copied_route_pair(outer, same_renderer).is_err());
    }

    #[test]
    fn copied_device_lost_error_keeps_structured_source_chain() {
        let error = scanout_io_context(
            "copied sink BO 2",
            scanout_vk_error("submit copied sink transfer", vk::Result::ERROR_DEVICE_LOST),
        );
        assert!(scanout_error_is_device_lost(&error));
        assert!(error.to_string().contains("copied sink BO 2"));
        assert!(error.to_string().contains("submit copied sink transfer"));
    }

    #[test]
    fn copied_recovery_reuses_resources_only_after_successful_quiescence() {
        assert!(copied_quiescence_result("test", Ok(())).is_ok());

        let lost = copied_quiescence_result("test", Err(vk::Result::ERROR_DEVICE_LOST))
            .expect_err("device-lost work must remain quarantined");
        assert!(scanout_error_is_device_lost(&lost));

        assert!(
            copied_quiescence_result("test", Err(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY)).is_err()
        );
    }

    #[test]
    fn copied_local_target_readback_is_independent_of_transport_ownership() {
        let mut contents = CopiedRenderTargetContents::default();
        assert!(contents.validate_readback().is_err());

        contents.note_submit_succeeded();
        assert!(contents.validate_readback().is_ok());

        contents.invalidate();
        assert!(contents.validate_readback().is_err());
    }

    #[test]
    fn copied_transport_preparation_tracks_first_discard_and_foreign_return() {
        for state in [
            CopiedSourceOwnership::RendererFirstUse,
            CopiedSourceOwnership::RendererDiscard,
        ] {
            assert_eq!(
                state.transport_preparation().expect("local full overwrite"),
                CopiedTransportPreparation {
                    foreign_acquire: false,
                    local_old_layout: vk::ImageLayout::UNDEFINED,
                }
            );
        }
        assert_eq!(
            CopiedSourceOwnership::ForeignAwaitingRenderer
                .transport_preparation()
                .expect("synchronized foreign return"),
            CopiedTransportPreparation {
                foreign_acquire: true,
                local_old_layout: vk::ImageLayout::GENERAL,
            }
        );
        assert!(
            CopiedSourceOwnership::ForeignAwaitingSink
                .transport_preparation()
                .is_err()
        );
        assert!(
            CopiedSourceOwnership::ForeignReturnPending
                .transport_preparation()
                .is_err()
        );
    }

    #[test]
    fn copied_sink_query_and_import_share_exact_transfer_source_usage() {
        assert_eq!(COPIED_SINK_IMPORT_USAGE, vk::ImageUsageFlags::TRANSFER_SRC);
    }

    #[test]
    fn lifecycle_quiescence_normalizes_every_source_handoff_to_full_discard() {
        let states = [
            CopiedSourceOwnership::RendererFirstUse,
            CopiedSourceOwnership::ForeignAwaitingSink,
            CopiedSourceOwnership::ForeignAwaitingRenderer,
            CopiedSourceOwnership::RendererDiscard,
            CopiedSourceOwnership::ForeignReturnPending,
        ];
        for state in states {
            assert_eq!(
                state.after_lifecycle_quiescence(),
                CopiedSourceOwnership::RendererDiscard,
                "source state {state:?} must resume through a full repaint"
            );
        }
    }

    #[test]
    fn lifecycle_quiescence_normalizes_every_destination_to_local_discard() {
        let states = [
            CopiedDestinationOwnership::LocalFirstUse,
            CopiedDestinationOwnership::ForeignImportedFirstUse,
            CopiedDestinationOwnership::ForeignPendingKmsFromSink,
            CopiedDestinationOwnership::ForeignPendingKmsUninitialized,
            CopiedDestinationOwnership::ForeignRetiredByKms,
            CopiedDestinationOwnership::ReleasedButAtomicRejected,
        ];
        for state in states {
            let resumed = state.after_lifecycle_quiescence();
            assert_eq!(
                resumed,
                CopiedDestinationOwnership::ReleasedButAtomicRejected,
                "destination state {state:?} must resume through a full copy"
            );
            assert_eq!(resumed.foreign_acquire_layouts(), None);
            assert_eq!(
                resumed
                    .local_copy_old_layout()
                    .expect("discard is reusable"),
                vk::ImageLayout::UNDEFINED
            );
        }
    }

    #[test]
    fn destination_becomes_foreign_reusable_only_after_kms_retirement() {
        assert!(
            CopiedDestinationOwnership::ForeignPendingKmsFromSink
                .local_copy_old_layout()
                .is_err()
        );
        assert!(
            CopiedDestinationOwnership::ForeignPendingKmsUninitialized
                .local_copy_old_layout()
                .is_err()
        );
        assert_eq!(
            CopiedDestinationOwnership::ForeignRetiredByKms.foreign_acquire_layouts(),
            Some((vk::ImageLayout::GENERAL, vk::ImageLayout::GENERAL))
        );
    }

    #[test]
    fn modeset_retirement_preserves_uninitialized_vs_sink_general_provenance() {
        let fresh = CopiedDestinationOwnership::LocalFirstUse.after_kms_modeset();
        assert_eq!(
            fresh,
            CopiedDestinationOwnership::ForeignPendingKmsUninitialized
        );
        let fresh_retired = fresh.after_kms_retirement(0).expect("fresh retirement");
        assert_eq!(
            fresh_retired,
            CopiedDestinationOwnership::ForeignImportedFirstUse
        );
        assert_eq!(
            fresh_retired.foreign_acquire_layouts(),
            Some((vk::ImageLayout::UNDEFINED, vk::ImageLayout::GENERAL))
        );

        let produced = CopiedDestinationOwnership::ForeignRetiredByKms.after_kms_modeset();
        assert_eq!(
            produced,
            CopiedDestinationOwnership::ForeignPendingKmsFromSink
        );
        let produced_retired = produced
            .after_kms_retirement(1)
            .expect("sink-produced retirement");
        assert_eq!(
            produced_retired,
            CopiedDestinationOwnership::ForeignRetiredByKms
        );
        assert_eq!(
            produced_retired.foreign_acquire_layouts(),
            Some((vk::ImageLayout::GENERAL, vk::ImageLayout::GENERAL))
        );
    }

    #[test]
    fn successful_or_failed_export_controls_binary_semaphore_reuse() {
        let mut state = ExportSemaphoreReuseState::Reusable;
        state.begin_post_submit_export();
        assert!(state.needs_rearm());
        state.finish_successful_export();
        assert!(!state.needs_rearm());
    }

    #[test]
    fn prime_capability_bits_keep_import_and_export_distinct() {
        assert_eq!(
            support_from_prime_bits(DRM_PRIME_CAP_IMPORT, DRM_PRIME_CAP_IMPORT),
            ScanoutMetadataSupport::Supported
        );
        assert_eq!(
            support_from_prime_bits(DRM_PRIME_CAP_IMPORT, DRM_PRIME_CAP_EXPORT),
            ScanoutMetadataSupport::Unsupported
        );
        assert_eq!(
            support_from_prime_bits(DRM_PRIME_CAP_EXPORT, DRM_PRIME_CAP_IMPORT),
            ScanoutMetadataSupport::Unsupported
        );
        assert_eq!(
            support_from_prime_bits(DRM_PRIME_CAP_EXPORT, DRM_PRIME_CAP_EXPORT),
            ScanoutMetadataSupport::Supported
        );
    }

    #[test]
    fn direction_metadata_uses_kms_export_for_output_and_import_for_renderer() {
        let metadata = build_dmabuf_scanout_metadata(
            ScanoutMetadataSupport::Supported,   // VK_KHR_external_memory_fd
            ScanoutMetadataSupport::Supported,   // KMS PRIME import
            ScanoutMetadataSupport::Unsupported, // KMS PRIME export
            (
                vec![TILED_A],
                ScanoutMetadataSupport::Supported, // Vulkan import
            ),
            (
                vec![TILED_B],
                ScanoutMetadataSupport::Supported, // Vulkan export
            ),
            (
                ScanoutMetadataSupport::Supported, // Vulkan imports GBM LINEAR
                KmsLinearLayout::ExplicitModifier,
            ),
            (
                ScanoutMetadataSupport::Supported, // Vulkan exports linear image
                KmsLinearLayout::ExplicitModifier,
            ),
        );

        assert_eq!(
            metadata.vulkan_external_memory_fd,
            ScanoutMetadataSupport::Supported
        );
        assert_eq!(
            metadata.output_owned.kms_prime,
            ScanoutMetadataSupport::Unsupported
        );
        assert_eq!(metadata.output_owned.modifiers, vec![TILED_A]);
        assert_eq!(
            metadata.output_owned.linear.path,
            ScanoutMetadataSupport::Unsupported
        );
        assert_eq!(
            metadata.output_owned.modifier_path,
            ScanoutMetadataSupport::Unsupported
        );
        assert_eq!(
            metadata.renderer_owned.kms_prime,
            ScanoutMetadataSupport::Supported
        );
        assert_eq!(metadata.renderer_owned.modifiers, vec![TILED_B]);
        assert_eq!(
            metadata.renderer_owned.linear.path,
            ScanoutMetadataSupport::Supported
        );
        assert_eq!(
            metadata.renderer_owned.modifier_path,
            ScanoutMetadataSupport::Supported
        );
    }

    #[test]
    fn linear_layout_distinguishes_legacy_unknown_from_not_advertised() {
        let legacy = kms_linear_layout(&[]);
        assert_eq!(legacy, KmsLinearLayout::LegacyAddfb);
        assert_eq!(
            kms_linear_layout_support(legacy),
            ScanoutMetadataSupport::Unknown
        );

        let explicit = kms_linear_layout(&[TILED_A, LINEAR]);
        assert_eq!(explicit, KmsLinearLayout::ExplicitModifier);
        assert_eq!(
            kms_linear_layout_support(explicit),
            ScanoutMetadataSupport::Supported
        );

        let absent_from_known_list = kms_linear_layout(&[TILED_A]);
        assert_eq!(absent_from_known_list, KmsLinearLayout::NotAdvertised);
        assert_eq!(
            kms_linear_layout_support(absent_from_known_list),
            ScanoutMetadataSupport::Unsupported
        );
    }

    #[test]
    fn renderer_owned_legacy_linear_evidence_remains_unknown_and_attemptable() {
        let linear = build_linear_metadata(
            ScanoutMetadataSupport::Supported,
            ScanoutMetadataSupport::Supported,
            KmsLinearLayout::LegacyAddfb,
        );
        assert_eq!(linear.kms_layout, KmsLinearLayout::LegacyAddfb);
        assert_eq!(linear.path, ScanoutMetadataSupport::Unknown);
    }

    #[test]
    fn absent_or_failed_modifier_metadata_stays_unknown() {
        let absent = classify_modifier_observations(false, &[]);
        assert_eq!(absent, (Vec::new(), ScanoutMetadataSupport::Unknown));

        let failed =
            classify_modifier_observations(true, &[(TILED_A, ScanoutMetadataSupport::Unknown)]);
        assert_eq!(failed, (Vec::new(), ScanoutMetadataSupport::Unknown));
    }

    #[test]
    fn conclusive_modifier_observations_remain_tri_state() {
        let unsupported = classify_modifier_observations(
            true,
            &[
                (TILED_A, ScanoutMetadataSupport::Unsupported),
                (TILED_B, ScanoutMetadataSupport::Unsupported),
            ],
        );
        assert_eq!(
            unsupported,
            (Vec::new(), ScanoutMetadataSupport::Unsupported)
        );

        let partly_known = classify_modifier_observations(
            true,
            &[
                (TILED_A, ScanoutMetadataSupport::Unknown),
                (TILED_B, ScanoutMetadataSupport::Supported),
            ],
        );
        assert_eq!(
            partly_known,
            (vec![TILED_B], ScanoutMetadataSupport::Supported)
        );
    }

    #[test]
    fn unknown_prerequisite_never_becomes_false() {
        assert_eq!(
            combine_required_metadata(
                ScanoutMetadataSupport::Supported,
                ScanoutMetadataSupport::Unknown,
            ),
            ScanoutMetadataSupport::Unknown
        );
        assert_eq!(
            combine_required_metadata(
                ScanoutMetadataSupport::Unknown,
                ScanoutMetadataSupport::Supported,
            ),
            ScanoutMetadataSupport::Unknown
        );
    }

    #[test]
    fn different_route_blocks_only_when_both_directions_are_unsupported() {
        use ScanoutMetadataSupport::{Supported, Unknown, Unsupported};

        for output_status in [Supported, Unsupported, Unknown] {
            for renderer_status in [Supported, Unsupported, Unknown] {
                let verdict = classify_route_from_direction_verdicts(
                    RenderKmsRelationship::Different,
                    Supported,
                    test_direction_verdict(output_status, DmabufAllocationDirection::OutputOwned),
                    test_direction_verdict(
                        renderer_status,
                        DmabufAllocationDirection::RendererOwned,
                    ),
                );
                let should_block = output_status == Unsupported && renderer_status == Unsupported;
                assert_eq!(
                    matches!(verdict, DmabufScanoutVerdict::Incompatible(_)),
                    should_block,
                    "output={output_status:?} renderer={renderer_status:?} verdict={verdict:?}"
                );
                if output_status == Supported || renderer_status == Supported {
                    assert_eq!(verdict, DmabufScanoutVerdict::Compatible);
                } else if !should_block {
                    assert!(matches!(verdict, DmabufScanoutVerdict::Unknown(_)));
                }
            }
        }
    }

    #[test]
    fn same_and_unknown_relationships_ignore_every_direction_status() {
        use ScanoutMetadataSupport::{Supported, Unknown, Unsupported};

        for external_memory_fd in [Supported, Unsupported, Unknown] {
            for output_status in [Supported, Unsupported, Unknown] {
                for renderer_status in [Supported, Unsupported, Unknown] {
                    let output = test_direction_verdict(
                        output_status,
                        DmabufAllocationDirection::OutputOwned,
                    );
                    let renderer = test_direction_verdict(
                        renderer_status,
                        DmabufAllocationDirection::RendererOwned,
                    );
                    assert_eq!(
                        classify_route_from_direction_verdicts(
                            RenderKmsRelationship::Same,
                            external_memory_fd,
                            output,
                            renderer,
                        ),
                        DmabufScanoutVerdict::Compatible,
                    );
                    assert_eq!(
                        classify_route_from_direction_verdicts(
                            RenderKmsRelationship::Unknown,
                            external_memory_fd,
                            output,
                            renderer,
                        ),
                        DmabufScanoutVerdict::Unknown(vec![
                            DmabufScanoutUncertainty::RenderKmsRelationshipUnknown,
                        ]),
                    );
                }
            }
        }
    }

    #[test]
    fn asahi_shaped_export_only_or_import_only_routes_remain_attemptable() {
        use ScanoutMetadataSupport::{Supported, Unsupported};

        let output_owned_only = test_scanout_metadata(
            Supported,
            test_direction_metadata(Supported, Supported, Unsupported),
            test_direction_metadata(Unsupported, Unsupported, Unsupported),
        );
        assert_eq!(
            classify_dmabuf_scanout_route(
                test_route(RenderKmsRelationship::Different),
                &output_owned_only,
                Supported,
            ),
            DmabufScanoutVerdict::Compatible,
        );

        let renderer_owned_only = test_scanout_metadata(
            Supported,
            test_direction_metadata(Unsupported, Unsupported, Unsupported),
            test_direction_metadata(Supported, Supported, Unsupported),
        );
        assert_eq!(
            classify_dmabuf_scanout_route(
                test_route(RenderKmsRelationship::Different),
                &renderer_owned_only,
                Supported,
            ),
            DmabufScanoutVerdict::Compatible,
        );
    }

    #[test]
    fn no_shared_layout_and_query_uncertainty_still_attempt() {
        use ScanoutMetadataSupport::{Supported, Unknown, Unsupported};

        let no_shared_layout = test_scanout_metadata(
            Supported,
            test_direction_metadata(Supported, Unsupported, Unsupported),
            test_direction_metadata(Supported, Unsupported, Unsupported),
        );
        let verdict = classify_dmabuf_scanout_route(
            test_route(RenderKmsRelationship::Different),
            &no_shared_layout,
            Supported,
        );
        assert_eq!(
            verdict,
            DmabufScanoutVerdict::Unknown(vec![
                DmabufScanoutUncertainty::OutputOwnedNoAdvertisedSharedLayout,
                DmabufScanoutUncertainty::RendererOwnedNoAdvertisedSharedLayout,
            ])
        );

        let query_unknown = test_scanout_metadata(
            Supported,
            test_direction_metadata(Unknown, Unknown, Unknown),
            test_direction_metadata(Unknown, Unknown, Unknown),
        );
        assert!(matches!(
            classify_dmabuf_scanout_route(
                test_route(RenderKmsRelationship::Different),
                &query_unknown,
                Unknown,
            ),
            DmabufScanoutVerdict::Unknown(_)
        ));
    }

    #[test]
    fn gbm_unavailable_makes_output_owned_unknown_but_prime_negative_dominates() {
        use ScanoutMetadataSupport::{Supported, Unknown, Unsupported};

        let output_only = test_scanout_metadata(
            Supported,
            test_direction_metadata(Supported, Supported, Unsupported),
            test_direction_metadata(Unsupported, Unsupported, Unsupported),
        );
        assert_eq!(
            classify_dmabuf_scanout_route(
                test_route(RenderKmsRelationship::Different),
                &output_only,
                Unknown,
            ),
            DmabufScanoutVerdict::Unknown(vec![
                DmabufScanoutUncertainty::OutputOwnedGbmUnavailable,
            ])
        );
        assert_eq!(
            classify_dmabuf_scanout_route(
                test_route(RenderKmsRelationship::Same),
                &output_only,
                Unknown,
            ),
            DmabufScanoutVerdict::Compatible,
        );
        assert_eq!(
            classify_dmabuf_scanout_route(
                test_route(RenderKmsRelationship::Unknown),
                &output_only,
                Unknown,
            ),
            DmabufScanoutVerdict::Unknown(vec![
                DmabufScanoutUncertainty::RenderKmsRelationshipUnknown,
            ]),
        );

        let neither_prime_direction = test_scanout_metadata(
            Supported,
            test_direction_metadata(Unsupported, Unsupported, Unsupported),
            test_direction_metadata(Unsupported, Unsupported, Unsupported),
        );
        assert!(matches!(
            classify_dmabuf_scanout_route(
                test_route(RenderKmsRelationship::Different),
                &neither_prime_direction,
                Unknown,
            ),
            DmabufScanoutVerdict::Incompatible(
                DmabufScanoutIncompatibility::BothAllocationDirectionsUnavailable { .. }
            )
        ));
    }

    #[test]
    fn external_memory_fd_absence_blocks_only_known_different_routes() {
        use ScanoutMetadataSupport::{Supported, Unknown, Unsupported};

        let supported_direction =
            test_direction_verdict(Supported, DmabufAllocationDirection::OutputOwned);
        let unknown_direction =
            test_direction_verdict(Unknown, DmabufAllocationDirection::RendererOwned);
        assert_eq!(
            classify_route_from_direction_verdicts(
                RenderKmsRelationship::Different,
                Unsupported,
                supported_direction,
                unknown_direction,
            ),
            DmabufScanoutVerdict::Incompatible(
                DmabufScanoutIncompatibility::VulkanExternalMemoryFdUnavailable,
            )
        );
        assert_eq!(
            classify_route_from_direction_verdicts(
                RenderKmsRelationship::Same,
                Unsupported,
                supported_direction,
                unknown_direction,
            ),
            DmabufScanoutVerdict::Compatible,
        );
        assert_eq!(
            classify_route_from_direction_verdicts(
                RenderKmsRelationship::Different,
                Unknown,
                supported_direction,
                unknown_direction,
            ),
            DmabufScanoutVerdict::Unknown(vec![
                DmabufScanoutUncertainty::VulkanExternalMemoryFdUnknown,
            ]),
        );
        assert!(matches!(
            classify_route_from_direction_verdicts(
                RenderKmsRelationship::Unknown,
                Unsupported,
                supported_direction,
                unknown_direction,
            ),
            DmabufScanoutVerdict::Unknown(_)
        ));
    }

    #[test]
    fn incompatible_metadata_retains_both_direction_diagnostics_without_a_gate() {
        use ScanoutMetadataSupport::{Supported, Unsupported};

        let route = test_route(RenderKmsRelationship::Different);
        let metadata = test_scanout_metadata(
            Supported,
            test_direction_metadata(Unsupported, Unsupported, Unsupported),
            test_direction_metadata(Unsupported, Unsupported, Unsupported),
        );
        let verdict = classify_dmabuf_scanout_route(route, &metadata, Supported);
        let message = format!("{verdict:?}");
        assert!(message.contains("OutputOwnedKmsPrimeExportUnsupported"));
        assert!(message.contains("RendererOwnedKmsPrimeImportUnsupported"));
        assert!(matches!(verdict, DmabufScanoutVerdict::Incompatible(_)));
    }

    #[test]
    fn scanout_usage_matches_render_and_readback_paths() {
        let usage = scanout_image_usage();
        assert!(usage.contains(vk::ImageUsageFlags::COLOR_ATTACHMENT));
        assert!(usage.contains(vk::ImageUsageFlags::TRANSFER_SRC));
        assert!(usage.contains(vk::ImageUsageFlags::TRANSFER_DST));
        assert!(!usage.contains(vk::ImageUsageFlags::SAMPLED));
    }

    #[test]
    fn exact_plan_order_keeps_output_imports_before_renderer_exports() {
        let plans = assemble_scanout_allocation_plans(
            true,
            true,
            &[TILED_A, LINEAR],
            &[TILED_B, LINEAR],
            3440,
            true,
        );

        assert_eq!(
            plans,
            vec![
                ScanoutAllocationPlan::GbmModifier(TILED_A),
                ScanoutAllocationPlan::GbmModifier(LINEAR),
                ScanoutAllocationPlan::PaddedExplicitLinear {
                    row_pitch: padded_linear_pitch(3440),
                },
                ScanoutAllocationPlan::DrmModifier(TILED_B),
                ScanoutAllocationPlan::DrmModifier(LINEAR),
                ScanoutAllocationPlan::ExplicitLinear,
                ScanoutAllocationPlan::LegacyLinear,
            ]
        );
        assert!(
            plans[..2]
                .iter()
                .all(|plan| plan.ownership() == ScanoutOwnership::Output)
        );
        assert!(
            plans[2..]
                .iter()
                .all(|plan| plan.ownership() == ScanoutOwnership::Renderer)
        );
    }

    #[test]
    fn unavailable_gbm_removes_only_output_owned_plans() {
        let plans =
            assemble_scanout_allocation_plans(true, false, &[TILED_A], &[TILED_B], 1920, false);

        assert_eq!(
            plans,
            vec![
                ScanoutAllocationPlan::DrmModifier(TILED_B),
                ScanoutAllocationPlan::ExplicitLinear,
                ScanoutAllocationPlan::LegacyLinear,
            ]
        );
    }

    #[test]
    fn device_lost_marker_survives_pool_and_bo_context() {
        let error = scanout_io_context(
            "pool",
            scanout_io_context(
                "BO 2",
                scanout_vk_error("queue submit", vk::Result::ERROR_DEVICE_LOST),
            ),
        );
        assert!(scanout_error_is_device_lost(&error));
        assert!(!scanout_error_is_device_lost(&scanout_vk_error(
            "queue submit",
            vk::Result::ERROR_OUT_OF_DEVICE_MEMORY,
        )));
    }

    #[test]
    fn probe_teardown_accepts_success_and_device_loss_only() {
        assert!(probe_teardown_wait_completed(Ok(())));
        assert!(probe_teardown_wait_completed(Err(
            vk::Result::ERROR_DEVICE_LOST
        )));
        assert!(!probe_teardown_wait_completed(Err(
            vk::Result::ERROR_OUT_OF_HOST_MEMORY
        )));
    }

    #[test]
    fn copied_probe_gives_every_fence_a_fresh_timeout() {
        const TIMEOUT_NS: u64 = 200_000_000;
        let mut observed = Vec::new();

        for _bo_idx in 0..3 {
            for _cycle in 0..2 {
                let mut render = ProbeFenceSpy::default();
                let mut sink = ProbeFenceSpy::default();
                wait_copied_probe_fence_pair(&mut render, &mut sink, TIMEOUT_NS)
                    .expect("both scripted fences complete");
                observed.extend(render.waits);
                observed.extend(sink.waits);
                assert_eq!(render.destroyed_idle, 1);
                assert_eq!(sink.destroyed_idle, 1);
            }
        }

        assert_eq!(observed.len(), 12);
        assert!(observed.iter().all(|(timeout, _)| *timeout == TIMEOUT_NS));
        assert_eq!(
            observed
                .iter()
                .map(|(_, operation)| *operation)
                .collect::<Vec<_>>(),
            ["copied renderer probe", "copied sink probe"].repeat(6),
        );
    }

    #[test]
    fn copy_free_probe_gives_every_bo_a_fresh_timeout() {
        const TIMEOUT_NS: u64 = 200_000_000;

        for _bo_idx in 0..3 {
            let mut fence = ProbeFenceSpy::default();
            wait_copy_free_probe_fence(&mut fence, TIMEOUT_NS)
                .expect("scripted copy-free fence completes");
            assert_eq!(
                fence.waits,
                vec![(TIMEOUT_NS, "disposable scanout rendering probe")]
            );
            assert_eq!(fence.destroyed_idle, 1);
            assert_eq!(fence.abandoned, 0);
        }
    }

    #[test]
    fn disposable_probe_failure_policy_distinguishes_rejection_and_quarantine() {
        let mismatch = DisposableProbeError::from(io::Error::new(
            io::ErrorKind::InvalidData,
            "pixel mismatch",
        ));
        assert!(!mismatch.requires_quarantine());
        assert!(!mismatch.abort_candidate_search());
        assert!(!mismatch.bypass_normal_teardown());

        let safe_pre_submit_timeout = DisposableProbeError::from(io::Error::new(
            io::ErrorKind::TimedOut,
            "pre-submit operation timed out",
        ));
        assert!(!safe_pre_submit_timeout.requires_quarantine());
        assert!(!safe_pre_submit_timeout.abort_candidate_search());
        assert!(!safe_pre_submit_timeout.bypass_normal_teardown());

        let blob_cleanup = DisposableProbeError::terminal_cleanup(io::Error::other(
            "TEST_ONLY mode blob cleanup failed",
        ));
        assert!(!blob_cleanup.requires_quarantine());
        assert!(blob_cleanup.abort_candidate_search());
        assert!(
            !blob_cleanup.bypass_normal_teardown(),
            "blob failure is terminal but must still strictly clean the pool"
        );

        let uncertain = DisposableProbeError::quarantined(io::Error::other("submission unknown"))
            .with_context("BO 2 copied sink wait");
        assert!(uncertain.requires_quarantine());
        assert!(uncertain.abort_candidate_search());
        assert!(uncertain.bypass_normal_teardown());
        assert!(uncertain.to_string().contains("BO 2 copied sink wait"));
    }

    #[test]
    fn probe_fence_failure_disposition_never_waits_device_wide() {
        let cases = [
            (PendingProbeSubmissions::None, (0, 1), (0, 1), false),
            (PendingProbeSubmissions::Render, (1, 0), (0, 1), true),
            (PendingProbeSubmissions::RenderAndSink, (1, 0), (1, 0), true),
        ];

        for (pending, render_expected, sink_expected, quarantine_expected) in cases {
            let mut render = ProbeFenceSpy::default();
            let mut sink = ProbeFenceSpy::default();
            let error = finish_pending_probe_failure(
                pending,
                DisposableProbeError::from(io::Error::other("probe failed")),
                &mut render,
                &mut sink,
            );

            assert_eq!(
                (render.abandoned, render.destroyed_idle),
                render_expected,
                "render fence disposition for {pending:?}"
            );
            assert_eq!(
                (sink.abandoned, sink.destroyed_idle),
                sink_expected,
                "sink fence disposition for {pending:?}"
            );
            assert_eq!(error.requires_quarantine(), quarantine_expected);
        }
    }

    #[test]
    fn actual_probe_fence_timeouts_are_terminal_and_quarantined() {
        const TIMEOUT_NS: u64 = 200_000_000;

        let mut copy_free = ProbeFenceSpy::timing_out();
        let error = wait_copy_free_probe_fence(&mut copy_free, TIMEOUT_NS)
            .expect_err("copy-free timeout must fail");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(error.requires_quarantine());
        assert!(error.abort_candidate_search());
        assert!(error.bypass_normal_teardown());
        assert_eq!((copy_free.abandoned, copy_free.destroyed_idle), (1, 0));

        let mut render = ProbeFenceSpy::timing_out();
        let mut sink = ProbeFenceSpy::default();
        let error = wait_copied_probe_fence_pair(&mut render, &mut sink, TIMEOUT_NS)
            .expect_err("renderer timeout must fail the copied pair");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(error.requires_quarantine());
        assert_eq!((render.abandoned, render.destroyed_idle), (1, 0));
        assert_eq!((sink.abandoned, sink.destroyed_idle), (1, 0));

        let mut render = ProbeFenceSpy::default();
        let mut sink = ProbeFenceSpy::timing_out();
        let error = wait_copied_probe_fence_pair(&mut render, &mut sink, TIMEOUT_NS)
            .expect_err("sink timeout must fail the copied pair");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(error.requires_quarantine());
        assert_eq!((render.abandoned, render.destroyed_idle), (0, 1));
        assert_eq!((sink.abandoned, sink.destroyed_idle), (1, 0));
    }

    #[test]
    fn strict_drm_cleanup_orders_framebuffer_before_gem_and_preserves_failures() {
        use std::cell::Cell;

        let calls = Cell::new(0_u8);
        let mut framebuffer = Some(11_u8);
        let mut gem = Some(22_u8);
        release_drm_handles_strict(
            &mut framebuffer,
            &mut gem,
            |handle| {
                assert_eq!((handle, calls.get()), (11, 0));
                calls.set(1);
                Ok(())
            },
            |handle| {
                assert_eq!((handle, calls.get()), (22, 1));
                calls.set(2);
                Ok(())
            },
        )
        .expect("both strict cleanup ioctls succeed");
        assert_eq!((framebuffer, gem, calls.get()), (None, None, 2));

        let calls = Cell::new(0_u8);
        let mut framebuffer = Some(11_u8);
        let mut gem = Some(22_u8);
        release_drm_handles_strict(
            &mut framebuffer,
            &mut gem,
            |_| {
                calls.set(1);
                Err(io::Error::other("RMFB failed"))
            },
            |_| {
                calls.set(2);
                Ok(())
            },
        )
        .expect_err("framebuffer cleanup failure is terminal");
        assert_eq!(
            (framebuffer, gem, calls.get()),
            (Some(11), Some(22), 1),
            "GEM close must not run after RMFB failure"
        );

        let calls = Cell::new(0_u8);
        let mut framebuffer = Some(11_u8);
        let mut gem = Some(22_u8);
        release_drm_handles_strict(
            &mut framebuffer,
            &mut gem,
            |_| {
                calls.set(1);
                Ok(())
            },
            |_| {
                assert_eq!(calls.get(), 1);
                calls.set(2);
                Err(io::Error::other("GEM_CLOSE failed"))
            },
        )
        .expect_err("GEM cleanup failure is terminal");
        assert_eq!(
            (framebuffer, gem, calls.get()),
            (None, Some(22), 2),
            "successful RMFB stays cleared while the failed GEM handle is retained"
        );
    }

    #[test]
    fn production_probe_finalizer_enforces_strict_cleanup_precedence() {
        use std::{cell::Cell, rc::Rc};

        #[derive(Default)]
        struct AttemptCounts {
            known_quiescent: Cell<u8>,
            strict_cleanups: Cell<u8>,
            drops: Cell<u8>,
            device_idle_waits: Cell<u8>,
        }

        struct AttemptSpy {
            counts: Rc<AttemptCounts>,
            cleanup_fails: bool,
        }

        impl DisposableProbeAttempt for AttemptSpy {
            fn mark_known_quiescent(&self) {
                self.counts
                    .known_quiescent
                    .set(self.counts.known_quiescent.get() + 1);
            }

            fn release_strict_drm_resources(&mut self) -> io::Result<()> {
                self.counts
                    .strict_cleanups
                    .set(self.counts.strict_cleanups.get() + 1);
                if self.cleanup_fails {
                    Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "strict DRM cleanup failure",
                    ))
                } else {
                    Ok(())
                }
            }
        }

        impl Drop for AttemptSpy {
            fn drop(&mut self) {
                self.counts.drops.set(self.counts.drops.get() + 1);
                if self.counts.known_quiescent.get() == 0 {
                    self.counts
                        .device_idle_waits
                        .set(self.counts.device_idle_waits.get() + 1);
                }
            }
        }

        let cases = [
            ("success", Ok(()), false, true, false, false, (1, 1, 1, 0)),
            (
                "completed mismatch",
                completed_probe_validation(Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "pixel mismatch",
                ))),
                false,
                false,
                false,
                false,
                (1, 1, 1, 0),
            ),
            (
                "copied source allocation rejected after destination allocation",
                Err(DisposableProbeError::from(io::Error::other(
                    "copied source allocation failed",
                ))),
                false,
                false,
                false,
                false,
                (1, 1, 1, 0),
            ),
            (
                "terminal blob cleanup after strict pool cleanup",
                Err(DisposableProbeError::terminal_cleanup(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "mode blob cleanup failure",
                ))),
                false,
                false,
                true,
                false,
                (1, 1, 1, 0),
            ),
            (
                "uncertain post-submit failure",
                Err(DisposableProbeError::quarantined(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "fence timeout",
                ))),
                false,
                false,
                true,
                true,
                (0, 0, 0, 0),
            ),
            (
                "cleanup failure overrides success",
                Ok(()),
                true,
                false,
                true,
                true,
                (1, 1, 0, 0),
            ),
            (
                "cleanup failure overrides completed mismatch",
                completed_probe_validation(Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "pixel mismatch",
                ))),
                true,
                false,
                true,
                true,
                (1, 1, 0, 0),
            ),
        ];

        for (label, result, cleanup_fails, expect_ok, expect_abort, expect_quarantine, expected) in
            cases
        {
            let counts = Rc::new(AttemptCounts::default());
            let returned = finish_disposable_probe_attempt(
                AttemptSpy {
                    counts: Rc::clone(&counts),
                    cleanup_fails,
                },
                result,
            );

            assert_eq!(returned.is_ok(), expect_ok, "{label}");
            assert_eq!(
                returned
                    .as_ref()
                    .err()
                    .is_some_and(DisposableProbeError::abort_candidate_search),
                expect_abort,
                "{label}",
            );
            assert_eq!(
                returned
                    .as_ref()
                    .err()
                    .is_some_and(DisposableProbeError::requires_quarantine),
                expect_quarantine,
                "{label}",
            );
            assert_eq!(
                (
                    counts.known_quiescent.get(),
                    counts.strict_cleanups.get(),
                    counts.drops.get(),
                    counts.device_idle_waits.get(),
                ),
                expected,
                "{label}",
            );
        }
    }

    #[test]
    fn completed_probe_validation_is_authoritative() {
        assert_eq!(
            completed_probe_validation::<u8, io::Error>(Ok(7)).expect("completed matching content"),
            7
        );

        let mismatch = completed_probe_validation::<(), io::Error>(Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "pixel mismatch",
        )))
        .expect_err("completed mismatching content remains a rejection");
        assert_eq!(mismatch.kind(), io::ErrorKind::InvalidData);
        assert!(!mismatch.requires_quarantine());
        assert!(!mismatch.abort_candidate_search());
        assert!(!mismatch.bypass_normal_teardown());
    }

    #[test]
    fn digest_readback_infrastructure_failure_is_not_route_rejection() {
        let invalidation = copied_probe_digest_readback_error(
            "test digest invalidate",
            vk::Result::ERROR_OUT_OF_HOST_MEMORY,
        );
        assert!(!invalidation.requires_quarantine());
        assert!(invalidation.abort_candidate_search());
        assert!(!invalidation.bypass_normal_teardown());

        let device_lost = copied_probe_digest_readback_error(
            "test digest invalidate",
            vk::Result::ERROR_DEVICE_LOST,
        );
        assert!(!device_lost.requires_quarantine());
        assert!(!device_lost.abort_candidate_search());
        assert!(scanout_error_is_device_lost(device_lost.as_io_error()));
    }

    #[test]
    fn disarmed_owned_backing_is_forgotten_instead_of_dropped() {
        use std::{cell::Cell, rc::Rc};

        struct DropSpy(Rc<Cell<u32>>);
        impl Drop for DropSpy {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }

        let drops = Rc::new(Cell::new(0));
        let mut backing = Some(DropSpy(Rc::clone(&drops)));
        leak_owned_backing(&mut backing);

        assert!(backing.is_none());
        assert_eq!(drops.get(), 0);
    }

    #[test]
    fn modifier_order_prefers_tiled_over_linear() {
        // KMS advertises linear first, then a tiled modifier; both are
        // Vulkan-supported and exportable. Tiled must win — issue #48.
        let candidates = order_scanout_modifier_candidates(
            &[LINEAR, TILED_A],
            &[LINEAR, TILED_A],
            false,
            |_| true,
        );
        assert_eq!(
            candidates,
            vec![TILED_A, LINEAR],
            "tiled modifier must precede LINEAR"
        );
    }

    #[test]
    fn modifier_order_keeps_kms_order_among_tiled() {
        let candidates = order_scanout_modifier_candidates(
            &[TILED_B, TILED_A, LINEAR],
            &[TILED_A, TILED_B, LINEAR],
            false,
            |_| true,
        );
        assert_eq!(candidates, vec![TILED_B, TILED_A, LINEAR]);
    }

    #[test]
    fn modifier_order_drops_tiled_not_supported_by_vulkan() {
        let candidates =
            order_scanout_modifier_candidates(&[TILED_A, LINEAR], &[LINEAR], false, |_| true);
        assert_eq!(candidates, vec![LINEAR], "TILED_A not in the Vulkan set");
    }

    #[test]
    fn modifier_order_drops_non_exportable_tiled() {
        // A multi-plane (DCC) tiled modifier fails the single-plane
        // exportability check and must be skipped, leaving LINEAR.
        let candidates =
            order_scanout_modifier_candidates(&[TILED_A, LINEAR], &[TILED_A, LINEAR], false, |m| {
                m != TILED_A
            });
        assert_eq!(candidates, vec![LINEAR]);
    }

    #[test]
    fn modifier_order_linear_only_plane_yields_linear() {
        let candidates = order_scanout_modifier_candidates(&[LINEAR], &[LINEAR], false, |_| true);
        assert_eq!(candidates, vec![LINEAR]);
    }

    #[test]
    fn modifier_order_drops_linear_without_directional_support() {
        let candidates = order_scanout_modifier_candidates(&[LINEAR], &[LINEAR], false, |_| false);
        assert!(candidates.is_empty());
    }

    #[test]
    fn modifier_order_empty_when_no_intersection() {
        let candidates = order_scanout_modifier_candidates(&[TILED_A], &[TILED_B], false, |_| true);
        assert!(candidates.is_empty());
    }

    #[test]
    fn linear_scanout_stride_alignment_matches_hw_observations() {
        // GTX 1050 @ 2560 wide → pitch 10240 = 256×40 → aligned → LINEAR OK.
        assert!(linear_scanout_stride_aligned(2560));
        // GTX 1060 @ 3440 ultrawide → pitch 13760, mod 256 = 192 → unaligned.
        assert!(!linear_scanout_stride_aligned(3440));
        // Common aligned widths.
        assert!(linear_scanout_stride_aligned(1920)); // 7680 = 256×30
        assert!(linear_scanout_stride_aligned(1280)); // 5120 = 256×20
        assert!(linear_scanout_stride_aligned(3840)); // 15360 = 256×60 (4K)
        // 1366 laptop → 5464, mod 256 = 88 → unaligned.
        assert!(!linear_scanout_stride_aligned(1366));
    }

    #[test]
    fn padded_linear_pitch_rounds_up_to_alignment() {
        // 3440 → tight 13760 → padded up to 13824 = 256×54.
        assert_eq!(padded_linear_pitch(3440), 13824);
        assert!(padded_linear_pitch(3440).is_multiple_of(SCANOUT_PITCH_ALIGN));
        // Already-aligned widths are unchanged.
        assert_eq!(padded_linear_pitch(2560), 10240); // 256×40
        assert_eq!(padded_linear_pitch(1920), 7680); // 256×30
        // 1366 → tight 5464 → padded 5632 = 256×22.
        assert_eq!(padded_linear_pitch(1366), 5632);
    }

    #[test]
    fn scanout_linear_policy_covers_dithering_drivers_not_amd() {
        use super::vk::DriverId;
        // HW-confirmed dithering on tiled scanout → must prefer LINEAR.
        assert!(scanout_prefers_linear(DriverId::NVIDIA_PROPRIETARY));
        assert!(scanout_prefers_linear(DriverId::INTEL_OPEN_SOURCE_MESA));
        // AMD needs tiled (RDNA4 LINEAR corrupts, issue #48) — must NOT prefer.
        assert!(!scanout_prefers_linear(DriverId::MESA_RADV));
        assert!(!scanout_prefers_linear(DriverId::AMD_PROPRIETARY));
        // Unconfirmed drivers stay on the tiled-first default.
        assert!(!scanout_prefers_linear(DriverId::MESA_LLVMPIPE));
    }

    // NVIDIA prefer_linear path — mirrors what GBM does on Pascal (GTX 1050):
    // LINEAR selected first even though tiled modifiers are also advertised.
    #[test]
    fn modifier_order_nvidia_prefers_linear_over_tiled() {
        // NVIDIA KMS advertises tiled first, then LINEAR — but prefer_linear=true
        // must put LINEAR at the front.
        let candidates = order_scanout_modifier_candidates(
            &[TILED_A, TILED_B, LINEAR],
            &[TILED_A, TILED_B, LINEAR],
            true,
            |_| true,
        );
        assert_eq!(candidates, vec![LINEAR, TILED_A, TILED_B]);
    }

    #[test]
    fn modifier_order_nvidia_linear_only_plane_yields_linear() {
        let candidates = order_scanout_modifier_candidates(&[LINEAR], &[LINEAR], true, |_| true);
        assert_eq!(candidates, vec![LINEAR]);
    }

    #[test]
    fn modifier_order_nvidia_no_linear_falls_back_to_tiled() {
        // If LINEAR is absent from the KMS plane, even prefer_linear=true should
        // yield the tiled modifiers (they're the only option).
        let candidates = order_scanout_modifier_candidates(
            &[TILED_A, TILED_B],
            &[TILED_A, TILED_B],
            true,
            |_| true,
        );
        assert_eq!(candidates, vec![TILED_A, TILED_B]);
    }

    #[test]
    fn fresh_bo_is_free() {
        let bo = BoState::default();
        assert_eq!(bo.phase, BoPhase::Free);
        assert!(bo.in_fence_fd.is_none());
        assert!(bo.release_fence_fd.is_none());
    }

    #[test]
    fn synchronous_modeset_reserves_its_front_buffer_without_waiting_for_an_event() {
        let mut bo = BoState::default();

        bo.mark_on_screen_after_modeset();

        assert_eq!(bo.phase, BoPhase::OnScreen);
        assert!(bo.in_fence_fd.is_none());
        assert!(bo.release_fence_fd.is_none());
    }

    #[test]
    fn record_then_submit_transitions_to_submitted() {
        let mut bo = BoState::default();
        bo.transition_to_recording();
        assert_eq!(bo.phase, BoPhase::Recording);
        bo.transition_to_submitted(/* in_fence */ 42);
        assert_eq!(bo.phase, BoPhase::Submitted);
        assert_eq!(bo.in_fence_fd, Some(42));
    }

    #[test]
    fn submit_with_no_fence_sentinel_does_not_store_fd() {
        let mut bo = BoState::default();
        bo.transition_to_recording();
        bo.transition_to_submitted(/* no fence */ -1);
        assert_eq!(bo.phase, BoPhase::Submitted);
        assert!(bo.in_fence_fd.is_none());
    }

    #[test]
    fn atomic_accept_returns_in_fence_for_caller_to_close_and_stores_out_fence() {
        let mut bo = BoState::default();
        bo.transition_to_recording();
        bo.transition_to_submitted(42);
        let reclaimed = bo.transition_to_pending(/* out_fence */ 99);
        assert_eq!(bo.phase, BoPhase::Pending);
        assert_eq!(
            reclaimed,
            Some(42),
            "caller closes the in-fence fd; kernel only refs the sync_file"
        );
        assert!(bo.in_fence_fd.is_none(), "moved out into reclaimed");
        assert_eq!(bo.release_fence_fd, Some(99));
    }

    #[test]
    fn atomic_accept_with_no_out_fence_sentinel_does_not_store_release_fd() {
        let mut bo = BoState::default();
        bo.transition_to_recording();
        bo.transition_to_submitted(42);
        let reclaimed = bo.transition_to_pending(/* no out fence */ -1);
        assert_eq!(reclaimed, Some(42));
        assert!(bo.release_fence_fd.is_none());
    }

    #[test]
    fn atomic_reject_returns_to_recording_and_we_still_own_in_fence() {
        let mut bo = BoState::default();
        bo.transition_to_recording();
        bo.transition_to_submitted(42);
        let reclaimed = bo.transition_to_recording_after_atomic_reject();
        assert_eq!(bo.phase, BoPhase::Recording);
        assert_eq!(reclaimed, Some(42), "caller closes the fd");
        assert!(bo.in_fence_fd.is_none(), "moved out into reclaimed");
    }

    #[test]
    fn modeset_preempt_from_submitted_returns_in_fence() {
        let mut bo = BoState::default();
        bo.transition_to_recording();
        bo.transition_to_submitted(7);
        let in_fence = bo.transition_to_free_after_modeset_preempt();
        assert_eq!(bo.phase, BoPhase::Free);
        assert_eq!(in_fence, Some(7));
    }

    #[test]
    fn pending_then_onscreen_then_retiring_then_free_releases_fence() {
        let mut bo = BoState::default();
        bo.transition_to_recording();
        bo.transition_to_submitted(11);
        let _ = bo.transition_to_pending(22);
        assert_eq!(bo.phase, BoPhase::Pending);

        bo.transition_to_on_screen();
        assert_eq!(bo.phase, BoPhase::OnScreen);
        assert_eq!(
            bo.release_fence_fd,
            Some(22),
            "release fence stays attached while on-screen"
        );

        bo.transition_to_retiring();
        assert_eq!(bo.phase, BoPhase::Retiring);

        let release = bo.transition_to_free_after_retire();
        assert_eq!(bo.phase, BoPhase::Free);
        assert_eq!(release, Some(22), "caller closes the release fence");
        assert!(bo.release_fence_fd.is_none());
    }

    /// 4.1.2.7 fence-cycle integration test (host-pure variant per
    /// the plan: "mock the GPU side if Vulkan creation under
    /// lavapipe is awkward inside `cargo test`"). Drives a 3-bo
    /// pool through 6 frames in the steady-state cycle and asserts
    /// every fence fd issued is closed exactly once. No real GPU.
    /// The accounting catches state-machine bugs that leak fence
    /// fds (which is exactly the class of bug we hit on bare metal
    /// with the original IN_FENCE_FD ownership confusion).
    #[test]
    fn six_frames_cycle_through_pool_without_leaking_fences() {
        let mut bos: Vec<BoState> = (0..3).map(|_| BoState::default()).collect();
        let mut issued = 0u32;
        let mut closed = 0u32;
        let mut next_fd = 100i32;

        let alloc_fd = |issued: &mut u32, next_fd: &mut i32| -> i32 {
            *issued += 1;
            let fd = *next_fd;
            *next_fd += 1;
            fd
        };
        let close = |fd: Option<i32>, closed: &mut u32| {
            if fd.is_some() {
                *closed += 1;
            }
        };

        for _frame in 0..6 {
            // 1. Acquire Free bo and submit.
            let bo_idx = bos.iter().position(|b| b.phase == BoPhase::Free).expect(
                "with 3 bos and the cycle-advance below, at least one bo \
                 should be Free every frame",
            );
            let bo = &mut bos[bo_idx];
            bo.transition_to_recording();
            let in_fence = alloc_fd(&mut issued, &mut next_fd);
            bo.transition_to_submitted(in_fence);

            // 2. Atomic accept → Pending; closes the in-fence we just
            //    issued.
            let out_fence = alloc_fd(&mut issued, &mut next_fd);
            close(bo.transition_to_pending(out_fence), &mut closed);

            // 3. Pageflip-complete advance (mirrors
            //    `advance_pool_on_pageflip_complete` in backend.rs).
            let phases: Vec<BoPhase> = bos.iter().map(|b| b.phase).collect();
            for (i, phase) in phases.into_iter().enumerate() {
                match phase {
                    BoPhase::Retiring => {
                        close(bos[i].transition_to_free_after_retire(), &mut closed);
                    }
                    BoPhase::OnScreen => bos[i].transition_to_retiring(),
                    BoPhase::Pending => bos[i].transition_to_on_screen(),
                    _ => {}
                }
            }
        }

        // Drain remaining bos (simulates shutdown).
        for bo in &mut bos {
            let r = bo.transition_to_free_after_modeset_reset();
            close(r.in_fence, &mut closed);
            close(r.release_fence, &mut closed);
        }

        assert_eq!(
            issued, closed,
            "every fence fd issued must be closed exactly once \
             (issued={issued}, closed={closed})"
        );
        assert_eq!(
            issued, 12,
            "6 frames × (1 in_fence + 1 release_fence) = 12 fds expected"
        );
    }

    #[test]
    fn modeset_reset_returns_all_currently_held_fences() {
        // Pending: in-fence already returned to caller for closing,
        // live release fence still held by the bo.
        let mut bo = BoState::default();
        bo.transition_to_recording();
        bo.transition_to_submitted(5);
        let _ = bo.transition_to_pending(60);
        let released = bo.transition_to_free_after_modeset_reset();
        assert_eq!(bo.phase, BoPhase::Free);
        assert_eq!(released.in_fence, None);
        assert_eq!(released.release_fence, Some(60));

        // Submitted: still own the in-fence.
        let mut bo = BoState::default();
        bo.transition_to_recording();
        bo.transition_to_submitted(5);
        let released = bo.transition_to_free_after_modeset_reset();
        assert_eq!(released.in_fence, Some(5));
        assert_eq!(released.release_fence, None);

        // Recording: nothing held.
        let mut bo = BoState::default();
        bo.transition_to_recording();
        let released = bo.transition_to_free_after_modeset_reset();
        assert_eq!(released.in_fence, None);
        assert_eq!(released.release_fence, None);
    }

    #[test]
    fn addfb_modifier_flag_tracks_modifier_presence() {
        assert_eq!(addfb_flags_for_modifier(None), FbCmd2Flags::empty());
        assert_eq!(
            addfb_flags_for_modifier(Some(crate::kms::vk::dri3::DRM_FORMAT_MOD_LINEAR)),
            FbCmd2Flags::MODIFIERS
        );
    }

    // ── YSERVER_SCANOUT_MODIFIER override ────────────────────────────

    #[test]
    fn modifier_override_parses_order_keywords() {
        assert_eq!(
            parse_scanout_modifier_override("tiled-first"),
            Some(ScanoutModifierOverride::TiledFirst)
        );
        assert_eq!(
            parse_scanout_modifier_override("linear-first"),
            Some(ScanoutModifierOverride::LinearFirst)
        );
        // Underscores and case are accepted — this is typed by hand on a
        // console during a hardware triage round.
        assert_eq!(
            parse_scanout_modifier_override("TILED_FIRST"),
            Some(ScanoutModifierOverride::TiledFirst)
        );
        assert_eq!(
            parse_scanout_modifier_override("  Linear-First  "),
            Some(ScanoutModifierOverride::LinearFirst)
        );
    }

    #[test]
    fn modifier_override_parses_explicit_modifier() {
        // The block-linear modifier an RTX 3060 Ti / driver 595 actually
        // scans out with (issue #32 telemetry).
        assert_eq!(
            parse_scanout_modifier_override("0x300000000606015"),
            Some(ScanoutModifierOverride::First(0x0300_0000_0060_6015))
        );
        // Bare hex (no 0x) is accepted: the log prints values with 0x, but
        // a copy-paste that loses the prefix should still work.
        assert_eq!(
            parse_scanout_modifier_override("300000000606015"),
            Some(ScanoutModifierOverride::First(0x0300_0000_0060_6015))
        );
        assert_eq!(
            parse_scanout_modifier_override("0X0"),
            Some(ScanoutModifierOverride::First(LINEAR))
        );
    }

    #[test]
    fn modifier_override_rejects_garbage_without_panicking() {
        // A typo must not take the display server down: unparseable values
        // are ignored (with a warning) and the driver policy stands.
        assert_eq!(parse_scanout_modifier_override(""), None);
        assert_eq!(parse_scanout_modifier_override("   "), None);
        assert_eq!(parse_scanout_modifier_override("tiled"), None);
        assert_eq!(parse_scanout_modifier_override("0xzz"), None);
        // Wider than u64.
        assert_eq!(
            parse_scanout_modifier_override("0x1_0000_0000_0000_0000"),
            None
        );
    }

    #[test]
    fn modifier_override_forces_order_against_driver_policy() {
        use super::vk::DriverId;
        // No override → driver policy stands (NVIDIA prefers LINEAR).
        assert!(resolve_prefer_linear(DriverId::NVIDIA_PROPRIETARY, None));
        assert!(!resolve_prefer_linear(DriverId::MESA_RADV, None));
        // tiled-first overrides NVIDIA's LINEAR preference — this is the
        // knob that answers "does GBM tiled scan out clean on Pascal?".
        assert!(!resolve_prefer_linear(
            DriverId::NVIDIA_PROPRIETARY,
            Some(ScanoutModifierOverride::TiledFirst)
        ));
        // linear-first overrides AMD's tiled preference (reproduces #48).
        assert!(resolve_prefer_linear(
            DriverId::MESA_RADV,
            Some(ScanoutModifierOverride::LinearFirst)
        ));
        // An explicit modifier does not change the LINEAR-vs-tiled policy
        // for the REST of the list; hoisting handles the pinned entry.
        assert!(resolve_prefer_linear(
            DriverId::NVIDIA_PROPRIETARY,
            Some(ScanoutModifierOverride::First(TILED_A))
        ));
    }

    #[test]
    fn modifier_override_hoists_pinned_modifier_to_front() {
        // Present in the list → moved to front, relative order preserved.
        let mut candidates = vec![LINEAR, TILED_A, TILED_B];
        hoist_modifier_first(&mut candidates, TILED_B);
        assert_eq!(candidates, vec![TILED_B, LINEAR, TILED_A]);

        // Already first → unchanged.
        let mut candidates = vec![TILED_A, LINEAR];
        hoist_modifier_first(&mut candidates, TILED_A);
        assert_eq!(candidates, vec![TILED_A, LINEAR]);

        // Absent → prepended anyway. The GBM plan checks importability at
        // allocation time and falls through cleanly, so pinning a modifier
        // the Vulkan side did not advertise is a legal experiment rather
        // than a boot failure.
        let mut candidates = vec![LINEAR];
        hoist_modifier_first(&mut candidates, TILED_A);
        assert_eq!(candidates, vec![TILED_A, LINEAR]);

        // Empty candidate list (no modifier survived the filters) still
        // yields the pinned modifier for the GBM path to try.
        let mut candidates = Vec::new();
        hoist_modifier_first(&mut candidates, TILED_A);
        assert_eq!(candidates, vec![TILED_A]);
    }
}
