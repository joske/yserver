//! `PlatformBackend` — hardware + OS surface for the v2 renderer.
//!
//! Per rendering-model-v2 spec § "PlatformBackend — hardware + OS
//! surface" and Stage 2 plan
//! (`docs/superpowers/plans/2026-05-16-stage-2.md`) substage 2a.
//! Owns the DRM device, KMS outputs, libinput context, Vulkan
//! device, command pool, recyclable fence pool, and per-output
//! scanout BO pools (with v2's per-BO generation tracking for
//! the buffer-age algorithm).
//!
//! Exposes the **two-sync-object** API the v2 model needs:
//! [`FenceTicket`] for CPU-side resource lifetime (I6a), and the
//! per-`ScanoutBo` long-lived `vk_semaphore` (consumed by KMS
//! `IN_FENCE_FD`) for the page-flip kernel wait. The
//! `KmsSyncSemaphore` wrapper from the Stage 2 plan turned out
//! to be unnecessary — `ScanoutBoPool` already owns reusable
//! per-BO export semaphores, so v2 reuses those directly.
//! Stage 2a's commit message records this departure.
//!
//! `KmsBackend` holds `platform: PlatformBackend` and
//! delegates DRM / Vk / libinput access through it. Paint paths
//! still log gaps in Stage 2a; the real `DrawableStore` /
//! `RenderEngine` / `SceneCompositor` arrive in Stage 2b–2e.
//!
//! Several APIs introduced here (`FenceTicket`, `FencePool`,
//! `ScanoutBoToken`, `PageFlipRetirement`, `invalidate_bo`,
//! `record_present`, `commit_bo_present`) are dead-code in 2a —
//! they're the surface 2b–2e consume. The dead-code allowances
//! below get retired one at a time as later substages land.

#![allow(
    dead_code,
    reason = "FenceTicket / scanout BO primitives are consumed by Stages 2b–2e"
)]

mod connectors;
mod cursor;
mod devices;
mod fence;
mod flip_events;
mod init;
mod power;
mod qualify;
mod scanout_bos;
mod storage_alloc;
mod submit;

#[cfg(test)]
use connectors::*;
use cursor::*;
use fence::*;
use init::*;
pub(crate) use qualify::*;
use scanout_bos::*;

#[cfg(test)]
mod tests;

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    io,
    os::fd::{AsFd, AsRawFd, IntoRawFd, OwnedFd, RawFd},
    path::PathBuf,
    rc::{Rc, Weak},
    sync::Arc,
};

use ash::vk;
use yserver_core::backend::{BackendFdKind, PresentClockSample, PresentClockSource};

use crate::{
    drm,
    kms::{
        backend::{
            ActiveOutput, OutputKey, PlatformInit, PlatformInitOutput,
            platform_init as core_platform_init,
        },
        render::{
            store::Storage,
            submit_group::{FlushReason, SubmitGroup},
        },
        scanout_route::{RenderKmsRelationship, ScanoutRoute},
        vk::{
            device::{VkContext, VkInitError, VulkanDeviceSelector},
            ops::{OpsCommandPool, submit_error_may_leave_pending},
            scanout::{
                BoPhase, BoState, CopiedScanoutPlan, CopiedScanoutPool, DisposableProbeError,
                OutputScanout, ScanoutAllocationPlan, ScanoutBoPool,
            },
        },
    },
};

pub(crate) use crate::kms::scanout_route::RenderDeviceId;

// ────────────────────────────────────────────────────────────────
// FenceTicket — CPU-side I6a lifetime ticket.
//
// One `FenceTicket` per submission, cloneable across consumers.
// Wraps an `Rc<FenceTicketInner>` so the underlying `vk::Fence`
// survives until every consumer drops its clone. On the final
// drop, if the fence has been observed signaled, it's recycled
// back to the platform's pool; otherwise it leaks (and a
// renderer_failed flag is set), since recycling an unsignaled
// fence whose GPU work might still reference resources would
// be a use-after-free.
//
// Per Stage 2 plan cross-cutting §1.
// ────────────────────────────────────────────────────────────────

