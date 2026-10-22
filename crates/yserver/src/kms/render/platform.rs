//! `PlatformBackend`: the hardware and OS surface of the v2 renderer.
//!
//! It owns the DRM devices and KMS outputs, libinput and the per-output
//! scanout BO pools, plus `gpu`: the [`GpuCore`] holding the Vulkan device,
//! the ops command pool, the recyclable fence pool and the submit group.
//! Sync is two objects: [`FenceTicket`](crate::kms::render::gpu::FenceTicket)
//! for CPU-side resource lifetime (I6a) and each `ScanoutBo`'s long-lived export semaphore for the KMS
//! `IN_FENCE_FD` wait. This file holds the imports, every type and const
//! (private fields stay visible to all children); the code lives in:
//!
//! - `init`: open, `from_platform_init`, inventory, rollback guard, `Drop`.
//! - `devices`: device, route and output-geometry accessors.
//! - `qualify`: scanout-route qualification (startup worker, probe).
//! - `cursor`: cursor-plane failure policy and `KmsCursorState`.
//! - `flip_events`: DRM events, page flips, render completions.
//! - `scanout_bos`: scanout BO lifecycle and copied submits.
//! - `scene_sink`: the scene's `OutputSink` and `CursorPlaneSink`.
//! - `connectors`: probe, enable/disable/remove, snapshot, layout.
//! - `power`: bounded idle wait, `disable_output`, DPMS.
//! - `tests`: unit tests.

#![allow(
    dead_code,
    reason = "FenceTicket / scanout BO primitives are consumed by Stages 2b–2e"
)]

mod connectors;
mod cursor;
mod cursor_atomic;
pub(crate) mod cursor_commit;
pub(crate) mod cursor_state;
mod devices;
pub(crate) mod flip_events;
mod init;
pub(in crate::kms::render) mod power;
mod qualify;
mod scanout_bos;
mod scene_sink;

#[cfg(test)]
use connectors::*;
#[cfg(test)]
use cursor::*;
#[cfg(test)]
use init::*;
#[cfg(test)]
pub(in crate::kms::render) use power::OutputPowerExecutor;
pub(crate) use qualify::*;
#[cfg(test)]
use scanout_bos::*;

#[cfg(test)]
mod tests;