/// A submission's CPU-side lifetime ticket. Cloneable; each
/// clone holds a refcount on the inner. The underlying
/// `vk::Fence` is returned to the platform's pool on the
/// final-drop iff it has been observed signaled.
///
/// Backend ownership is single-threaded, so this uses `Rc`/`Cell`/
/// `RefCell` rather than thread-safe refcounting, atomics, and mutexes.
#[derive(Clone, Debug)]
pub(crate) struct FenceTicket {
    inner: Rc<FenceTicketInner>,
}

struct FenceTicketInner {
    fence: vk::Fence,
    /// Set on the first `poll_signaled` that observes
    /// `vk::SUCCESS`. After this, `poll_signaled` short-circuits
    /// without calling the driver.
    signaled_cache: Cell<bool>,
    /// Weak handle to the platform's fence pool. On `Drop`, if
    /// the fence is signaled AND the pool still exists, return
    /// the fence handle to the pool. If not signaled, leak the
    /// fence handle and set `renderer_failed` on the platform.
    pool: Weak<RefCell<FencePoolInner>>,
    /// Strong ref to the `VkContext` so the `Drop` fallback path
    /// can call `destroy_fence` directly when the pool is already
    /// gone. Mirrors [`PresentCompletionSignal`]'s pattern. The
    /// triggering case is `KmsBackend`'s field-drop order:
    /// `platform` (which contains `fence_pool`) is declared before
    /// `store` / `engine` / `scene`, all of which hold
    /// `FenceTicket`s; those tickets only release after the pool
    /// is gone, so without this ref each one would leak a `VkFence`
    /// handle (1471 leaked at SIGTERM observed on bee/MATE
    /// 2026-05-31). Holding a strong `Arc<VkContext>` keeps the
    /// device alive at least until the last ticket destroys its
    /// fence; the device's other Arcs (one per pool/pipeline)
    /// guarantee `destroy_device` only fires after every ticket
    /// has released. `None` only for the test-only `for_tests_stub`
    /// constructor which has no real device available.
    vk: Option<Arc<VkContext>>,
    /// Semaphores this submission names that must outlive it: the
    /// temporary SYNC_FD payloads it waits on, and the exportable
    /// signal semaphore of a GLX-TFP write publish
    /// ([`FenceTicket::retain_signal_semaphore`]). Vulkan requires each
    /// handle to remain alive until the queue operation retires
    /// (VUID-vkDestroySemaphore-semaphore-05149), so these share the
    /// submission fence's lifetime rather than being destroyed
    /// immediately after submit.
    imported_wait_semaphores: RefCell<Vec<vk::Semaphore>>,
}

/// Export-only binary semaphore for deferred PRESENT completion.
///
/// This object is deliberately separate from [`FenceTicket`].
/// Exporting a sync fd is allowed to affect the source payload, so
/// PRESENT completion uses this disposable semaphore while yserver's
/// internal lifetime bookkeeping continues to poll the untouched
/// `FenceTicket`.
pub(crate) struct PresentCompletionSignal {
    vk: Arc<VkContext>,
    semaphore: vk::Semaphore,
}

// ────────────────────────────────────────────────────────────────
// FencePool — recyclable VkFence allocator.
//
// Simple stack: `acquire` either pops a recycled (already-reset)
// fence or creates a new one; `recycle` pushes back after
// resetting the fence. `Drop` walks the entire pool (including
// leaked unsignaled handles) and destroys each fence.
// ────────────────────────────────────────────────────────────────

pub(crate) struct FencePool {
    inner: Rc<RefCell<FencePoolInner>>,
}

struct FencePoolInner {
    vk: Arc<VkContext>,
    /// Free list of fences known to be in the unsignaled
    /// (reset) state, ready to be passed to `vkQueueSubmit2`.
    free: Vec<vk::Fence>,
    /// Handles deliberately leaked because they were dropped
    /// while still potentially in flight. Destroyed only at
    /// `Drop` after `vkDeviceWaitIdle`.
    leaked_fences: Vec<vk::Fence>,
    /// Set when `FenceTicketInner::Drop` observes an unsignaled
    /// fence — the renderer is no longer safe to continue.
    renderer_failed: bool,
}

// ────────────────────────────────────────────────────────────────
// BoGenerationEntry / ScanoutBoToken / PageFlipRetirement —
// I6b retirement signal infra augmenting ScanoutBoPool's BoState.
// ────────────────────────────────────────────────────────────────

/// Per-BO v2 augmentation parallel to `ScanoutBo::state` (which
/// tracks the Vk/KMS sync state machine). This carries the
/// buffer-age algorithm's `last_present_generation` and the
/// failed-flip `content_invalidated` flag.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct BoGenerationEntry {
    /// Last successful page-flip's generation on this BO.
    /// `None` means freshly-allocated (never presented) OR
    /// invalidated (see `content_invalidated`).
    pub(crate) last_present_generation: Option<u64>,
    /// `true` after a failed atomic commit where this BO's
    /// contents became indeterminate. Cleared on next
    /// successful present.
    pub(crate) content_invalidated: bool,
}

/// Handle returned by `acquire_scanout_bo`. Carries the
/// information the SceneCompositor needs to drive the
/// buffer-age algorithm without poking at `ScanoutBoPool`
/// internals.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ScanoutBoToken {
    pub(crate) output_idx: usize,
    pub(crate) bo_idx: usize,
    pub(crate) extent: vk::Extent2D,
    pub(crate) last_present_generation: Option<u64>,
    pub(crate) content_invalidated: bool,
}

/// Returned by `on_page_flip_complete`. Identifies the BO that
/// just retired (releasable for reuse on next acquire) and the
/// BO that just went on-screen (caller advances its
/// `last_present_generation`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct PageFlipRetirement {
    pub(crate) retired_bo_idx: Option<usize>,
    pub(crate) presented_bo_idx: usize,
    pub(crate) generation: u64,
}

// ────────────────────────────────────────────────────────────────
// FlushOutcome
// ────────────────────────────────────────────────────────────────

/// Phase A: result of a `flush_submit_group` call. Same shape on
/// both Ok and Err paths; the `aborted` flag distinguishes them.
/// Task 3.5 hooks the deferred-queue drain that consumes this.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FlushOutcome {
    pub(crate) flushed_entries: usize,
    pub(crate) reason: FlushReason,
    pub(crate) aborted: bool,
}

// ────────────────────────────────────────────────────────────────
// PlatformBackend
// ────────────────────────────────────────────────────────────────

/// Stage 5 Task 6.1: epoll event-data token for the backend's
/// wakeup_eventfd. Per-batch sync_file FDs use their raw fd as the
/// token instead, distinguishing them from the wakeup_eventfd.
pub(crate) const WAKEUP_EVENTFD_TOKEN: u64 = u64::MAX;

/// One source-renderer completion retained by the platform until its
/// `sync_file` becomes readable.  The stable [`OutputKey`] and monotonic job
/// id remain authoritative across output-vector rebuilds; raw fds and vector
/// indices are deliberately not identities.
struct PendingScanoutRenderCompletion {
    job_id: u64,
    output_key: OutputKey,
    bo_idx: usize,
    /// `None` is Vulkan's valid already-signalled SYNC_FD payload (`fd=-1`).
    /// It bypasses readiness polling but remains a real synchronization
    /// payload that the sink imports as raw -1.
    fd: Option<OwnedFd>,
}

/// A completed source-render job ready for the sink-side copied-scanout
/// submission.
pub(crate) struct ReadyScanoutRenderCompletion {
    pub(crate) job_id: u64,
    pub(crate) output_key: OutputKey,
    pub(crate) bo_idx: usize,
    pub(crate) fd: Option<OwnedFd>,
}

/// The device-local eligibility consequence of one cursor operation and its
/// optional best-effort rollback. A permanent failure always wins over a
/// transient `EINVAL`, even when it was the rollback that exposed the
/// unsupported ioctl. This prevents an `EINVAL` operation followed by an
/// `ENODEV` hide failure from being misclassified as a retryable state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CursorFailureDisposition {
    Unchanged,
    Transient,
    Permanent,
}