use std::{
    collections::{HashMap, HashSet},
    io,
    os::fd::OwnedFd,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use ash::vk;
use yserver_core::backend::PresentClockSample;

use crate::{
    drm::{
        self,
        async_flip::{AsyncFlipLedger, FlipTiming},
    },
    kms::{
        backend::{ActiveOutput, OutputKey},
        scanout_route::ScanoutRoute,
        vk::{
            device::{VkContext, VulkanDeviceSelector},
            scanout::{
                CopiedScanoutPlan, CopiedScanoutPool, OutputScanout, ScanoutAllocationPlan,
                ScanoutBoPool,
            },
        },
    },
};

pub(crate) use crate::kms::{
    render::gpu::{FencePool, GpuCore},
    scanout_route::RenderDeviceId,
};

// ────────────────────────────────────────────────────────────────
// BoGenerationEntry —
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

// ────────────────────────────────────────────────────────────────
// PlatformBackend
// ────────────────────────────────────────────────────────────────

/// Stage 5 Task 6.1: epoll event-data token for the backend's
/// wakeup_eventfd. Per-batch sync_file FDs use their raw fd as the
/// token instead, distinguishing them from the wakeup_eventfd.
const WAKEUP_EVENTFD_TOKEN: u64 = u64::MAX;

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

#[derive(Debug, Clone, Copy)]
pub(crate) struct FlipEventTimestamps {
    /// Nonzero DRM atomic `user_data` normalized to an optional identity.
    #[allow(dead_code)] // identity filtering is connected in Task 2
    pub(crate) commit_id: Option<u64>,
    /// CLOCK_MONOTONIC receipt time used by async ioctl-to-event turnaround.
    pub(crate) received_at_monotonic: Option<Duration>,
    /// Kernel completion time converted to CLOCK_MONOTONIC for Present latency.
    pub(crate) completion_at_monotonic: Option<Duration>,
}

type DrainedPageFlipEvents = (
    Vec<(usize, PresentClockSample, FlipEventTimestamps)>,
    Vec<SequenceCompletion>,
);

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

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct CursorHideRecovery {
    attempts: u8,
}

/// Per-DRM-device hardware cursor state. Raw CRTC handles and legacy cursor
/// ioctls are device-local, so none of these fields may live at platform scope.
pub(crate) struct KmsCursorState {
    pub(crate) plane: Option<crate::kms::cursor_plane::CursorPlane>,
    pub(crate) crtc_states: HashMap<::drm::control::crtc::Handle, cursor_state::CursorCrtcState>,
    pending_move: Option<(i32, i32, u16, u16)>,
    /// Pointer-event timestamp paired with `pending_move` across an EBUSY
    /// retry. Atomic positions carry this timestamp directly.
    pending_move_timestamp: Option<Instant>,
    /// Latest discovered atomic position for each cursor-visible CRTC. The
    /// position remains available for every scene commit; the unsent copy is
    /// consumed only after an accepted commit carries it.
    atomic_positions:
        HashMap<::drm::control::crtc::Handle, crate::drm::atomic::CursorPlanePosition>,
    atomic_pending_moves:
        HashMap<::drm::control::crtc::Handle, crate::drm::atomic::CursorPlanePosition>,
    /// Accepted cursor-only commits, keyed by CRTC. A cursor-only commit is
    /// in flight only after its atomic ioctl succeeded.
    atomic_cursor_commits_in_flight:
        HashMap<::drm::control::crtc::Handle, cursor_state::InFlightCursorCommit>,
    /// A cursor-only EBUSY proves another commit is pending on this CRTC.
    /// Keep the latest position unsent until that CRTC's next flip event.
    cursor_retry_waits_for_flip: HashSet<::drm::control::crtc::Handle>,
    /// A rejected cursor request whose legacy hide failed. Pointer motion
    /// retries this hide, with a device-wide terminal failure after 3 tries.
    cursor_hide_recovery: HashMap<::drm::control::crtc::Handle, CursorHideRecovery>,
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
    /// Capability-selected atomic position path for this device's active
    /// CRTC topology.
    pub(crate) atomic_cursor: bool,
    sprite_signature: Option<(u16, u16, u16, u16)>,
}

impl KmsCursorState {
    pub(crate) fn state_path(&self) -> crate::kms::cursor_plane::topology::CursorStatePath {
        self.plane
            .as_ref()
            .map(crate::kms::cursor_plane::CursorPlane::state_path)
            .map(|decision| decision.path)
            .unwrap_or(crate::kms::cursor_plane::topology::CursorStatePath::Legacy)
    }
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
pub(super) const PRIME_RENDER_PROBE_TIMEOUT_NS: u64 = 200_000_000;

struct PreparedScanoutPool {
    pool: ScanoutBoPool,
    /// The exact framebuffer synchronously installed by the candidate loop.
    /// Its BO is already marked `OnScreen` before ownership leaves the helper.
    committed_framebuffer: Option<::drm::control::framebuffer::Handle>,
}

struct PreparedCopiedScanoutPool {
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

enum ExactPlanReplay<T> {
    Prepared(T),
    Rejected(io::Error),
}

#[derive(Debug, thiserror::Error)]
enum CopyFreeScanoutError {
    #[error("{0}")]
    Candidates(io::Error),
    #[error("terminal disposable copy-free probe failure: {0}")]
    TerminalDisposableProbe(io::Error),
    #[error("live renderer lost during copy-free scanout setup: {0}")]
    LiveRendererLost(io::Error),
}

#[derive(Debug, thiserror::Error)]
enum CopiedScanoutError {
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
    /// `DRM_CAP_ATOMIC_ASYNC_PAGE_FLIP`, sampled once when this KMS device opens.
    pub(crate) async_page_flip: bool,
    pub(crate) atomic_commit_sink: Rc<crate::drm::atomic::AtomicCommitSink>,
    pub(crate) page_flip_event_source: Rc<dyn flip_events::PageFlipEventSource>,
    #[cfg(test)]
    pub(crate) test_page_flip_event_source: Option<Rc<flip_events::RecordingPageFlipEventSource>>,
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
pub struct PlatformBackend {
    /// Armed only while the outer `KmsBackend` constructor is still
    /// fallible. `Drop` disables the initial modesets before the dumb
    /// swapchains are destroyed; full construction explicitly disarms it.
    initial_scanout_rollback_armed: bool,
    // DRM / output side
    pub(crate) devices: Vec<KmsDevice>,
    /// Shared tearing state used by per-device commit telemetry and the
    /// composed scene submit path.
    pub(crate) tearing_on: bool,
    /// Earliest backend-received Copy damage waiting for an async composed
    /// commit. Retained across coalesced scene work.
    pub(crate) tearing_copy_received_at: Option<Instant>,
    pub(crate) tearing_copy_received_monotonic: Option<Duration>,
    pub(super) output_power_executor: Rc<dyn power::OutputPowerExecutor>,
    /// Cursor rollback outcomes raised while the scene owns the platform
    /// borrow. The backend drains this immediately after the submit pass to
    /// wake the scene and request an unflip when needed.
    deferred_cursor_move_outcome: CursorMoveOutcome,
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

    /// Per-stream history for async page-flip EINVAL fallback. Cleared when
    /// the backend leaves its active KMS session.
    pub(crate) async_ledger: AsyncFlipLedger,
    /// Most recent outcome per independently submitted stream. The scene
    /// output contract does not carry flip timing, so retain the result here
    /// for the Present policy and diagnostics that follow a synchronous submit.
    pub(crate) async_outcomes:
        HashMap<crate::drm::async_flip::AsyncStream, crate::drm::async_flip::AsyncOutcome>,
    /// The submit timing selected by the caller. Production initializes this
    /// to Vsync; the later Present policy may request Async for eligible work.
    pub(crate) requested_flip_timing: FlipTiming,
    /// Scene timing is selected per output for one `maybe_composite` pass.
    /// `None` preserves the legacy test/caller override above; production sets
    /// every output on every pass, starting with Vsync.
    pub(crate) scene_flip_timings: Vec<Option<FlipTiming>>,
    /// A scene retirement attempted to hide a hardware cursor and failed.
    /// The backend consumes this to keep tearing's fallback diagnostic once.
    pub(crate) scene_cursor_hide_failed: bool,
    /// While set, a shared scene frame is rendered and its fence exported but
    /// not committed: `(output_idx, bo_idx, complete)` is recorded in
    /// `deferred_scene_flips` for one grouped commit by the caller.
    /// `complete` is false when the compose recorded only a prefix of the
    /// scene (descriptor pool exhausted); such a frame must not be flipped.
    pub(crate) defer_scene_flips: bool,
    pub(crate) deferred_scene_flips: Vec<(usize, usize, bool)>,

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
    /// Readiness observer for copied-scanout render completions. The live
    /// implementation only polls the original completion fd.
    pub(crate) render_completion_readiness: Rc<dyn flip_events::RenderCompletionReadiness>,
    pending_scanout_render_completions: std::collections::VecDeque<PendingScanoutRenderCompletion>,
    next_scanout_render_job_id: u64,

    /// Command buffer + fence reused by scanout reads, allocated from
    /// `gpu.ops_command_pool`. `Drop` resets it first; it is also declared
    /// before `gpu`.
    pub(crate) scanout_readback_op: Option<crate::kms::vk::ops::ReusableOneShot>,
    /// The GPU-only state; lifetime rules on [`GpuCore`].
    pub(crate) gpu: GpuCore,
    /// Reused `HOST_CACHED`-preferred destination for synchronous scanout
    /// reads (root GetImage / ShmGetImage). Idle between reads, which wait
    /// on their own fence; grown on demand. Holds its own `Arc<VkContext>`.
    pub(crate) scanout_readback: Option<crate::kms::render::engine::StagingBuffer>,

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

    pub(crate) shutting_down: bool,
}

impl PlatformBackend {
    #[cfg(test)]
    pub(crate) fn for_tests_gpu_contexts(&self) -> Vec<Arc<VkContext>> {
        let mut contexts = Vec::new();
        if let Some(vk) = self.gpu.vk.as_ref() {
            contexts.push(Arc::clone(vk));
        }
        for vk in self.copy_vk_contexts.values() {
            if !contexts.iter().any(|context| Arc::ptr_eq(context, vk)) {
                contexts.push(Arc::clone(vk));
            }
        }
        contexts
    }

    pub(crate) fn set_tearing_on(&mut self, on: bool) {
        self.tearing_on = on;
        for device in &self.devices {
            device.atomic_commit_sink.set_tearing_on(on);
        }
        if !on {
            self.tearing_copy_received_at = None;
            self.tearing_copy_received_monotonic = None;
        }
    }

    pub(crate) fn note_tearing_copy_damage(
        &mut self,
        received_at: Instant,
        received_monotonic: Option<Duration>,
    ) {
        if self.tearing_on && self.tearing_copy_received_at.is_none() {
            self.tearing_copy_received_at = Some(received_at);
            self.tearing_copy_received_monotonic = received_monotonic;
        }
    }

    pub(crate) fn emit_tearing_summary_if_due(&self, on: bool, avg_gpu_render_ns: u64) {
        for device in &self.devices {
            device.atomic_commit_sink.emit_tearing_summary_if_due(
                device.key,
                on,
                avg_gpu_render_ns,
            );
            device
                .atomic_commit_sink
                .emit_present_latency_summary_if_due(device.key);
        }
    }

    pub(crate) fn note_tearing_async_flip_event(
        &self,
        output_idx: usize,
        timestamps: FlipEventTimestamps,
    ) -> Option<u64> {
        let output = self.outputs.get(output_idx)?;
        if let Some(device) = self.device_for_output(&output.key) {
            return device.atomic_commit_sink.note_async_flip_event(
                &device.device,
                output.output.crtc,
                timestamps,
            );
        }
        None
    }

    pub(crate) fn note_direct_present_received(&self, output_idx: usize) {
        let Some(output) = self.outputs.get(output_idx) else {
            return;
        };
        if let Some(device) = self.device_for_output(&output.key) {
            device.atomic_commit_sink.note_direct_present_received();
        }
    }
}

/// A placed rectangle: `(x, y, width, height)`.
pub(super) type LayoutRect = (i32, i32, u16, u16);

/// Outcome of a connector rescan.
#[derive(Debug, Default)]
pub(crate) struct RescanResult {
    pub added_keys: Vec<OutputKey>,
    pub dropped_keys: Vec<OutputKey>,
    pub dropped_old_indices: Vec<usize>,
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