/// Returned by `drain_page_flip_events` per `DRM_CRTC_SEQUENCE` event.
/// Fields are raw kernel values; validation (time_ns sign, crtc_id
/// resolution) and `user_data` tag decoding happen in
/// `KmsBackend::on_crtc_sequence_event`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SequenceCompletion {
    /// DRM primary-node identity that produced this event. CRTC handles are
    /// only unique within one DRM device.
    pub(crate) device_key: crate::platform::drm::DrmDeviceKey,
    /// Echoed verbatim from the arm call: low 32 bits are the crtc_id,
    /// the high bit optionally tags an absolute per-target arm
    /// (`ABSOLUTE_SEQ_TAG` in `backend.rs`).
    pub(crate) user_data: u64,
    pub(crate) time_ns: i64,
    pub(crate) sequence: u64,
}

pub(crate) type DrainedPageFlipEvents = (Vec<(usize, PresentClockSample)>, Vec<SequenceCompletion>);

/// Process-local identity of one KMS CRTC.
///
/// DRM object handles are scoped to a DRM device. Two cards may expose the
/// same raw CRTC handle, so every long-lived clock, vblank arm, and event
/// route must carry the owning device as well.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct CrtcKey {
    pub(crate) device_key: crate::platform::drm::DrmDeviceKey,
    pub(crate) crtc: ::drm::control::crtc::Handle,
}

#[derive(Debug, Clone, Copy)]
struct TransientCursorFallback {
    /// Number of successful software-composed retirements this CRTC must
    /// observe before another hardware probe is allowed.
    remaining_sw_retires: u8,
    /// Consecutive EINVAL probes. Retained while a retry is eligible so a
    /// driver that repeatedly rejects the same temporary state is backed off
    /// exponentially rather than probed every frame.
    failures: u8,
}

/// Per-DRM-device hardware cursor state. Raw CRTC handles and legacy cursor
/// ioctls are device-local, so none of these fields may live at platform scope.
pub(crate) struct KmsCursorState {
    pub(crate) plane: Option<crate::kms::cursor_plane::CursorPlane>,
    pending_move: Option<(i32, i32, u16, u16)>,
    permanently_disabled: bool,
    /// CursorPlane::new was attempted for an active startup topology and
    /// failed transiently. Only explicit active-topology/resume boundaries
    /// may retry it.
    initialization_retryable: bool,
    /// This opened device had no active startup CRTC and has never attempted
    /// cursor-plane construction. Only a successful explicit RANDR enable may
    /// consume this state; connected-Off probes and ordinary frames cannot.
    headless_deferred: bool,
    topology_blocked: bool,
    transient_fallback_crtcs: HashMap<::drm::control::crtc::Handle, TransientCursorFallback>,
    nvidia_policy_disabled: bool,
    sprite_signature: Option<(u16, u16, u16, u16)>,
}

/// Aggregate result of one pointer move fanout. A fallback change means a
/// device/output changed HW eligibility and the scene must repaint its SW
/// outputs even if the cursor aggregate mode still contains live HW planes.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CursorMoveOutcome {
    pub(crate) ebusy_count: u32,
    pub(crate) fallback_changed: bool,
    /// A move failed and the hide rollback also failed, so the old HW cursor
    /// remains authoritative and needs another retirement boundary before its
    /// latest position/full ownership can be reconciled.
    pub(crate) retry_required: bool,
}

/// One Vulkan renderer endpoint. Its physical-device handle belongs to the
/// platform's `VkContext` instance; KMS devices are represented separately.
pub(crate) struct RenderDevice {
    pub(crate) id: RenderDeviceId,
    pub(crate) physical_device: vk::PhysicalDevice,
    /// Stable cross-instance identity used to create an exact disposable or
    /// sink-side transfer logical device.  The opaque physical-device handle
    /// above is valid only inside the live renderer's Vulkan instance.
    pub(crate) selector: VulkanDeviceSelector,
    /// Renderer-side primary identity advertised by Vulkan. This is metadata
    /// for same-device detection only and never implies KMS capability.
    pub(crate) advertised_primary_node: Option<crate::platform::drm::DrmDeviceKey>,
    /// Renderer-side render identity advertised by Vulkan.
    pub(crate) advertised_render_node: Option<crate::platform::drm::DrmDeviceKey>,
    /// The selected operational render node. Only the active renderer owns
    /// this resource for now; other inventory entries are identity records.
    pub(crate) render_node: Option<crate::kms::render_node::OpenedRenderNode>,
    /// DRM wrapper over the same selected render node, used for syncobj ioctls.
    pub(crate) render_node_device: Option<Arc<crate::drm::Device>>,
    pub(crate) syncobj_timeline: bool,
}

const SCANOUT_POOL_DEPTH: usize = 3;
/// Fresh completion timeout for each submitted disposable-probe fence.
/// Allocation, atomic TEST_ONLY, pipeline setup, and completed CPU content
/// validation are not charged to this GPU-liveness bound.
pub(crate) const PRIME_RENDER_PROBE_TIMEOUT_NS: u64 = 200_000_000;

pub(crate) struct PreparedScanoutPool {
    pool: ScanoutBoPool,
    /// The exact framebuffer synchronously installed by the candidate loop.
    /// Its BO is already marked `OnScreen` before ownership leaves the helper.
    committed_framebuffer: Option<::drm::control::framebuffer::Handle>,
}

pub(crate) struct PreparedCopiedScanoutPool {
    pool: CopiedScanoutPool,
    committed_framebuffer: Option<::drm::control::framebuffer::Handle>,
}

/// Live exact-plan replay prepared while the current display topology remains
/// active. The fields stay private so only the platform can commit or destroy
/// the uninstalled scanout pool.
pub(crate) struct PreparedQualifiedConnector {
    output_key: OutputKey,
    output: crate::platform::drm::Output,
    mode_spec: yserver_core::backend::ModeSpec,
    x: i32,
    y: i32,
    scanout_route: ScanoutRoute,
    pool: OutputScanout,
}

struct ResolvedConnectorEnable {
    connector: String,
    device: Rc<drm::Device>,
    output: crate::platform::drm::Output,
    scanout_route: ScanoutRoute,
    existing_idx: Option<usize>,
    needs_pool_realloc: bool,
}

/// Resource-free result of disposable cross-device qualification. The parent
/// may replay only this exact representation on its live Vulkan contexts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum QualifiedScanoutPlan {
    Shared(ScanoutAllocationPlan),
    Copied {
        sink_id: RenderDeviceId,
        plan: CopiedScanoutPlan,
    },
}

/// Scalar identity of the copied-path sink selected for one worker probe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CopiedQualificationSink {
    pub(crate) id: RenderDeviceId,
    pub(crate) selector: VulkanDeviceSelector,
}

/// Structured worker-visible qualification outcome. Ordinary incompatibility
/// may be reported to the client; indeterminate submitted work and device loss
/// must stop the candidate sequence immediately.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ScanoutQualificationError {
    #[error("scanout route rejected: {0}")]
    Rejected(io::Error),
    #[error("scanout route probe became indeterminate: {0}")]
    Indeterminate(io::Error),
    #[error("scanout route probe lost a Vulkan device: {0}")]
    DeviceLost(io::Error),
}

pub(crate) enum ExactPlanReplay<T> {
    Prepared(T),
    Rejected(io::Error),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum CopyFreeScanoutError {
    #[error("{0}")]
    Candidates(io::Error),
    #[error("terminal disposable copy-free probe failure: {0}")]
    TerminalDisposableProbe(io::Error),
    #[error("live renderer lost during copy-free scanout setup: {0}")]
    LiveRendererLost(io::Error),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum CopiedScanoutError {
    #[error("{0}")]
    Candidates(io::Error),
    #[error("terminal disposable copied probe failure: {0}")]
    TerminalDisposableProbe(io::Error),
    #[error("live Vulkan device lost during copied scanout setup ({context}): {source}")]
    LiveDeviceLost {
        context: String,
        #[source]
        source: io::Error,
    },
}

#[derive(Debug, thiserror::Error)]
#[error("terminal disposable scanout probe failure: {source}")]
struct TerminalDisposableProbeMarker {
    #[source]
    source: io::Error,
}

/// One opened display/KMS device. Renderer identity and render-node resources
/// deliberately live in `RenderDevice` instead.
pub(crate) struct KmsDevice {
    pub(crate) key: crate::platform::drm::DrmDeviceKey,
    pub(crate) device: Rc<drm::Device>,
    pub(crate) cursor: KmsCursorState,
}

trait RollbackScanoutOutput {
    fn output_key(&self) -> &OutputKey;
    fn drm_output(&self) -> &crate::platform::drm::Output;
    fn disarm_swapchain(&mut self);
}

/// RAII coverage for every fallible step between the initial modeset and a
/// fully-built [`PlatformBackend`]. The output buffers stay borrowed until
/// this guard is either dropped (rollback) or explicitly disarmed when
/// responsibility transfers into `PlatformBackend::drop`.
struct InitialScanoutRollbackGuard<'a, O, F>
where
    O: RollbackScanoutOutput,
    F: FnMut(&drm::Device, &crate::platform::drm::Output) -> io::Result<()>,
{
    devices: &'a [KmsDevice],
    outputs: &'a mut [O],
    disable: F,
    armed: bool,
}

/// v2's real DRM/Vk/libinput owner. Replaces the flat field set
/// that Stage 1b's `KmsBackend` carried.
pub(crate) struct PlatformBackend {
    /// Armed only while the outer `KmsBackend` constructor is still
    /// fallible. `Drop` disables the initial modesets before the dumb
    /// swapchains are destroyed; full construction explicitly disarms it.
    initial_scanout_rollback_armed: bool,
    // DRM / output side
    pub(crate) devices: Vec<KmsDevice>,
    /// Immutable same-instance Vulkan inventory of graphics+transfer
    /// queue-capable render-identified devices. Handles are valid for exactly
    /// the lifetime of `vk` below.
    pub(crate) render_devices: Vec<RenderDevice>,
    pub(crate) selected_render_device: Option<RenderDeviceId>,
    pub(crate) outputs: Vec<ActiveOutput>,
    pub(crate) fb_w: u16,
    pub(crate) fb_h: u16,
    /// Non-identity current CRTC transforms (RANDR `SetCrtcTransform`),
    /// client-owned like the logical screen size, so keyed by output and
    /// kept across the `outputs` rebuilds of a topology change.
    pub(crate) output_transforms: HashMap<OutputKey, yserver_core::randr::CrtcTransform>,
    /// Latest general kernel `(msc, ust_micros)` per device-qualified CRTC, updated
    /// by pageflip retirements and standalone sequence events. Drives
    /// `PresentNotifyMSC` (`present_get_ust_msc`): a compositor's
    /// `PresentNotifyMSC` completes with these real values so its frame
    /// clock advances at the display refresh rate. Empty until the first
    /// flip retires.
    pub(crate) ust_msc: std::collections::HashMap<CrtcKey, (u64, u64)>,
    /// Latest per-output sample eligible to release a paced Present
    /// completion. Pageflip retirements always qualify; standalone sequence
    /// events are inserted by `KmsBackend` only when the display is idle.
    pub(crate) completion_clocks: std::collections::HashMap<CrtcKey, PresentClockSample>,

    /// Per-output software MSC fallback. Some KMS drivers (notably
    /// apple_drm on Asahi) report `frame == 0` in every page-flip
    /// completion event — the kernel does not maintain a CRTC
    /// sequence counter — AND reject `DRM_IOCTL_CRTC_QUEUE_SEQUENCE`
    /// with `EOPNOTSUPP`, so the idle-vblank arming path can't
    /// advance the clock either. Without a non-zero MSC, every
    /// `msc > 0` gate in the Present NotifyMSC path deadlocks a
    /// compositor's vblank scheduler (picom presents frame 0 then
    /// blocks forever).
    ///
    /// This counter increments on every pageflip retirement where
    /// the kernel reports `frame == 0`, giving Present a monotonically
    /// advancing MSC at the actual pageflip cadence. On drivers that
    /// report a real `frame > 0` this map stays empty (the real value
    /// is used directly).
    pub(crate) software_msc: std::collections::HashMap<CrtcKey, u64>,

    // Input side
    input_ctx: Option<crate::input::SendContext>,
    #[cfg(target_os = "linux")]
    pub(crate) hotplug_monitor: Option<crate::kms::hotplug::DrmHotplugMonitor>,

    /// Stage 5 Task 6.1: inner poll FD aggregating per-batch
    /// sync_file FDs for deferred PRESENT completion. Exposed via
    /// `poll_fds()` under `BackendFdKind::PresentCompletion`. Spec
    /// `2026-05-23-deferred-present-completion-design.md`.
    pub(crate) present_completion_epfd: crate::kms::render::completion_poller::CompletionPoller,

    /// Stage 5 Task 6.1: eventfd used to wake the main loop when a
    /// PRESENT completion is enqueued. Registered with
    /// `present_completion_epfd` at init under `WAKEUP_EVENTFD_TOKEN`.
    pub(crate) wakeup_eventfd: nix::sys::eventfd::EventFd,

    /// Stable native readiness aggregator for renderer-completion sync files
    /// used by copied reverse-PRIME.  This is intentionally distinct from the
    /// Present completion poller: the two readiness streams have different
    /// ownership, cancellation, and delivery semantics.
    scanout_render_completion_epfd: crate::kms::render::completion_poller::CompletionPoller,
    pending_scanout_render_completions: std::collections::VecDeque<PendingScanoutRenderCompletion>,
    next_scanout_render_job_id: u64,

    // Vulkan side. `Option` only to support test fixtures that
    // skip Vk init (`for_tests`). Production `open_with_commit`
    // always returns `Some`. v2 has no pixman fallback.
    pub(crate) vk: Option<Arc<VkContext>>,
    /// Command buffer + fence reused by scanout reads; allocated from
    /// `ops_command_pool`, so declared before it to drop first.
    pub(crate) scanout_readback_op: Option<crate::kms::vk::ops::ReusableOneShot>,
    /// Wrapped in `Option` for the same reason. Drop order
    /// matters: ops_command_pool BEFORE fence_pool BEFORE vk
    /// (handled by struct field order — Rust drops fields in
    /// declaration order).
    pub(crate) ops_command_pool: Option<OpsCommandPool>,
    pub(crate) fence_pool: Option<FencePool>,
    /// Reused `HOST_CACHED`-preferred destination for synchronous scanout
    /// reads (root GetImage / ShmGetImage). Idle between reads, which wait
    /// on their own fence; grown on demand. Holds its own `Arc<VkContext>`.
    pub(crate) scanout_readback: Option<crate::kms::render::engine::StagingBuffer>,

    /// Stage 3f.10: recycled `(image, view, memory)` triples for
    /// CreatePixmap. Reuses v1's `PixmapPool` verbatim — its
    /// `try_take` / `try_return` API + bucket-cap + size-cap
    /// logic is backend-agnostic. Bypassed by the test fixture
    /// (`for_tests`) and on `for_tests_with_vk` (the harness
    /// constructs `RenderEngine` directly without going through
    /// `open_with_commit`).
    pub(crate) pixmap_pool: Option<Arc<crate::kms::vk::pixmap_pool::PixmapPool>>,

    /// Minimal sink-side Vulkan transfer contexts, keyed by the exact renderer
    /// endpoint whose advertised primary identity matches a KMS device.
    /// Copied outputs on the same sink share one queue/context; each pool owns
    /// an `Arc` so imported aliases remain valid until pool teardown.
    copy_vk_contexts: HashMap<RenderDeviceId, Arc<VkContext>>,

    /// Per-output scanout BO pool. `None` if a particular
    /// output's allocation failed (rare; e.g. RADV/gfx8 quirks).
    /// Stage 2c+ paint paths skip output indices with `None`
    /// pool, mirroring v1's behaviour.
    pub(crate) scanout_pools: Vec<Option<OutputScanout>>,

    /// Per-output, per-BO generation entries. `bo_generations[oi][bi]`
    /// pairs with `scanout_pools[oi].as_ref().unwrap().bos[bi]`.
    /// `Vec::new()` for outputs whose pool is `None`.
    pub(crate) bo_generations: Vec<Vec<BoGenerationEntry>>,
    /// Monotonic per-platform counter. Each successful present
    /// gets a fresh generation; SceneCompositor's `frame_gen`
    /// derives from `current_generation + 1` per spec.
    pub(crate) next_present_generation: u64,

    /// Per-output flag — was the first pageflip-complete event
    /// logged for this output? Mirrors v1's `first_pageflip_logged`.
    pub(crate) first_pageflip_logged: Vec<bool>,

    /// Latched on any submit-time / pool-time Vk error. Once
    /// true, the renderer is in a stuck state and the next
    /// composite tick should bail.
    pub(crate) renderer_failed: bool,
    pub(crate) shutting_down: bool,

    /// Phase A: multi-CB accumulator. Populated by Task 3 callers;
    /// flushed via `flush_submit_group`.
    submit_group: SubmitGroup,

    /// Phase A: last `FlushOutcome` produced by `flush_submit_group`.
    /// Consumed exactly once by `take_last_flush_outcome`.
    last_flush_outcome: Option<FlushOutcome>,

    /// Test-only: when true, the next `flush_submit_group` call will
    /// route through `abort_flush` instead of the real
    /// `vkQueueSubmit2`. Reset to false after consumption.
    /// Always compiled (not cfg(test)) so integration-test pub wrappers
    /// on `KmsBackend` can reach it from the external test crate.
    force_next_submit_failure: bool,
    /// #214 telemetry: the cause the next group flush is counted under
    /// (set by a frame close); `None` maps the `FlushReason`.
    next_submit_cause: Option<crate::kms::vk::submit_stats::SubmitCause>,
    /// Fault injection like `force_next_submit_failure`: the next frame
    /// close fails while recording, before anything is submitted.
    force_next_frame_record_failure: bool,
    /// Real `vkQueueSubmit2` calls made through this platform's paint
    /// group and Present signal paths (per instance, so parallel tests
    /// can assert exact counts).
    queue_submits: u64,
}

/// One live scanout route a connector snapshot removed, with the layout
/// rectangle and mode identity it occupied at the moment it was removed.
///
/// `apply_connector_snapshot` deletes the `ActiveOutput` row, so this is the
/// only surviving record of where the route was. The backend owns layout
/// policy (packing, the virtual-screen extent, reserved slots), and it needs
/// the rectangle after the snapshot has already destroyed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DroppedRoute {
    pub key: OutputKey,
    pub x: i32,
    pub y: i32,
    pub width: u16,
    pub height: u16,
    pub vrefresh: u32,
}

/// A placed rectangle: `(x, y, width, height)`.
pub(crate) type LayoutRect = (i32, i32, u16, u16);

/// Outcome of a connector rescan.
#[derive(Debug, Default)]
pub(crate) struct RescanResult {
    pub added_keys: Vec<OutputKey>,
    pub dropped_keys: Vec<OutputKey>,
    pub dropped_old_indices: Vec<usize>,
    /// Layout rectangle + mode identity of every route `dropped_old_indices`
    /// removed, captured before the `ActiveOutput` row was deleted.
    pub dropped_layouts: Vec<DroppedRoute>,
    pub added_count: usize,
    /// Every connector currently discovered as connected, including inactive
    /// secondary-card connectors. The backend reconciles this complete,
    /// device-qualified snapshot into its stable RANDR registry.
    pub connected: Vec<ConnectorSnapshot>,
}

/// Device-qualified connector metadata gathered at a forced heavy
/// startup/hotplug/resume boundary. It refreshes the stable RANDR registry
/// without inventing a CRTC/plane assignment.
#[derive(Debug, Clone)]
pub(crate) struct ConnectorSnapshot {
    pub(crate) key: OutputKey,
    pub(crate) modes: Vec<crate::platform::drm::Mode>,
    pub(crate) mm_width: u32,
    pub(crate) mm_height: u32,
    pub(crate) edid: Vec<u8>,
    pub(crate) connector_type: String,
}
