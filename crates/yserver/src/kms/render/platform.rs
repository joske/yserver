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

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    io,
    os::fd::{AsFd, AsRawFd, OwnedFd, RawFd},
    path::PathBuf,
    rc::{Rc, Weak},
    sync::Arc,
};

use ash::vk;
use yserver_core::backend::{BackendFdKind, PresentClockSample, PresentClockSource};

use crate::{
    drm,
    kms::{
        backend::{ActiveOutput, OutputKey, PlatformInit, platform_init as core_platform_init},
        render::{
            store::Storage,
            submit_group::{FlushReason, SubmitGroup},
        },
        vk::{
            device::VkContext,
            ops::OpsCommandPool,
            scanout::{BoPhase, BoState, ScanoutBoPool},
        },
    },
};

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
    /// Temporary SYNC_FD semaphore payloads waited by this submission.
    /// Vulkan requires each semaphore handle to remain alive until the
    /// queue operation retires, so these share the submission fence's
    /// lifetime rather than being destroyed immediately after submit.
    imported_wait_semaphores: RefCell<Vec<vk::Semaphore>>,
}

impl std::fmt::Debug for FenceTicketInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `VkContext` owns raw Vulkan handles and doesn't impl
        // `Debug`; opaque-print the `vk` field rather than dragging
        // a Debug derive through the whole device chain.
        f.debug_struct("FenceTicketInner")
            .field("fence", &self.fence)
            .field("signaled_cache", &self.signaled_cache.get())
            .field("pool", &"<weak>")
            .field("vk", &self.vk.as_ref().map(|_| "<Arc<VkContext>>"))
            .field(
                "imported_wait_semaphores",
                &self
                    .imported_wait_semaphores
                    .try_borrow()
                    .map(|waits| waits.len())
                    .unwrap_or_default(),
            )
            .finish()
    }
}

impl FenceTicket {
    /// Non-blocking signaled check. Caches `true` once observed
    /// so subsequent calls don't hit the driver.
    pub(crate) fn poll_signaled(&self, vk: &VkContext) -> bool {
        if self.inner.signaled_cache.get() {
            return true;
        }
        // ash's `get_fence_status` returns `Result<bool, vk::Result>`
        // where the bool is the signaled state (Ok(true) =
        // VK_SUCCESS, Ok(false) = VK_NOT_READY). Errors are real
        // driver failures.
        match unsafe { vk.device.get_fence_status(self.inner.fence) } {
            Ok(true) => {
                self.inner.signaled_cache.set(true);
                true
            }
            Ok(false) => false,
            Err(e) => {
                log::warn!("FenceTicket::poll_signaled: get_fence_status: {e:?}");
                false
            }
        }
    }

    /// Synchronous wait. **Off the hot path** — used by
    /// `get_image` readback and shutdown teardown.
    pub(crate) fn wait(&self, vk: &VkContext) -> Result<(), vk::Result> {
        if self.inner.signaled_cache.get() {
            return Ok(());
        }
        // 5 second timeout — long enough to cover any realistic
        // GPU work; if we hit it the device is hung anyway.
        match unsafe {
            vk.device
                .wait_for_fences(&[self.inner.fence], true, 5_000_000_000)
        } {
            Ok(()) => {
                self.inner.signaled_cache.set(true);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Raw fence handle for `vkQueueSubmit2`. Caller MUST NOT
    /// destroy or reset this fence — the ticket owns its
    /// lifetime via the pool.
    pub(crate) fn fence(&self) -> vk::Fence {
        self.inner.fence
    }

    /// Keep imported binary wait semaphores alive until this submission's
    /// fence retires. Called only after a successful `vkQueueSubmit2`.
    fn retain_imported_wait_semaphores(&self, semaphores: Vec<vk::Semaphore>) {
        if semaphores.is_empty() {
            return;
        }
        self.inner
            .imported_wait_semaphores
            .borrow_mut()
            .extend(semaphores);
    }

    /// Test-only constructor: returns a ticket whose `poll_signaled`
    /// returns `true` and `wait` returns `Ok(())` without ever touching
    /// a real VkDevice. Built with a null fence, `signaled_cache` pre-set
    /// to `true`, and a dangling pool Weak so Drop becomes a no-op.
    /// Use ONLY in unit tests that need a `FenceTicket` value without
    /// constructing a real fence.
    #[cfg(test)]
    pub(crate) fn for_tests_stub() -> Self {
        Self {
            inner: Rc::new(FenceTicketInner {
                fence: vk::Fence::null(),
                signaled_cache: Cell::new(true),
                pool: Weak::<RefCell<FencePoolInner>>::new(),
                vk: None,
                imported_wait_semaphores: RefCell::new(Vec::new()),
            }),
        }
    }
}

impl Drop for FenceTicketInner {
    fn drop(&mut self) {
        let Some(pool) = self.pool.upgrade() else {
            // Pool already gone — `KmsBackend`'s field-drop order
            // runs `platform` (containing `fence_pool`) before
            // `store` / `engine` / `scene`, all of which hold
            // tickets that only release at this point. The
            // `VkContext` is still alive (we kept a strong `Arc`),
            // so destroy the fence handle directly. Pre-2026-05-31
            // this branch bailed out, leaking the fence — 1471
            // VkFences leaked at SIGTERM on bee/MATE. `None` is
            // the `for_tests_stub` shape (no real device); also
            // no-op for `vk::Fence::null()`.
            if let Some(vk) = self.vk.as_ref()
                && self.fence != vk::Fence::null()
            {
                unsafe {
                    for semaphore in self.imported_wait_semaphores.get_mut().drain(..) {
                        vk.device.destroy_semaphore(semaphore, None);
                    }
                    vk.device.destroy_fence(self.fence, None);
                }
            }
            return;
        };
        let mut pool = pool.borrow_mut();
        let signaled = self.signaled_cache.get()
            || match unsafe { pool.vk.device.get_fence_status(self.fence) } {
                Ok(true) => {
                    self.signaled_cache.set(true);
                    true
                }
                Ok(false) => false,
                Err(e) => {
                    log::warn!("FenceTicketInner::drop: get_fence_status: {e:?}");
                    false
                }
            };
        if signaled {
            unsafe {
                for semaphore in self.imported_wait_semaphores.get_mut().drain(..) {
                    pool.vk.device.destroy_semaphore(semaphore, None);
                }
            }
            pool.recycle(self.fence);
        } else {
            // Unsignaled drop: per the spec, recycling here
            // would race the still-pending GPU work that names
            // this fence (it might be referenced by an
            // in-flight submit). Leak the handle and flag the
            // renderer as failed so the next op surfaces the
            // condition.
            log::error!(
                "FenceTicket: leaked unsignaled fence {:?} on drop \
                 — renderer_failed will be set on next platform access",
                self.fence,
            );
            pool.renderer_failed = true;
            pool.leaked_fences.push(self.fence);
        }
    }
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

impl PresentCompletionSignal {
    #[must_use]
    pub(crate) fn semaphore(&self) -> vk::Semaphore {
        self.semaphore
    }

    pub(crate) fn export_sync_file_fd(&self) -> Result<Option<OwnedFd>, vk::Result> {
        let info = vk::SemaphoreGetFdInfoKHR::default()
            .semaphore(self.semaphore)
            .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
        let raw = unsafe { self.vk.external_semaphore_fd.get_semaphore_fd(&info)? };
        crate::kms::vk::optional_sync_fd_from_vk(raw, "vkGetSemaphoreFdKHR(SYNC_FD)")
    }
}

fn create_present_completion_signal(
    vk: Arc<VkContext>,
) -> Result<PresentCompletionSignal, vk::Result> {
    let mut export_info = vk::ExportSemaphoreCreateInfo::default()
        .handle_types(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
    let create_info = vk::SemaphoreCreateInfo::default().push_next(&mut export_info);
    let semaphore = unsafe { vk.device.create_semaphore(&create_info, None)? };
    Ok(PresentCompletionSignal { vk, semaphore })
}

impl Drop for PresentCompletionSignal {
    fn drop(&mut self) {
        unsafe {
            self.vk.device.destroy_semaphore(self.semaphore, None);
        }
    }
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

impl FencePoolInner {
    fn recycle(&mut self, fence: vk::Fence) {
        // Reset to unsignaled so the next acquire can re-pass
        // the handle straight to vkQueueSubmit2 (which requires
        // unsignaled).
        if let Err(e) = unsafe { self.vk.device.reset_fences(&[fence]) } {
            log::warn!("FencePool::recycle: reset_fences: {e:?} — leaking fence");
            self.leaked_fences.push(fence);
            return;
        }
        self.free.push(fence);
    }
}

impl FencePool {
    pub(crate) fn new(vk: Arc<VkContext>) -> Self {
        Self {
            inner: Rc::new(RefCell::new(FencePoolInner {
                vk,
                free: Vec::with_capacity(8),
                leaked_fences: Vec::new(),
                renderer_failed: false,
            })),
        }
    }

    fn acquire(&self) -> Result<FenceTicket, vk::Result> {
        let mut pool = self.inner.borrow_mut();
        let fence = if let Some(f) = pool.free.pop() {
            f
        } else {
            let info = vk::FenceCreateInfo::default();
            unsafe { pool.vk.device.create_fence(&info, None)? }
        };
        let vk = Arc::clone(&pool.vk);
        drop(pool);
        Ok(FenceTicket {
            inner: Rc::new(FenceTicketInner {
                fence,
                signaled_cache: Cell::new(false),
                pool: Rc::downgrade(&self.inner),
                vk: Some(vk),
                imported_wait_semaphores: RefCell::new(Vec::new()),
            }),
        })
    }

    pub(crate) fn renderer_failed(&self) -> bool {
        self.inner
            .try_borrow()
            .map(|p| p.renderer_failed)
            .unwrap_or(true)
    }
}

impl Drop for FencePool {
    fn drop(&mut self) {
        let pool = self.inner.borrow();
        // Best-effort wait so any still-in-flight fence
        // (shouldn't happen but be defensive) is safe to
        // destroy.
        unsafe {
            let _ = pool.vk.device.device_wait_idle();
            for &f in &pool.free {
                pool.vk.device.destroy_fence(f, None);
            }
            for &f in &pool.leaked_fences {
                pool.vk.device.destroy_fence(f, None);
            }
        }
    }
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

/// True iff a cursor-plane ioctl error means the driver does not
/// implement the (legacy) cursor ioctls at all — a permanent,
/// per-driver condition that warrants latching the HW cursor strategy
/// off and falling back to the SW composite path.
///
/// Apple's DCP display driver (Asahi) returns `ENXIO` from
/// `DRM_IOCTL_MODE_CURSOR2`; other atomic-only drivers may return
/// `ENODEV` / `EOPNOTSUPP`. Recoverable / ambiguous errors (`EBUSY`,
/// `EINVAL`, non-OS errors) must NOT latch — `EBUSY` is transient and
/// latching it would needlessly kill the HW cursor on drivers that do
/// support the ioctl (e.g. amdgpu). This mirrors Xorg's modesetting
/// driver, which clears `use_hw_cursor` when the cursor ioctl fails.
fn cursor_err_disables_hw(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::ENXIO | libc::ENODEV | libc::EOPNOTSUPP)
    )
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

fn cursor_error_is_transient_fallback(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::InvalidInput
}

fn classify_cursor_failure_pair(
    operation_error: &io::Error,
    rollback_error: Option<&io::Error>,
) -> CursorFailureDisposition {
    if cursor_err_disables_hw(operation_error) || rollback_error.is_some_and(cursor_err_disables_hw)
    {
        CursorFailureDisposition::Permanent
    } else if cursor_error_is_transient_fallback(operation_error)
        || rollback_error.is_some_and(cursor_error_is_transient_fallback)
    {
        CursorFailureDisposition::Transient
    } else {
        CursorFailureDisposition::Unchanged
    }
}

fn cursor_dimensions_fit(plane_width: u32, plane_height: u32, width: u32, height: u32) -> bool {
    width <= plane_width && height <= plane_height
}

fn drm_device_is_nvidia(device: &drm::Device) -> bool {
    use ::drm::Device as _;
    device
        .get_driver()
        .ok()
        .map(|driver| {
            driver
                .name()
                .to_string_lossy()
                .to_ascii_lowercase()
                .contains("nvidia")
        })
        .unwrap_or(false)
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

impl CrtcKey {
    pub(crate) fn new(
        device_key: crate::platform::drm::DrmDeviceKey,
        crtc: ::drm::control::crtc::Handle,
    ) -> Self {
        Self { device_key, crtc }
    }

    pub(crate) fn for_output(output: &ActiveOutput) -> Self {
        Self::new(output.key.device_key, output.output.crtc)
    }
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

impl KmsCursorState {
    pub(crate) fn new(nvidia_policy_disabled: bool) -> Self {
        Self {
            plane: None,
            pending_move: None,
            permanently_disabled: false,
            initialization_retryable: false,
            headless_deferred: true,
            topology_blocked: false,
            transient_fallback_crtcs: HashMap::new(),
            nvidia_policy_disabled,
            sprite_signature: None,
        }
    }

    fn available_on(&self, crtc: ::drm::control::crtc::Handle) -> bool {
        self.plane.is_some()
            && !self.permanently_disabled
            && !self.topology_blocked
            && !self.nvidia_policy_disabled
            && self
                .transient_fallback_crtcs
                .get(&crtc)
                .is_none_or(|retry| retry.remaining_sw_retires == 0)
    }

    fn note_einval(&mut self, crtc: ::drm::control::crtc::Handle) {
        let retry = self
            .transient_fallback_crtcs
            .entry(crtc)
            .or_insert(TransientCursorFallback {
                remaining_sw_retires: 0,
                failures: 0,
            });
        retry.failures = retry.failures.saturating_add(1);
        let shift = retry.failures.saturating_sub(1).min(3);
        retry.remaining_sw_retires = 1_u8 << shift;
    }

    fn note_cursor_success(&mut self, crtc: ::drm::control::crtc::Handle) {
        self.transient_fallback_crtcs.remove(&crtc);
    }

    fn note_cursor_failure_pair(
        &mut self,
        crtc: ::drm::control::crtc::Handle,
        operation_error: &io::Error,
        rollback_error: Option<&io::Error>,
    ) -> CursorFailureDisposition {
        let disposition = classify_cursor_failure_pair(operation_error, rollback_error);
        match disposition {
            CursorFailureDisposition::Unchanged => {}
            CursorFailureDisposition::Transient => {
                self.note_einval(crtc);
                // Once eligibility changes, the cursorless scene handoff owns
                // recovery. A position-only retry must not race it or clear a
                // failed bind/hotspot/upload observation.
                self.pending_move = None;
            }
            CursorFailureDisposition::Permanent => {
                self.permanently_disabled = true;
                self.pending_move = None;
                self.transient_fallback_crtcs.clear();
            }
        }
        disposition
    }

    fn note_initialization_failure(&mut self, error: &io::Error) {
        self.headless_deferred = false;
        let permanent = cursor_err_disables_hw(error);
        self.permanently_disabled |= permanent;
        self.initialization_retryable = !permanent;
    }

    fn should_initialize_headless_deferred(&self, has_active_crtcs: bool) -> bool {
        has_active_crtcs
            && self.headless_deferred
            && self.plane.is_none()
            && !self.permanently_disabled
            && !self.initialization_retryable
    }

    fn should_retry_initialization(&self, has_active_crtcs: bool) -> bool {
        has_active_crtcs
            && self.plane.is_none()
            && !self.permanently_disabled
            && self.initialization_retryable
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

impl CursorMoveOutcome {
    fn merge(&mut self, other: Self) {
        self.ebusy_count = self.ebusy_count.saturating_add(other.ebusy_count);
        self.fallback_changed |= other.fallback_changed;
        self.retry_required |= other.retry_required;
    }
}

fn apply_cursor_move_rollback_result(
    state: &mut KmsCursorState,
    crtc: ::drm::control::crtc::Handle,
    move_error: &io::Error,
    rollback: io::Result<()>,
    outcome: &mut CursorMoveOutcome,
) -> bool {
    let disposition = state.note_cursor_failure_pair(crtc, move_error, rollback.as_ref().err());
    if disposition != CursorFailureDisposition::Unchanged {
        outcome.fallback_changed = true;
    }
    match rollback {
        Ok(()) => false,
        Err(_) => {
            outcome.retry_required = true;
            // A classified eligibility failure is recovered by the scene's
            // visibility-aware cursorless hide transaction, not by a stale
            // position-only retry. Keep pending only for an unclassified
            // ownership uncertainty.
            disposition == CursorFailureDisposition::Unchanged
        }
    }
}

fn apply_cursor_show_failure_state(
    state: &mut KmsCursorState,
    crtc: ::drm::control::crtc::Handle,
    error: &crate::kms::cursor_plane::CursorShowError,
    desired_move: (i32, i32, u16, u16),
) -> CursorFailureDisposition {
    let disposition =
        state.note_cursor_failure_pair(crtc, error.operation_error(), error.rollback_error());
    if error.remains_visible()
        && disposition == CursorFailureDisposition::Unchanged
        && !error.needs_full_rebind()
    {
        state.pending_move = Some(desired_move);
    }
    disposition
}

fn apply_cursor_operation_result(
    state: &mut KmsCursorState,
    crtc: ::drm::control::crtc::Handle,
    result: &io::Result<()>,
) -> CursorFailureDisposition {
    result
        .as_ref()
        .err()
        .map_or(CursorFailureDisposition::Unchanged, |error| {
            state.note_cursor_failure_pair(crtc, error, None)
        })
}

/// Stable renderer endpoint identity. Verified devices are keyed by their DRM
/// render node. The explicit unverified fallback covers a targeted selection
/// when every candidate lacks DRM identity, and a headless scored selection
/// whose candidate advertises no render node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RenderDeviceId {
    DrmRender(crate::platform::drm::DrmDeviceKey),
    UnverifiedFallback,
}

/// One Vulkan renderer endpoint. Its physical-device handle belongs to the
/// platform's `VkContext` instance; KMS devices are represented separately.
pub(crate) struct RenderDevice {
    pub(crate) id: RenderDeviceId,
    pub(crate) physical_device: vk::PhysicalDevice,
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

impl RenderDevice {
    #[must_use]
    pub(crate) fn is_same_device_as(&self, kms: &KmsDevice) -> Option<bool> {
        self.advertised_primary_node
            .map(|primary| primary == kms.key)
    }
}

/// One opened display/KMS device. Renderer identity and render-node resources
/// deliberately live in `RenderDevice` instead.
pub(crate) struct KmsDevice {
    pub(crate) key: crate::platform::drm::DrmDeviceKey,
    pub(crate) device: Rc<drm::Device>,
    pub(crate) cursor: KmsCursorState,
}

fn install_cursor_plane_for_device(
    kms_device: &mut KmsDevice,
    crtcs: &[::drm::control::crtc::Handle],
    boundary: &str,
    plane: crate::kms::cursor_plane::CursorPlane,
) {
    kms_device.cursor.topology_blocked = !plane.supports_crtcs(crtcs);
    kms_device.cursor.initialization_retryable = false;
    kms_device.cursor.headless_deferred = false;
    log::info!(
        "render cursor: device {} initialized {}x{} ARGB8888 for {} active CRTC(s) at {boundary}; topology_blocked={}",
        kms_device.key,
        plane.width(),
        plane.height(),
        crtcs.len(),
        kms_device.cursor.topology_blocked,
    );
    kms_device.cursor.plane = Some(plane);
}

fn initialize_cursor_plane_for_device(
    kms_device: &mut KmsDevice,
    crtcs: &[::drm::control::crtc::Handle],
    boundary: &str,
) {
    kms_device.cursor.headless_deferred = false;
    match crate::kms::cursor_plane::CursorPlane::new(Rc::clone(&kms_device.device), crtcs) {
        Ok(plane) => install_cursor_plane_for_device(kms_device, crtcs, boundary, plane),
        Err(error) => {
            kms_device.cursor.note_initialization_failure(&error);
            let retry = if kms_device.cursor.initialization_retryable {
                "will retry at an explicit topology/resume boundary"
            } else {
                "cursor support is permanently unavailable on this device"
            };
            log::warn!(
                "render cursor: device {} initialization failed at {boundary} ({error}); using software cursor, {retry}",
                kms_device.key
            );
        }
    }
}

fn rollback_initial_scanout_with<F>(
    devices: &[KmsDevice],
    outputs: &mut [ActiveOutput],
    disable: &mut F,
) where
    F: FnMut(&drm::Device, &crate::platform::drm::Output) -> io::Result<()>,
{
    for layout in outputs.iter_mut().rev() {
        let Some(device) = devices
            .iter()
            .find(|device| device.key == layout.key.device_key)
        else {
            log::error!(
                "initial scanout rollback: no DRM device {} for {}; \
                 leaving its buffers for DRM-fd close",
                layout.key.device_key,
                layout.output.connector_name,
            );
            layout.swapchain.disarm();
            continue;
        };
        if let Err(err) = disable(&device.device, &layout.output) {
            log::warn!(
                "initial scanout rollback: failed to disable {} on {}: {err}; \
                 leaving its buffers for DRM-fd close",
                layout.output.connector_name,
                device.key,
            );
            layout.swapchain.disarm();
        }
    }
}

/// RAII coverage for every fallible step between the initial modeset and a
/// fully-built [`PlatformBackend`]. The output buffers stay borrowed until
/// this guard is either dropped (rollback) or explicitly disarmed when
/// responsibility transfers into `PlatformBackend::drop`.
struct InitialScanoutRollbackGuard<'a, F>
where
    F: FnMut(&drm::Device, &crate::platform::drm::Output) -> io::Result<()>,
{
    devices: &'a [KmsDevice],
    outputs: &'a mut [ActiveOutput],
    disable: F,
    armed: bool,
}

impl<'a, F> InitialScanoutRollbackGuard<'a, F>
where
    F: FnMut(&drm::Device, &crate::platform::drm::Output) -> io::Result<()>,
{
    fn new_with(devices: &'a [KmsDevice], outputs: &'a mut [ActiveOutput], disable: F) -> Self {
        Self {
            devices,
            armed: !outputs.is_empty(),
            outputs,
            disable,
        }
    }

    fn devices(&self) -> &[KmsDevice] {
        self.devices
    }

    fn outputs(&self) -> &[ActiveOutput] {
        self.outputs
    }

    fn is_armed(&self) -> bool {
        self.armed
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl<F> Drop for InitialScanoutRollbackGuard<'_, F>
where
    F: FnMut(&drm::Device, &crate::platform::drm::Output) -> io::Result<()>,
{
    fn drop(&mut self) {
        if self.armed {
            rollback_initial_scanout_with(self.devices, self.outputs, &mut self.disable);
            self.armed = false;
        }
    }
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

    // Vulkan side. `Option` only to support test fixtures that
    // skip Vk init (`for_tests`). Production `open_with_commit`
    // always returns `Some`. v2 has no pixman fallback.
    pub(crate) vk: Option<Arc<VkContext>>,
    /// Wrapped in `Option` for the same reason. Drop order
    /// matters: ops_command_pool BEFORE fence_pool BEFORE vk
    /// (handled by struct field order — Rust drops fields in
    /// declaration order).
    pub(crate) ops_command_pool: Option<OpsCommandPool>,
    pub(crate) fence_pool: Option<FencePool>,

    /// Stage 3f.10: recycled `(image, view, memory)` triples for
    /// CreatePixmap. Reuses v1's `PixmapPool` verbatim — its
    /// `try_take` / `try_return` API + bucket-cap + size-cap
    /// logic is backend-agnostic. Bypassed by the test fixture
    /// (`for_tests`) and on `for_tests_with_vk` (the harness
    /// constructs `RenderEngine` directly without going through
    /// `open_with_commit`).
    pub(crate) pixmap_pool: Option<Arc<crate::kms::vk::pixmap_pool::PixmapPool>>,

    /// Per-output scanout BO pool. `None` if a particular
    /// output's allocation failed (rare; e.g. RADV/gfx8 quirks).
    /// Stage 2c+ paint paths skip output indices with `None`
    /// pool, mirroring v1's behaviour.
    pub(crate) scanout_pools: Vec<Option<ScanoutBoPool>>,

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
}

fn build_render_device_inventory(
    vk: &Arc<VkContext>,
    render_node: Option<crate::kms::render_node::OpenedRenderNode>,
) -> io::Result<(Vec<RenderDevice>, RenderDeviceId)> {
    let mut render_devices: Vec<_> = vk
        .drm_physical_devices
        .iter()
        .map(|entry| RenderDevice {
            id: RenderDeviceId::DrmRender(
                entry
                    .identity
                    .render
                    .expect("VkContext inventory contains only render-identified devices"),
            ),
            physical_device: entry.physical_device,
            advertised_primary_node: entry.identity.primary,
            advertised_render_node: entry.identity.render,
            render_node: None,
            render_node_device: None,
            syncobj_timeline: false,
        })
        .collect();

    let selected_index = render_devices
        .iter()
        .position(|device| device.physical_device == vk.physical_device)
        .unwrap_or_else(|| {
            let identity = vk.selected_drm_identity;
            render_devices.push(RenderDevice {
                id: RenderDeviceId::UnverifiedFallback,
                physical_device: vk.physical_device,
                advertised_primary_node: identity.and_then(|identity| identity.primary),
                advertised_render_node: identity.and_then(|identity| identity.render),
                render_node: None,
                render_node_device: None,
                syncobj_timeline: false,
            });
            render_devices.len() - 1
        });

    let selected = &mut render_devices[selected_index];
    if let Some(render_node) = render_node {
        validate_render_node_attachment(selected.id, render_node.key())?;
        let render_node_device = drm::Device::open_render_node(
            render_node
                .path()
                .to_str()
                .unwrap_or("<non-UTF-8 render-node path>"),
        )
        .and_then(|device| {
            render_node.verify_fd(device.as_fd())?;
            Ok(Arc::new(device))
        })
        .map_err(|error| {
            log::warn!(
                "render device: failed to reopen selected DRM render node {}: {error}; syncobj support unavailable",
                render_node.path().display(),
            );
        })
        .ok();
        let syncobj_timeline = render_node_device
            .as_ref()
            .and_then(|device| {
                use ::drm::Device as _;
                device
                    .get_driver_capability(::drm::DriverCapability::TimelineSyncObj)
                    .ok()
            })
            .is_some_and(|value| value != 0);
        selected.render_node = Some(render_node);
        selected.render_node_device = render_node_device;
        selected.syncobj_timeline = syncobj_timeline;
    }

    let selected_id = render_devices[selected_index].id;
    Ok((render_devices, selected_id))
}

fn validate_render_node_attachment(
    selected: RenderDeviceId,
    opened: crate::platform::drm::DrmDeviceKey,
) -> io::Result<()> {
    match selected {
        RenderDeviceId::DrmRender(advertised) if advertised != opened => {
            Err(io::Error::other(format!(
                "selected Vulkan renderer advertises DRM render node {advertised}, but the opened render endpoint is {opened}"
            )))
        }
        RenderDeviceId::DrmRender(_) | RenderDeviceId::UnverifiedFallback => Ok(()),
    }
}

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

/// Device-qualified connector metadata used to reconcile lightweight probes
/// with the stable RANDR registry without inventing a CRTC/plane assignment.
#[derive(Debug, Clone)]
pub(crate) struct ConnectorSnapshot {
    pub(crate) key: OutputKey,
    pub(crate) modes: Vec<crate::platform::drm::Mode>,
    pub(crate) mm_width: u32,
    pub(crate) mm_height: u32,
    pub(crate) edid: Vec<u8>,
    pub(crate) connector_type: String,
}

impl ConnectorSnapshot {
    fn from_probe(
        device_key: crate::platform::drm::DrmDeviceKey,
        probe: &crate::platform::drm::ConnectorSnapshotProbe,
    ) -> Self {
        Self {
            key: OutputKey::new(device_key, probe.connector_name.clone()),
            modes: probe.modes.clone(),
            mm_width: probe.mm_width,
            mm_height: probe.mm_height,
            edid: probe.edid.clone(),
            connector_type: probe.connector_type.clone(),
        }
    }

    pub(crate) fn preserves_active_output(&self, output: &ActiveOutput) -> bool {
        // A monitor replacement on the same connector does not revoke the
        // mode already programmed in KMS. Keep that live route until a RANDR
        // client selects a replacement; only a disconnected/no-mode probe
        // forces it Off. The later metadata-refresh replay will make the
        // registry's exact mode identities authoritative without changing
        // this live-mode preservation policy.
        self.key == output.key && !self.modes.is_empty()
    }
}

/// Pure recompute of the virtual-screen extent from `(x, y, width, height)`.
///
/// 2-D: `fb_w = max(x + width)`, `fb_h = max(y + height)`. A client may
/// place a CRTC at any `(x, y)` (e.g. a monitor stacked below), so the
/// framebuffer must encompass `y + height`, not just `max(height)`.
pub(crate) fn recompute_fb_extent_from(layouts: &[(i32, i32, u16, u16)]) -> (u16, u16) {
    let fb_w = layouts
        .iter()
        .map(|(x, _, w, _)| x.saturating_add(i32::from(*w)))
        .map(|v| u16::try_from(v.max(0)).unwrap_or(u16::MAX))
        .max()
        .unwrap_or(0);
    let fb_h = layouts
        .iter()
        .map(|(_, y, _, h)| y.saturating_add(i32::from(*h)))
        .map(|v| u16::try_from(v.max(0)).unwrap_or(u16::MAX))
        .max()
        .unwrap_or(0);
    (fb_w, fb_h)
}

impl Drop for PlatformBackend {
    fn drop(&mut self) {
        if !self.initial_scanout_rollback_armed {
            return;
        }
        rollback_initial_scanout_with(
            &self.devices,
            &mut self.outputs,
            &mut drm::modeset::disable_output,
        );
        self.initial_scanout_rollback_armed = false;
    }
}

impl PlatformBackend {
    /// Mark the initial modesets as owned by a fully constructed backend.
    /// Normal shutdown will disable them through `KmsBackend::disable_output`;
    /// construction failures leave this armed so `Drop` performs rollback.
    pub(crate) fn disarm_initial_scanout_rollback(&mut self) {
        self.initial_scanout_rollback_armed = false;
    }

    /// Backend constructor. Opens DRM, initialises Vk,
    /// allocates per-output scanout pools, builds the fence pool.
    /// Fatal initialization failures tear down already-allocated resources
    /// and return `Err`.
    ///
    /// # Errors
    ///
    /// Propagates platform-init failures from `core_platform_init`,
    /// Vk init failures from `VkContext::new`, command-pool allocation
    /// failures from `OpsCommandPool::new`. An individual `ScanoutBoPool`
    /// failure is non-fatal while another output remains displayable; startup
    /// fails if every connected output lacks a live pool.
    pub(crate) fn open_with_commit(
        device_paths: &[PathBuf],
        commit: fn(
            &drm::Device,
            &crate::platform::drm::Output,
            ::drm::control::framebuffer::Handle,
        ) -> io::Result<()>,
    ) -> io::Result<Self> {
        // `core_platform_init` runs the hardware-Vulkan preflight after it
        // discovers an active output but before allocating or committing
        // scanout. Headless platforms skip it and may use software Vulkan.
        let platform_init = core_platform_init(device_paths, commit)?;
        Self::from_platform_init(platform_init)
    }

    /// Shared bring-up body: Vk + pools + epoll + cursor plane init
    /// from a pre-built [`PlatformInit`]. Called by
    /// [`open_with_commit`] (Direct mode — the only mode).
    fn from_platform_init(platform_init: PlatformInit) -> io::Result<Self> {
        let PlatformInit {
            devices,
            render_node,
            mut layouts,
            fb_w,
            fb_h,
            input_ctx,
        } = platform_init;

        let mut devices: Vec<KmsDevice> = devices
            .into_iter()
            .map(|device| {
                let cursor = KmsCursorState::new(drm_device_is_nvidia(&device.device));
                KmsDevice {
                    key: device.key,
                    device: device.device,
                    cursor,
                }
            })
            .collect();

        // One independently-owned cursor buffer/state per DRM device. A
        // device with no active startup CRTC stays explicitly deferred until
        // its first successful RANDR enable inserts an ActiveOutput.
        for kms_device in &mut devices {
            let crtcs: Vec<_> = layouts
                .iter()
                .filter(|layout| layout.key.device_key == kms_device.key)
                .map(|layout| layout.output.crtc)
                .collect();
            if crtcs.is_empty() {
                log::info!(
                    "render cursor: device {} has no active startup CRTC; initialization deferred",
                    kms_device.key
                );
                continue;
            }
            initialize_cursor_plane_for_device(kms_device, &crtcs, "active startup");
        }

        let mut initial_scanout_rollback = InitialScanoutRollbackGuard::new_with(
            &devices,
            &mut layouts,
            drm::modeset::disable_output,
        );

        let requested_render_node = render_node.as_ref().map(|node| node.key());
        let vk_result = devices.first().map_or_else(VkContext::new, |display| {
            VkContext::new_for_render_device(requested_render_node, display.key)
        });
        let vk = match vk_result {
            Ok(v) => v,
            Err(e) => {
                return Err(io::Error::other(format!(
                    "render PlatformBackend: VkContext init failed (render backend requires Vulkan; \
                     no pixman fallback): {e:?}"
                )));
            }
        };
        log::info!(
            "render PlatformBackend: VkContext ready (driver_id={:?}, device_type={:?})",
            vk.driver_id,
            vk.device_type,
        );
        let (render_devices, selected_render_device) =
            build_render_device_inventory(&vk, render_node)?;
        if let Some(display) = devices.first() {
            let selected = render_devices
                .iter()
                .find(|device| device.id == selected_render_device)
                .expect("selected renderer is present in renderer inventory");
            let kms_relationship = match selected.is_same_device_as(display) {
                Some(true) => "same-device",
                Some(false) => "different-device",
                None => "unknown",
            };
            log::info!(
                "render PlatformBackend: selected renderer {:?} (render={:?}, primary={:?}) has {kms_relationship} relationship to KMS device {}",
                selected.physical_device,
                selected.advertised_render_node,
                selected.advertised_primary_node,
                display.key,
            );
        }

        // Refuse to drive real KMS scanout off a software rasterizer.
        // If the only Vulkan device is llvmpipe/lavapipe (CPU type) —
        // typically because the GPU's hardware Vulkan driver is missing
        // (e.g. nvidia removed but nouveau not loaded, so Mesa falls back
        // to llvmpipe) — then exporting a host-memory buffer and handing
        // it to a real GPU's atomic scanout commit HARD-HANGS the machine
        // (observed on nouveau/Pascal: no SSH, nothing in the journal).
        // Fail fast with an actionable error instead of wedging the box.
        // Venus (virtio-gpu) reports VIRTUAL_GPU, not CPU, so it is not
        // affected; the env override exists for any deliberate
        // software-scanout setup (e.g. lavapipe under vng).
        if !initial_scanout_rollback.outputs().is_empty()
            && vk.is_software_rasterizer()
            && std::env::var_os("YSERVER_ALLOW_SOFTWARE_VULKAN").is_none()
        {
            return Err(io::Error::other(format!(
                "render PlatformBackend: the only Vulkan device is a software rasterizer \
                 (device_type=CPU, driver_id={:?} — llvmpipe/lavapipe). Driving real KMS \
                 scanout off software Vulkan hard-hangs the machine on hardware that can't \
                 scan out a host-memory buffer. Refusing to start. Install a hardware Vulkan \
                 driver for the scanout GPU (radv / anv / nvk), or check your GPU/driver setup \
                 (e.g. nvidia removed but nouveau not loaded → Mesa falls back to llvmpipe). \
                 To override (e.g. virtio-gpu under vng), set YSERVER_ALLOW_SOFTWARE_VULKAN=1.",
                vk.driver_id,
            )));
        }
        if initial_scanout_rollback.outputs().is_empty() && vk.is_software_rasterizer() {
            log::info!(
                "render PlatformBackend: using software Vulkan for headless rendering; no KMS outputs are active"
            );
        }

        let ops_command_pool = OpsCommandPool::new(Arc::clone(&vk))
            .map_err(|e| io::Error::other(format!("ops command pool: {e:?}")))?;

        let fence_pool = FencePool::new(Arc::clone(&vk));

        // Stage 3f.10: pixmap pool reuses v1's allocator verbatim.
        // MATE / xfce4 / GTK widgets churn ~90 pixmap allocs/sec;
        // without this every CreatePixmap pays a full
        // create_image + allocate_memory + bind + create_view cycle.
        // Registers with the GLOBAL_LATEST_POOL hook so the main-
        // loop telemetry path can sample hit/miss counters even
        // though v2 doesn't own the telemetry-emit cadence directly.
        let pixmap_pool = {
            let p = Arc::new(crate::kms::vk::pixmap_pool::PixmapPool::new(Arc::clone(
                &vk,
            )));
            crate::kms::vk::pixmap_pool::register_for_telemetry(&p);
            Some(p)
        };

        // One ScanoutBoPool per output, 3-BO depth (matches v1).
        let mut scanout_pools = Vec::with_capacity(initial_scanout_rollback.outputs().len());
        let mut bo_generations = Vec::with_capacity(initial_scanout_rollback.outputs().len());
        let mut scanout_alloc_errors: Vec<String> = Vec::new();
        for (i, layout) in initial_scanout_rollback.outputs().iter().enumerate() {
            let w = u32::from(layout.width);
            let h = u32::from(layout.height);
            let device = initial_scanout_rollback
                .devices()
                .iter()
                .find(|device| device.key == layout.key.device_key)
                .ok_or_else(|| {
                    io::Error::other(format!(
                        "render PlatformBackend: output {} belongs to missing DRM device {}",
                        layout.key.connector_name, layout.key.device_key
                    ))
                })?;
            match ScanoutBoPool::allocate(
                Arc::clone(&vk),
                Rc::clone(&device.device),
                w,
                h,
                3,
                &layout.output.scanout_modifiers,
            ) {
                Ok(pool) => {
                    let n = pool.bos.len();
                    scanout_pools.push(Some(pool));
                    bo_generations.push(vec![BoGenerationEntry::default(); n]);
                }
                Err(e) => {
                    log::warn!(
                        "render: ScanoutBoPool allocate failed for output {i} ({}x{}): {e:?} \
                         — output will be skipped from compose",
                        w,
                        h,
                    );
                    scanout_alloc_errors.push(format!("output {i} ({w}x{h}): {e}"));
                    scanout_pools.push(None);
                    bo_generations.push(Vec::new());
                }
            }
        }
        // Refuse to run invisibly: if there are connected outputs but none
        // got a scanout pool, there is nothing to display — fail loudly
        // instead of leaving a silent black screen. (Split-GPU scanout
        // with no shared modifier, e.g. RPi 4/400, lands here.)
        let live_pool_count = scanout_pools.iter().filter(|p| p.is_some()).count();
        if let Err(msg) = check_scanout_liveness(
            initial_scanout_rollback.outputs().len(),
            live_pool_count,
            &scanout_alloc_errors,
        ) {
            return Err(io::Error::other(format!("render PlatformBackend: {msg}")));
        }
        let first_pageflip_logged = vec![false; initial_scanout_rollback.outputs().len()];

        // Stage 5 Task 6.1: backend-internal poll FD + wakeup
        // eventfd for deferred PRESENT completion. The eventfd lives
        // inside the poll set under `WAKEUP_EVENTFD_TOKEN`; per-entry
        // sync_file FDs join later via the enqueue path.
        let wakeup_eventfd = nix::sys::eventfd::EventFd::from_value_and_flags(
            0,
            nix::sys::eventfd::EfdFlags::EFD_CLOEXEC | nix::sys::eventfd::EfdFlags::EFD_NONBLOCK,
        )
        .map_err(|e| io::Error::other(format!("eventfd: {e}")))?;

        // Backend-internal readiness set (epoll/kqueue). The wakeup
        // eventfd joins it under WAKEUP_EVENTFD_TOKEN; per-batch
        // sync_file FDs are added later via the enqueue path.
        let present_completion_epfd =
            crate::kms::render::completion_poller::CompletionPoller::new()?;
        present_completion_epfd.register(wakeup_eventfd.as_fd(), WAKEUP_EVENTFD_TOKEN)?;

        let submit_group = SubmitGroup::new();
        #[cfg(target_os = "linux")]
        let hotplug_monitor = match crate::kms::hotplug::DrmHotplugMonitor::new() {
            Ok(monitor) => monitor,
            Err(e) => {
                // Don't fail bring-up — yserver runs fine without runtime
                // hotplug — but surface WHY (udev/netlink/permission) so a
                // silently-disabled monitor is diagnosable.
                log::warn!(
                    "render PlatformBackend: DRM hotplug monitor unavailable ({e}); \
                     runtime display hotplug disabled"
                );
                None
            }
        };

        log::info!(
            "render PlatformBackend: ready — {} outputs, fb {}x{}, {} scanout pools live",
            initial_scanout_rollback.outputs().len(),
            fb_w,
            fb_h,
            scanout_pools.iter().filter(|p| p.is_some()).count(),
        );

        // `Self` construction below is infallible. Transfer rollback
        // responsibility from the borrowing stack guard to PlatformBackend's
        // Drop implementation so the outer KmsBackend constructor remains
        // covered until it explicitly disarms the completed backend.
        let initial_scanout_rollback_armed = initial_scanout_rollback.is_armed();
        initial_scanout_rollback.disarm();
        drop(initial_scanout_rollback);

        Ok(Self {
            initial_scanout_rollback_armed,
            devices,
            render_devices,
            selected_render_device: Some(selected_render_device),
            outputs: layouts,
            fb_w,
            fb_h,
            ust_msc: std::collections::HashMap::new(),
            completion_clocks: std::collections::HashMap::new(),
            software_msc: std::collections::HashMap::new(),
            input_ctx,
            #[cfg(target_os = "linux")]
            hotplug_monitor,
            present_completion_epfd,
            wakeup_eventfd,
            vk: Some(vk),
            ops_command_pool: Some(ops_command_pool),
            fence_pool: Some(fence_pool),
            pixmap_pool,
            scanout_pools,
            bo_generations,
            next_present_generation: 0,
            first_pageflip_logged,
            renderer_failed: false,
            shutting_down: false,
            submit_group,
            last_flush_outcome: None,
            force_next_submit_failure: false,
        })
    }

    /// Headless test seed. No live DRM device, no Vk, single
    /// stub 800×600 output. Mirrors `KmsBackend::for_tests`'s
    /// existing shape from Stage 1b.
    #[doc(hidden)]
    pub(crate) fn for_tests() -> Self {
        let wakeup_eventfd = nix::sys::eventfd::EventFd::from_value_and_flags(
            0,
            nix::sys::eventfd::EfdFlags::EFD_CLOEXEC | nix::sys::eventfd::EfdFlags::EFD_NONBLOCK,
        )
        .expect("test eventfd");

        let present_completion_epfd =
            crate::kms::render::completion_poller::CompletionPoller::new().expect("test poller");
        present_completion_epfd
            .register(wakeup_eventfd.as_fd(), WAKEUP_EVENTFD_TOKEN)
            .expect("test poller register");
        #[cfg(target_os = "linux")]
        let hotplug_monitor = None;
        let device_key = crate::platform::drm::DrmDeviceKey { major: 0, minor: 0 };
        let device = Rc::new(drm::Device::for_tests().expect("test drm device"));
        Self {
            initial_scanout_rollback_armed: false,
            devices: vec![KmsDevice {
                key: device_key,
                device,
                cursor: KmsCursorState::new(false),
            }],
            render_devices: Vec::new(),
            selected_render_device: None,
            outputs: vec![ActiveOutput::new(
                device_key,
                crate::platform::drm::Output {
                    connector: ::drm::control::from_u32(1).unwrap(),
                    connector_name: "test".to_string(),
                    encoder: ::drm::control::from_u32(1).unwrap(),
                    crtc: ::drm::control::from_u32(1).unwrap(),
                    plane: ::drm::control::from_u32(1).unwrap(),
                    // SAFETY: tests never pass this mode to DRM.
                    mode: unsafe { std::mem::zeroed() },
                    picked: crate::platform::drm::Mode {
                        name: "test".to_string(),
                        width: 800,
                        height: 600,
                        vrefresh: 60,
                        preferred: true,
                        ..Default::default()
                    },
                    plane_fb_id_prop: ::drm::control::from_u32(1).unwrap(),
                    plane_crtc_id_prop: ::drm::control::from_u32(1).unwrap(),
                    plane_src_x_prop: ::drm::control::from_u32(1).unwrap(),
                    plane_src_y_prop: ::drm::control::from_u32(1).unwrap(),
                    plane_src_w_prop: ::drm::control::from_u32(1).unwrap(),
                    plane_src_h_prop: ::drm::control::from_u32(1).unwrap(),
                    plane_crtc_x_prop: ::drm::control::from_u32(1).unwrap(),
                    plane_crtc_y_prop: ::drm::control::from_u32(1).unwrap(),
                    plane_crtc_w_prop: ::drm::control::from_u32(1).unwrap(),
                    plane_crtc_h_prop: ::drm::control::from_u32(1).unwrap(),
                    plane_in_fence_fd_prop: None,
                    crtc_out_fence_ptr_prop: None,
                    scanout_modifiers: Vec::new(),
                    mm_width: 0,
                    mm_height: 0,
                    edid: Vec::new(),
                    connector_type: "unknown".to_string(),
                    modes: vec![crate::platform::drm::Mode {
                        name: "test".to_string(),
                        width: 800,
                        height: 600,
                        vrefresh: 60,
                        preferred: true,
                        ..Default::default()
                    }],
                },
                drm::Swapchain::empty_for_tests(),
                0,
                0,
            )],
            fb_w: 800,
            fb_h: 600,
            ust_msc: std::collections::HashMap::new(),
            completion_clocks: std::collections::HashMap::new(),
            software_msc: std::collections::HashMap::new(),
            input_ctx: None,
            #[cfg(target_os = "linux")]
            hotplug_monitor,
            present_completion_epfd,
            wakeup_eventfd,
            vk: None,
            ops_command_pool: None,
            fence_pool: None,
            pixmap_pool: None,
            scanout_pools: vec![None],
            bo_generations: vec![Vec::new()],
            next_present_generation: 0,
            first_pageflip_logged: vec![false],
            renderer_failed: false,
            shutting_down: false,
            submit_group: SubmitGroup::new(),
            last_flush_outcome: None,
            force_next_submit_failure: false,
        }
    }

    /// Attach a live Vulkan context to the headless test fixture while
    /// preserving the renderer-inventory invariant used by production.
    pub(crate) fn attach_test_vk_context(&mut self, vk: Arc<VkContext>) {
        let (render_devices, selected) = build_render_device_inventory(&vk, None)
            .expect("a test Vulkan context without an opened render node cannot mismatch");
        self.render_devices = render_devices;
        self.selected_render_device = Some(selected);
        self.vk = Some(vk);
    }

    pub(crate) fn fb_dimensions(&self) -> (u16, u16) {
        (self.fb_w, self.fb_h)
    }

    // ── Stage 5 Phase B — hardware cursor-plane hooks ─────────────
    //
    // The plan splits the legacy `set_cursor2`-driven path into
    // narrow per-CRTC primitives so the Phase D `PendingAck`
    // transition state machine can drive the plane without
    // re-introducing the multi-output double-cursor hazard.
    //
    // - `cursor_plane_available_for_output()` is consulted by `build_scene`'s
    //   pure `CursorAssignment` decision. It resolves the output's stable
    //   device key before looking at that KmsDevice's independent plane and
    //   fallback state.
    // - `cursor_plane_upload_image_for_output` memcpys bytes into the owning
    //   device's dumb buffer ONLY. It does NOT call `set_cursor2`.
    //   `set_cursor2(Some, …)` IS the show operation in legacy DRM;
    //   upload-as-show would prematurely bind on CRTCs whose Sw→Hw
    //   transition hasn't retired yet.
    // - `cursor_plane_show_on_crtc` is the sole `set_cursor2(Some,
    //   …)` site, called per-output from `handle_page_flip_complete`
    //   when that CRTC's PendingAck queues a `ShowOnRetire`. The
    //   immediate `move_to` follow-up is required because some
    //   kernels reset the cursor position to (0, 0) on rebind (v1
    //   pattern at `backend.rs:2173`).
    // - A steady-state sprite swap is queued per output and repeats the full
    //   upload+ShowOnRetire transaction. It never treats several cards as one
    //   atomic cursor resource.
    // - `cursor_plane_move` is the pointer-fast-path entry point;
    //   one ioctl per visible CRTC, no GPU work.
    // - `cursor_plane_hide_on_crtc` and `cursor_plane_hide_all`
    //   serve Phase D' output-local / global recovery respectively.

    /// True iff the cursor plane was successfully initialised at
    /// boot AND hasn't been disabled by an auto-fallback latch. The
    /// scene strategy decision (`CursorAssignment`) gates on this
    /// without holding a `PlatformBackend` borrow.
    #[must_use]
    pub(crate) fn cursor_plane_available(&self) -> bool {
        self.outputs
            .iter()
            .enumerate()
            .any(|(output_idx, _)| self.cursor_plane_available_for_output(output_idx))
    }

    /// True iff this output's owning DRM device currently has an eligible
    /// cursor plane. Raw CRTC handles never participate in device selection.
    #[must_use]
    pub(crate) fn cursor_plane_available_for_output(&self, output_idx: usize) -> bool {
        let Some(layout) = self.outputs.get(output_idx) else {
            return false;
        };
        self.device_for_key(layout.key.device_key)
            .is_some_and(|device| device.cursor.available_on(layout.output.crtc))
    }

    #[must_use]
    pub(crate) fn cursor_plane_fits_for_output(
        &self,
        output_idx: usize,
        width: u32,
        height: u32,
    ) -> bool {
        let Some(layout) = self.outputs.get(output_idx) else {
            return false;
        };
        let Some(device) = self.device_for_key(layout.key.device_key) else {
            return false;
        };
        device.cursor.available_on(layout.output.crtc)
            && device.cursor.plane.as_ref().is_some_and(|plane| {
                cursor_dimensions_fit(plane.width(), plane.height(), width, height)
            })
    }

    fn cursor_output_route(
        &self,
        output_idx: usize,
    ) -> io::Result<(usize, ::drm::control::crtc::Handle, i32, i32)> {
        let layout = self
            .outputs
            .get(output_idx)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such output"))?;
        let device_idx = self
            .devices
            .iter()
            .position(|device| device.key == layout.key.device_key)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "no DRM device {} for output {output_idx}",
                        layout.key.device_key
                    ),
                )
            })?;
        Ok((device_idx, layout.output.crtc, layout.x, layout.y))
    }

    fn note_unbound_cursor_failure(
        &mut self,
        device_idx: usize,
        crtc: ::drm::control::crtc::Handle,
        error: &io::Error,
    ) -> bool {
        let device_key = self.devices[device_idx].key;
        let was_permanently_disabled = self.devices[device_idx].cursor.permanently_disabled;
        let disposition = self.devices[device_idx]
            .cursor
            .note_cursor_failure_pair(crtc, error, None);
        match disposition {
            CursorFailureDisposition::Permanent => {
                if !was_permanently_disabled {
                    log::warn!(
                        "render cursor: device {device_key} permanently rejected cursor ioctls ({error}); using software cursor on that device"
                    );
                }
                true
            }
            CursorFailureDisposition::Transient => {
                log::warn!(
                    "render cursor: device {device_key} CRTC {crtc:?} rejected a temporary cursor state ({error}); rate-limited software fallback"
                );
                true
            }
            CursorFailureDisposition::Unchanged => false,
        }
    }

    /// Diagnostic/test hook: whether one particular device is permanently
    /// latched to software cursor composition.
    #[must_use]
    pub(crate) fn hw_cursor_disabled_for_device(
        &self,
        key: crate::platform::drm::DrmDeviceKey,
    ) -> bool {
        self.device_for_key(key)
            .is_some_and(|device| device.cursor.permanently_disabled)
    }

    /// A new sprite or hotspot invalidates EINVAL observations made against a
    /// previous parameter set. This is deliberately a global sprite fanout,
    /// but every device clears only its own CRTC retry records.
    pub(crate) fn cursor_plane_note_sprite_hotspot(
        &mut self,
        width: u16,
        height: u16,
        hot_x: u16,
        hot_y: u16,
    ) {
        let signature = (width, height, hot_x, hot_y);
        for device in &mut self.devices {
            if device.cursor.sprite_signature != Some(signature) {
                device.cursor.sprite_signature = Some(signature);
                device.cursor.transient_fallback_crtcs.clear();
            }
        }
    }

    pub(crate) fn cursor_plane_upload_image_for_output(
        &mut self,
        output_idx: usize,
        version: u64,
        width: u32,
        height: u32,
        bgra_bytes: &[u8],
    ) -> io::Result<()> {
        let (device_idx, crtc, _, _) = self.cursor_output_route(output_idx)?;
        let state = &mut self.devices[device_idx].cursor;
        if !state.available_on(crtc) {
            return Err(io::Error::other("cursor plane unavailable for output"));
        }
        let result = state
            .plane
            .as_mut()
            .expect("available cursor state has a plane")
            .upload_image(version, width, height, bgra_bytes);
        // Only a real upload attempt is classified. The availability
        // precheck above and scene-side version races are not driver
        // observations and must not extend this output's backoff.
        apply_cursor_operation_result(state, crtc, &result);
        result
    }

    #[must_use]
    pub(crate) fn cursor_plane_uploaded_version_for_output(
        &self,
        output_idx: usize,
    ) -> Option<u64> {
        let (device_idx, _, _, _) = self.cursor_output_route(output_idx).ok()?;
        self.devices[device_idx]
            .cursor
            .plane
            .as_ref()
            .and_then(|plane| plane.uploaded_version())
    }

    /// Bind the plane on `output_idx`'s CRTC + position at `(x, y)`
    /// in root-space (translated to CRTC-local coords here). The
    /// sole `set_cursor2(crtc, Some(dumb), …)` call site.
    ///
    /// # Errors
    /// `set_cursor2` or `move_cursor` ioctl failure; `NotFound` if
    /// `output_idx` is out of range or plane is unavailable.
    pub(crate) fn cursor_plane_show_on_crtc(
        &mut self,
        output_idx: usize,
        hot_x: u16,
        hot_y: u16,
        x: i32,
        y: i32,
    ) -> Result<(), crate::kms::cursor_plane::CursorShowError> {
        let (device_idx, crtc, layout_x, layout_y) = self
            .cursor_output_route(output_idx)
            .map_err(crate::kms::cursor_plane::CursorShowError::Unbound)?;
        let (cx, cy) = cursor_root_to_crtc_local(x, y, layout_x, layout_y, hot_x, hot_y);
        if !self.devices[device_idx].cursor.available_on(crtc) {
            return Err(crate::kms::cursor_plane::CursorShowError::Unbound(
                io::Error::other("cursor plane unavailable for output"),
            ));
        }
        let result = self.devices[device_idx]
            .cursor
            .plane
            .as_mut()
            .expect("available cursor state has a plane")
            .show(crtc, (i32::from(hot_x), i32::from(hot_y)), cx, cy);
        match result {
            Ok(()) => {
                self.devices[device_idx].cursor.note_cursor_success(crtc);
                Ok(())
            }
            Err(error) => {
                let device_key = self.devices[device_idx].key;
                if error.remains_visible() {
                    let disposition = apply_cursor_show_failure_state(
                        &mut self.devices[device_idx].cursor,
                        crtc,
                        &error,
                        (x, y, hot_x, hot_y),
                    );
                    if disposition == CursorFailureDisposition::Permanent {
                        log::warn!(
                            "render cursor: device {device_key} reported a permanent cursor failure while the prior HW binding remained visible; retaining actual HW mode until a hide succeeds"
                        );
                    } else if disposition == CursorFailureDisposition::Transient {
                        log::warn!(
                            "render cursor: device {device_key} CRTC {crtc:?} rejected a temporary cursor state while the prior HW binding remained visible; entering rate-limited cursorless fallback"
                        );
                    }
                } else {
                    self.note_unbound_cursor_failure(device_idx, crtc, error.operation_error());
                }
                Err(error)
            }
        }
    }

    /// Atomic cursor move per visible CRTC. Hidden CRTCs are
    /// skipped — the kernel naturally clips off-output coords on
    /// the visible ones, so no per-output geometry test is needed
    /// beyond the visibility filter.
    ///
    /// Returns the number of per-CRTC commits that the kernel
    /// rejected with `EBUSY` (cursor commit lost to a pending
    /// primary-plane commit on the same CRTC — the move's effect
    /// is dropped, the caller's telemetry counts it). Other
    /// errors are logged per-CRTC and not counted.
    ///
    /// # Errors
    /// `Err` only when the plane is unavailable; per-CRTC ioctl
    /// failures are logged + counted (EBUSY) or logged (other).
    pub(crate) fn cursor_plane_move(
        &mut self,
        x: i32,
        y: i32,
        hot_x: u16,
        hot_y: u16,
    ) -> io::Result<CursorMoveOutcome> {
        let mut aggregate = CursorMoveOutcome::default();
        let mut found = false;
        for device_idx in 0..self.devices.len() {
            if self.devices[device_idx].cursor.plane.is_none() {
                continue;
            }
            found = true;
            let outcome = self.try_cursor_plane_move_for_device(device_idx, x, y, hot_x, hot_y)?;
            aggregate.merge(outcome);
        }
        found
            .then_some(aggregate)
            .ok_or_else(|| io::Error::other("cursor plane unavailable"))
    }

    /// Retry the most recent pending cursor move, if any. Called from
    /// the backend's page-flip-complete handler — the just-retired
    /// flip means the primary atomic-commit queue freed up for this
    /// CRTC, so the cursor commit that lost the race a few ms ago has
    /// a fresh window to land. Latest-wins: only the most recent
    /// position is retried, intermediate motions are discarded.
    ///
    /// Returns the EBUSY count from this retry (typically 0 if the
    /// commit landed; >0 means the cursor commit raced another
    /// pending primary commit and stays queued for the next page-flip
    /// retire).
    ///
    /// # Errors
    /// `Err` only when the plane is unavailable.
    pub(crate) fn cursor_plane_drain_pending_move_for_output(
        &mut self,
        output_idx: usize,
    ) -> io::Result<CursorMoveOutcome> {
        let (device_idx, _, _, _) = self.cursor_output_route(output_idx)?;
        let Some((x, y, hot_x, hot_y)) = self.devices[device_idx].cursor.pending_move else {
            return Ok(CursorMoveOutcome::default());
        };
        self.try_cursor_plane_move_for_device(device_idx, x, y, hot_x, hot_y)
    }

    /// Internal helper: per-CRTC `move_to` iteration that returns the
    /// number of CRTCs whose atomic commit returned `EBUSY`. Shared by
    /// `cursor_plane_move` (first-attempt path) and
    /// `cursor_plane_drain_pending_move` (retry path).
    fn try_cursor_plane_move_for_device(
        &mut self,
        device_idx: usize,
        x: i32,
        y: i32,
        hot_x: u16,
        hot_y: u16,
    ) -> io::Result<CursorMoveOutcome> {
        let device_key = self.devices[device_idx].key;
        let layouts: Vec<(::drm::control::crtc::Handle, i32, i32)> = self
            .outputs
            .iter()
            .filter(|layout| layout.key.device_key == device_key)
            .map(|l| (l.output.crtc, l.x, l.y))
            .collect();
        let state = &mut self.devices[device_idx].cursor;
        if state.plane.is_none() {
            return Err(io::Error::other("cursor plane unavailable"));
        }
        let mut outcome = CursorMoveOutcome::default();
        let mut keep_pending = false;
        for (crtc, layout_x, layout_y) in layouts {
            if !state
                .plane
                .as_ref()
                .is_some_and(|plane| plane.is_visible_on(crtc))
            {
                continue;
            }
            let (cx, cy) = cursor_root_to_crtc_local(x, y, layout_x, layout_y, hot_x, hot_y);
            let move_result = state
                .plane
                .as_ref()
                .expect("checked cursor plane")
                .move_to(crtc, cx, cy);
            if let Err(e) = move_result {
                if e.raw_os_error() == Some(libc::EBUSY) {
                    outcome.ebusy_count = outcome.ebusy_count.saturating_add(1);
                    keep_pending = true;
                } else if e.raw_os_error() == Some(libc::EINVAL) || cursor_err_disables_hw(&e) {
                    // A move failure leaves the old HW sprite visible. Only
                    // enter SW fallback after a successful hide rollback.
                    let hide_result = state
                        .plane
                        .as_mut()
                        .expect("checked cursor plane")
                        .hide(crtc);
                    let rollback_error = hide_result.as_ref().err().map(ToString::to_string);
                    keep_pending |= apply_cursor_move_rollback_result(
                        state,
                        crtc,
                        &e,
                        hide_result,
                        &mut outcome,
                    );
                    if let Some(rollback_error) = rollback_error {
                        log::warn!(
                            "render cursor move: device {device_key} CRTC {crtc:?} failed ({e}); hide rollback also failed ({rollback_error}), retaining HW ownership"
                        );
                    }
                } else {
                    log::warn!("render cursor move: device {device_key} CRTC {crtc:?} failed: {e}");
                }
            }
        }
        state.pending_move =
            (keep_pending || outcome.ebusy_count > 0).then_some((x, y, hot_x, hot_y));
        Ok(outcome)
    }

    /// True iff the set of CRTCs whose region the cursor footprint
    /// intersects differs from the set the plane is currently bound on
    /// (`is_visible_on`).
    ///
    /// The pointer fast path (`cursor_plane_move`) only *repositions*
    /// the cursor on already-bound CRTCs — it never shows the plane on
    /// a CRTC the pointer newly crosses into, nor hides it on one it
    /// leaves. Cross-CRTC show/hide is decided by the scene's
    /// `CursorAssignment` during compose. While an idle desktop
    /// composited every frame (pre-#30) that reassignment happened for
    /// free; now that idle desktops stop compositing, the fast path
    /// must detect a boundary crossing and route it through one compose
    /// tick. This predicate is that detector, using the same footprint
    /// intersection rule as `cursor_footprint_rect` so its membership
    /// decision matches the scene's exactly.
    ///
    /// `(x, y)` is the root-space cursor position, `(hot_x, hot_y)` the
    /// sprite hotspot, `(cw, ch)` the sprite extent.
    pub(crate) fn cursor_crtc_membership_dirty(
        &self,
        x: i32,
        y: i32,
        hot_x: u16,
        hot_y: u16,
        cw: i32,
        ch: i32,
    ) -> bool {
        for l in &self.outputs {
            let Some(device) = self.device_for_key(l.key.device_key) else {
                continue;
            };
            let Some(plane) = device.cursor.plane.as_ref() else {
                continue;
            };
            let dx = x - i32::from(hot_x) - l.x;
            let dy = y - i32::from(hot_y) - l.y;
            let intersects = cursor_footprint_intersects_output(
                dx,
                dy,
                cw,
                ch,
                i32::from(l.width),
                i32::from(l.height),
            );
            if intersects != plane.is_visible_on(l.output.crtc) {
                return true;
            }
        }
        false
    }

    /// Detach the plane on a single CRTC. Output-local recovery
    /// (Phase D') uses this; the per-CRTC visibility map updates
    /// so subsequent rebind / move calls skip the CRTC cleanly.
    ///
    /// # Errors
    /// `NotFound` if `output_idx` is out of range or plane is
    /// unavailable; `set_cursor2` ioctl failure otherwise.
    pub(crate) fn cursor_plane_hide_on_crtc(&mut self, output_idx: usize) -> io::Result<()> {
        let (device_idx, crtc, _, _) = self.cursor_output_route(output_idx)?;
        let result = {
            let Some(plane) = self.devices[device_idx].cursor.plane.as_mut() else {
                return Err(io::Error::other("cursor plane unavailable"));
            };
            plane.hide(crtc)
        };
        apply_cursor_operation_result(&mut self.devices[device_idx].cursor, crtc, &result);
        result
    }

    /// Kernel-side visibility for this output's device-qualified CRTC. Scene
    /// bookkeeping can temporarily lag after an ioctl/rollback failure, so a
    /// compose tick must consult this before deciding it is safe to draw SW.
    #[must_use]
    pub(crate) fn cursor_plane_visible_for_output(&self, output_idx: usize) -> bool {
        let Ok((device_idx, crtc, _, _)) = self.cursor_output_route(output_idx) else {
            return false;
        };
        self.devices[device_idx]
            .cursor
            .plane
            .as_ref()
            .is_some_and(|plane| plane.is_visible_on(crtc))
    }

    /// Advance only this output's EINVAL retry budget after its own successful
    /// software-composed retirement. Returns true when another repaint is
    /// needed either to consume more backoff or to perform the now-eligible HW
    /// retry. A retirement on another card cannot touch this record.
    pub(crate) fn cursor_plane_note_composed_retirement(&mut self, output_idx: usize) -> bool {
        let Ok((device_idx, crtc, _, _)) = self.cursor_output_route(output_idx) else {
            return false;
        };
        let Some(retry) = self.devices[device_idx]
            .cursor
            .transient_fallback_crtcs
            .get_mut(&crtc)
        else {
            return false;
        };
        if retry.remaining_sw_retires > 0 {
            retry.remaining_sw_retires -= 1;
            return true;
        }
        false
    }

    /// Revalidate every existing per-device cursor plane after an active CRTC
    /// topology change. A genuinely headless-deferred device is deliberately
    /// skipped here: only the post-success explicit-enable hook may make its
    /// first attempt. A prior transient attempt is retried at this later
    /// topology/resume boundary.
    fn refresh_cursor_topology_for_devices(
        &mut self,
        changed_devices: &HashSet<crate::platform::drm::DrmDeviceKey>,
    ) {
        self.refresh_cursor_topology_for_devices_with(
            changed_devices,
            initialize_cursor_plane_for_device,
        );
    }

    fn refresh_cursor_topology_for_devices_with<F>(
        &mut self,
        changed_devices: &HashSet<crate::platform::drm::DrmDeviceKey>,
        mut factory: F,
    ) where
        F: FnMut(&mut KmsDevice, &[::drm::control::crtc::Handle], &str),
    {
        let mut crtcs_by_device: HashMap<_, Vec<_>> = HashMap::new();
        for output in &self.outputs {
            crtcs_by_device
                .entry(output.key.device_key)
                .or_default()
                .push(output.output.crtc);
        }
        for device in &mut self.devices {
            if !changed_devices.contains(&device.key) {
                continue;
            }
            device.cursor.pending_move = None;
            device.cursor.transient_fallback_crtcs.clear();
            let crtcs = crtcs_by_device.remove(&device.key).unwrap_or_default();
            if device.cursor.should_retry_initialization(!crtcs.is_empty()) {
                factory(device, &crtcs, "lifecycle retry");
            }
            if let Some(plane) = device.cursor.plane.as_mut() {
                plane.retain_crtcs(&crtcs.iter().copied().collect());
            }
            let blocked = device
                .cursor
                .plane
                .as_ref()
                .is_some_and(|plane| !plane.supports_crtcs(&crtcs));
            if blocked != device.cursor.topology_blocked {
                log::warn!(
                    "render cursor: device {} topology_blocked {} -> {} for {} active CRTC(s)",
                    device.key,
                    device.cursor.topology_blocked,
                    blocked,
                    crtcs.len()
                );
            }
            device.cursor.topology_blocked = blocked;
        }
    }

    /// Run the one-time first-output factory for an opened device that began
    /// genuinely headless. Callers must invoke this only after the successful
    /// ActiveOutput insertion. All post-insertion active CRTCs on the owning
    /// device are passed to the factory, and no other card is touched.
    fn initialize_headless_cursor_for_device_with<F>(
        &mut self,
        device_key: crate::platform::drm::DrmDeviceKey,
        boundary: &str,
        factory: F,
    ) -> bool
    where
        F: FnOnce(&mut KmsDevice, &[::drm::control::crtc::Handle], &str),
    {
        let crtcs: Vec<_> = self
            .outputs
            .iter()
            .filter(|output| output.key.device_key == device_key)
            .map(|output| output.output.crtc)
            .collect();
        let Some(device) = self
            .devices
            .iter_mut()
            .find(|device| device.key == device_key)
        else {
            return false;
        };
        if !device
            .cursor
            .should_initialize_headless_deferred(!crtcs.is_empty())
        {
            return false;
        }

        // Consume genuine-deferred before invoking the injectable factory so
        // even a test/factory panic cannot make ordinary probes look like a
        // never-attempted device. The production factory records success or
        // retryable/permanent failure in the remaining state.
        device.cursor.headless_deferred = false;
        factory(device, &crtcs, boundary);
        true
    }

    pub(crate) fn refresh_cursor_topology(&mut self) {
        let all_devices: HashSet<_> = self.devices.iter().map(|device| device.key).collect();
        self.refresh_cursor_topology_for_devices(&all_devices);
    }

    /// Detach the plane on every CRTC the plane has ever been bound
    /// against AND every currently-known output. Global recovery
    /// fallback only — `drain_all`, shutdown, VT-leave, DRM-master
    /// loss. Per Phase D' this also invalidates `uploaded_version`
    /// so the next acquire/modeset re-uploads cleanly.
    ///
    /// # Errors
    /// Per-CRTC failures are logged; this never returns `Err`
    /// unless the plane is unavailable.
    pub(crate) fn cursor_plane_hide_all(&mut self) -> io::Result<()> {
        let outputs: Vec<_> = self
            .outputs
            .iter()
            .map(|layout| (layout.key.device_key, layout.output.crtc))
            .collect();
        let mut found = false;
        for device in &mut self.devices {
            device.cursor.pending_move = None;
            let Some(plane) = device.cursor.plane.as_mut() else {
                continue;
            };
            found = true;
            let mut crtcs: Vec<_> = outputs
                .iter()
                .filter(|(key, _)| *key == device.key)
                .map(|(_, crtc)| *crtc)
                .collect();
            for crtc in plane.known_crtcs() {
                if !crtcs.contains(&crtc) {
                    crtcs.push(crtc);
                }
            }
            for crtc in crtcs {
                if let Err(error) = plane.hide(crtc) {
                    if error.kind() == io::ErrorKind::PermissionDenied {
                        log::debug!(
                            "render cursor hide_all: device {} CRTC {crtc:?} (no master): {error}",
                            device.key
                        );
                    } else {
                        log::warn!(
                            "render cursor hide_all: device {} CRTC {crtc:?} failed: {error}",
                            device.key
                        );
                    }
                }
            }
            plane.invalidate_uploaded_version();
        }
        if found {
            Ok(())
        } else {
            Err(io::Error::other("cursor plane unavailable"))
        }
    }

    pub(crate) fn take_input_ctx(&mut self) -> Option<crate::input::SendContext> {
        self.input_ctx.take()
    }

    pub(crate) fn primary_device(&self) -> Option<&KmsDevice> {
        self.devices.first()
    }

    pub(crate) fn selected_render_device(&self) -> Option<&RenderDevice> {
        let selected = self.selected_render_device?;
        self.render_devices
            .iter()
            .find(|device| device.id == selected)
    }

    #[cfg(test)]
    pub(crate) fn selected_render_device_mut(&mut self) -> Option<&mut RenderDevice> {
        let selected = self.selected_render_device?;
        self.render_devices
            .iter_mut()
            .find(|device| device.id == selected)
    }

    pub(crate) fn device_for_key(
        &self,
        key: crate::platform::drm::DrmDeviceKey,
    ) -> Option<&KmsDevice> {
        self.devices.iter().find(|device| device.key == key)
    }

    pub(crate) fn device_for_output(&self, key: &OutputKey) -> Option<&KmsDevice> {
        self.device_for_key(key.device_key)
    }

    /// Gather lightweight connector probes for every opened DRM device.
    /// Results are accumulated before callers mutate RANDR state, so a
    /// failure on card N cannot leave a half-reconciled combined snapshot.
    pub(crate) fn probe_all_connectors(
        &self,
    ) -> io::Result<
        Vec<(
            crate::platform::drm::DrmDeviceKey,
            Vec<crate::platform::drm::ConnectorProbe>,
        )>,
    > {
        let mut all = Vec::with_capacity(self.devices.len());
        for device in &self.devices {
            let probes =
                crate::platform::drm::probe_connectors(&device.device).map_err(|error| {
                    io::Error::new(
                        error.kind(),
                        format!("probe connectors on DRM device {}: {error}", device.key),
                    )
                })?;
            all.push((device.key, probes));
        }
        Ok(all)
    }

    /// Gather the complete connected-connector snapshot without mutating any
    /// live output, pool, or RANDR-facing state. The backend can therefore
    /// quiesce GPU/page-flip/direct work only after every device probe has
    /// succeeded, and before applying removals.
    pub(crate) fn probe_connector_snapshot(&self) -> io::Result<Vec<ConnectorSnapshot>> {
        let mut snapshot = Vec::new();
        for device in &self.devices {
            let probes = crate::platform::drm::probe_connector_snapshots(&device.device).map_err(
                |error| {
                    io::Error::new(
                        error.kind(),
                        format!(
                            "probe connector snapshot on DRM device {}: {error}",
                            device.key
                        ),
                    )
                },
            )?;
            snapshot.extend(
                probes
                    .iter()
                    .map(|probe| ConnectorSnapshot::from_probe(device.key, probe)),
            );
        }
        Ok(snapshot)
    }

    pub(crate) fn output_index_for_crtc(&self, crtc_key: CrtcKey) -> Option<usize> {
        self.outputs.iter().position(|output| {
            output.key.device_key == crtc_key.device_key && output.output.crtc == crtc_key.crtc
        })
    }

    fn prune_present_clocks_to_live_outputs(&mut self) {
        let live: HashSet<CrtcKey> = self.outputs.iter().map(CrtcKey::for_output).collect();
        self.ust_msc.retain(|key, _| live.contains(key));
        self.completion_clocks.retain(|key, _| live.contains(key));
        self.software_msc.retain(|key, _| live.contains(key));
    }

    pub(crate) fn poll_fds(&self) -> Vec<(RawFd, BackendFdKind)> {
        let mut fds = Vec::with_capacity(3 + self.devices.len());
        if let Some(ctx) = self.input_ctx.as_ref() {
            fds.push((ctx.fd(), BackendFdKind::Libinput));
        }
        for device in &self.devices {
            fds.push((device.device.as_fd().as_raw_fd(), BackendFdKind::Drm));
        }
        #[cfg(target_os = "linux")]
        if let Some(mon) = self.hotplug_monitor.as_ref() {
            fds.push((mon.raw_fd(), BackendFdKind::DrmHotplug));
        }
        // Stage 5 Task 6.1: stable inner epfd for deferred PRESENT
        // completion. Always present.
        fds.push((
            self.present_completion_epfd.as_raw_fd(),
            BackendFdKind::PresentCompletion,
        ));
        fds
    }

    fn drm_device_index_for_fd(&self, drm_fd: RawFd) -> Option<usize> {
        self.devices
            .iter()
            .position(|device| device.device.as_fd().as_raw_fd() == drm_fd)
    }

    /// Drain page-flip events that belong to the topology epoch just taken
    /// fully off-screen. A blocking ALLOW_MODESET waits prior flips, but the
    /// kernel can signal that wait just before it links the corresponding
    /// event onto the DRM fd. Wait boundedly for every CRTC known to have had
    /// a pending flip, then drain any already-ready tail. Sequence events are
    /// intentionally discarded too: all vblank arm bookkeeping was cleared
    /// when the CRTCs were disabled.
    pub(crate) fn discard_old_drm_events_after_all_off(
        &self,
        expected_pageflips: &HashSet<CrtcKey>,
        timeout: std::time::Duration,
    ) -> io::Result<()> {
        let mut expected = expected_pageflips.clone();
        let deadline = std::time::Instant::now() + timeout;

        loop {
            let wait_ms = if expected.is_empty() {
                0
            } else {
                let now = std::time::Instant::now();
                if now >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!(
                            "timed out waiting for {} old DRM page-flip event(s): {expected:?}",
                            expected.len()
                        ),
                    ));
                }
                i32::try_from((deadline - now).as_millis().max(1)).unwrap_or(i32::MAX)
            };
            let mut poll_fds: Vec<libc::pollfd> = self
                .devices
                .iter()
                .map(|device| libc::pollfd {
                    fd: device.device.as_fd().as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                })
                .collect();
            if poll_fds.is_empty() {
                return if expected.is_empty() {
                    Ok(())
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "old page flips remained but no DRM device is open",
                    ))
                };
            }
            let nfds = libc::nfds_t::try_from(poll_fds.len())
                .map_err(|_| io::Error::other("too many DRM fds to poll"))?;
            // SAFETY: `poll_fds` is a live contiguous array of `nfds`
            // initialized pollfd records for the duration of this call.
            let ready = unsafe { libc::poll(poll_fds.as_mut_ptr(), nfds, wait_ms) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if ready == 0 {
                if expected.is_empty() {
                    return Ok(());
                }
                continue;
            }

            for (device, poll_fd) in self.devices.iter().zip(&poll_fds) {
                let error_events = libc::POLLERR | libc::POLLHUP | libc::POLLNVAL;
                if poll_fd.revents & error_events != 0 {
                    return Err(io::Error::other(format!(
                        "DRM fd {} reported poll error flags 0x{:x} while draining old events",
                        poll_fd.fd, poll_fd.revents
                    )));
                }
                if poll_fd.revents & libc::POLLIN == 0 {
                    continue;
                }
                let device_key = device.key;
                crate::drm::page_flip::drain_events(
                    &device.device,
                    |crtc, _frame, _duration| {
                        expected.remove(&CrtcKey::new(device_key, crtc));
                    },
                    |_user_data, _time_ns, _sequence| {},
                )?;
            }
        }
    }

    pub(crate) fn drain_page_flip_events(
        &mut self,
        drm_fd: RawFd,
    ) -> io::Result<DrainedPageFlipEvents> {
        use ::drm::control::crtc;

        let device_index = self.drm_device_index_for_fd(drm_fd).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("page-flip readiness from unknown DRM fd {drm_fd}"),
            )
        })?;
        let device_key = self.devices[device_index].key;
        let device = Rc::clone(&self.devices[device_index].device);

        // Capture the kernel vblank (msc=frame, ust=duration) alongside the
        // CRTC so Present pacing can complete NotifyMSC with real values.
        let mut flipped: Vec<(crtc::Handle, u32, std::time::Duration)> = Vec::new();
        let mut sequenced: Vec<SequenceCompletion> = Vec::new();
        crate::drm::page_flip::drain_events(
            &device,
            |c, frame, dur| {
                flipped.push((c, frame, dur));
            },
            |user_data, time_ns, sequence| {
                // Raw kernel values; validation (time_ns sign, crtc_id
                // resolution) and tag decode happen in
                // `on_crtc_sequence_event`.
                sequenced.push(SequenceCompletion {
                    device_key,
                    user_data,
                    time_ns,
                    sequence,
                });
            },
        )?;

        let mut completions = Vec::with_capacity(flipped.len());
        for (crtc, frame, dur) in flipped {
            let crtc_key = CrtcKey::new(device_key, crtc);
            let Some(output_idx) = self.output_index_for_crtc(crtc_key) else {
                log::warn!("render: pageflip-complete for unknown CRTC {crtc:?} on {device_key}");
                continue;
            };
            // u32 frame → u64 MSC (kernel wraps at 2^32; monotonic enough
            // for a frame clock within a session). UST in microseconds.
            let ust = u64::try_from(dur.as_micros()).unwrap_or(u64::MAX);
            // apple_drm (Asahi) reports `frame == 0` on every page-flip
            // completion — the kernel does not maintain a CRTC sequence
            // counter — and rejects `DRM_IOCTL_CRTC_QUEUE_SEQUENCE` with
            // `EOPNOTSUPP`, so the idle-vblank arming path can't advance
            // the clock either. Without a non-zero MSC the Present
            // NotifyMSC path deadlocks (picom presents frame 0 then blocks
            // forever). Fall back to a per-output software counter that
            // increments on every flip when the kernel reports 0; on
            // drivers that report a real frame this stays untouched.
            let msc = if frame == 0 {
                let next = self
                    .software_msc
                    .get(&crtc_key)
                    .copied()
                    .unwrap_or(0)
                    .saturating_add(1);
                self.software_msc.insert(crtc_key, next);
                log::debug!(
                    target: "yserver::kms::render::platform",
                    "render pageflip software-msc fallback output={output_idx} msc={next} \
                     ust={ust} (kernel reports frame=0)"
                );
                next
            } else {
                u64::from(frame)
            };
            log::debug!(
                target: "yserver::kms::render::platform",
                "render pageflip ust_msc output={output_idx} msc={msc} kernel_frame={frame} kernel_ust_micros={ust}"
            );
            self.record_vblank_clock(crtc_key, msc, ust);
            let sample = PresentClockSample {
                msc,
                ust,
                source: PresentClockSource::PageFlip,
            };
            self.record_completion_clock(crtc_key, sample);
            log::debug!(
                target: "present_pace",
                "present_clock sample source=pageflip output={output_idx} msc={msc} ust={ust}"
            );
            completions.push((output_idx, sample));
        }
        Ok((completions, sequenced))
    }

    /// Latest kernel `(msc, ust_micros)` for one device-qualified CRTC, or
    /// `(0, 0)` before that display domain has produced a pageflip/sequence
    /// event. Samples from other cards or CRTCs must never influence this
    /// result: their MSC counters are unrelated even when raw handles match.
    pub(crate) fn present_get_ust_msc(&self, crtc_key: CrtcKey) -> (u64, u64) {
        self.ust_msc.get(&crtc_key).copied().unwrap_or((0, 0))
    }

    /// Latest completion-eligible clock for one device-qualified CRTC.
    pub(crate) fn present_get_completion_clock(&self, crtc_key: CrtcKey) -> PresentClockSample {
        self.completion_clocks
            .get(&crtc_key)
            .copied()
            .unwrap_or(PresentClockSample {
                msc: 0,
                ust: 0,
                source: PresentClockSource::PageFlip,
            })
    }

    /// Record a general vblank sample without allowing late events to move
    /// this CRTC domain's Present clock backwards.
    pub(crate) fn record_vblank_clock(&mut self, crtc_key: CrtcKey, msc: u64, ust: u64) {
        let replace = self.ust_msc.get(&crtc_key).is_none_or(|(old_msc, _)| {
            msc == *old_msc || yserver_core::present_scheduler::msc_is_after(msc, *old_msc)
        });
        if replace {
            self.ust_msc.insert(crtc_key, (msc, ust));
        }
    }

    /// Record a completion-eligible clock sample. At equal MSC, prefer a
    /// pageflip sample over an idle-sequence sample so provenance reflects
    /// the stronger event if both arrive for the same field.
    pub(crate) fn record_completion_clock(
        &mut self,
        crtc_key: CrtcKey,
        sample: PresentClockSample,
    ) {
        let replace = self.completion_clocks.get(&crtc_key).is_none_or(|old| {
            yserver_core::present_scheduler::msc_is_after(sample.msc, old.msc)
                || (sample.msc == old.msc
                    && (old.source != PresentClockSource::PageFlip
                        || sample.source == PresentClockSource::PageFlip))
        });
        if replace {
            self.completion_clocks.insert(crtc_key, sample);
        }
    }

    /// VkContext accessor for the engine. Returns `None` on the
    /// test fixture (`for_tests`) where Vk init is skipped.
    pub(crate) fn vk(&self) -> Option<&Arc<VkContext>> {
        self.vk.as_ref()
    }

    /// `OpsCommandPool` handle for the engine. `None` on the test
    /// fixture. Engine allocates per-op CBs from this pool.
    pub(crate) fn ops_command_pool_handle(&self) -> Option<vk::CommandPool> {
        self.ops_command_pool.as_ref().map(OpsCommandPool::handle)
    }

    // ── Storage allocation (Stage 2c) ───────────────────────────

    /// Sample-side view swizzle for a (format, depth) pair. The
    /// attachment-side view kept by `Storage::image_view` always
    /// uses IDENTITY (VUID-VkFramebufferCreateInfo-pAttachments-00891
    /// requires that for color attachments). The sample-side view
    /// kept by `Storage::sample_view` carries the format-aware
    /// swizzle so the scene compositor + engine sampling paths see
    /// X11-correct alpha semantics:
    ///
    /// - `(R8_UNORM, _)` → `a=R, rgb=ZERO` — R8 storage sampled as
    ///   an alpha mask (glyphs, RENDER mask scratch, depth-1 / 8
    ///   bitmaps). RGB channels intentionally zeroed so the
    ///   composite shader's `src * coverage` reads zero RGB and
    ///   the dst keeps its own colour.
    /// - `(B8G8R8A8_UNORM, depth == 24)` → `a=ONE` — depth-24
    ///   pixmaps (`PictFormat.alpha_mask = 0` per X11 RENDER spec)
    ///   must read α = 1.0 regardless of the BGRA8 padding byte.
    ///   Otherwise the scene's `alpha_passthrough=true` window
    ///   draws blend with undefined α and the layer below leaks
    ///   through.
    /// - everything else → IDENTITY (depth-32 ARGB passes α
    ///   through; unknown formats default-safe).
    ///
    /// Mirrors `engine::swizzle_class_for` (the engine's RENDER
    /// view-cache classifier) — the engine cache stays for the
    /// cases where the sampler config also differs; this helper
    /// owns the storage-side view that the scene compositor
    /// binds directly.
    pub(crate) fn sample_view_components(format: vk::Format, depth: u8) -> vk::ComponentMapping {
        match (format, depth) {
            (vk::Format::R8_UNORM, _) => vk::ComponentMapping {
                r: vk::ComponentSwizzle::ZERO,
                g: vk::ComponentSwizzle::ZERO,
                b: vk::ComponentSwizzle::ZERO,
                a: vk::ComponentSwizzle::R,
            },
            (vk::Format::B8G8R8A8_UNORM, 24) => vk::ComponentMapping {
                r: vk::ComponentSwizzle::IDENTITY,
                g: vk::ComponentSwizzle::IDENTITY,
                b: vk::ComponentSwizzle::IDENTITY,
                a: vk::ComponentSwizzle::ONE,
            },
            _ => vk::ComponentMapping {
                r: vk::ComponentSwizzle::IDENTITY,
                g: vk::ComponentSwizzle::IDENTITY,
                b: vk::ComponentSwizzle::IDENTITY,
                a: vk::ComponentSwizzle::IDENTITY,
            },
        }
    }

    /// Build a fresh sample-side `vk::ImageView` over `image` with
    /// the format/depth-aware swizzle from
    /// [`Self::sample_view_components`]. Used by the fresh-alloc
    /// path, the pool-take path (where the pool only stores the
    /// attachment view), and the DRI3 import path (where the
    /// imported DrawableImage carries an identity-swizzle view we
    /// can't reuse for scene sampling).
    pub(crate) fn build_sample_view(
        vk: &crate::kms::vk::device::VkContext,
        image: vk::Image,
        format: vk::Format,
        depth: u8,
    ) -> Result<vk::ImageView, vk::Result> {
        let info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format)
            .components(Self::sample_view_components(format, depth))
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            );
        unsafe { vk.device.create_image_view(&info, None) }
    }

    /// Build a fresh attachment-side `vk::ImageView` over `image` with
    /// an IDENTITY component swizzle. This matches what
    /// [`Self::allocate_drawable_storage`]'s fresh-alloc path builds for
    /// `Storage::image_view` (the colour-attachment view —
    /// VUID-VkFramebufferCreateInfo-pAttachments-00891 requires IDENTITY
    /// for attachment views). Used by the GLX-TFP promotion path
    /// (`RenderEngine::promote_drawable_exportable`) to rebuild the
    /// attachment view over the newly-adopted exportable image.
    pub(crate) fn build_attachment_view(
        vk: &crate::kms::vk::device::VkContext,
        image: vk::Image,
        format: vk::Format,
    ) -> Result<vk::ImageView, vk::Result> {
        let info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            );
        unsafe { vk.device.create_image_view(&info, None) }
    }

    /// Map an X11 drawable depth to its v2 storage format. Mirrors
    /// `DrawableImage::format_for_pixmap_depth` (v1) so the two
    /// don't drift.
    #[must_use]
    pub(crate) fn format_for_depth(depth: u8) -> vk::Format {
        match depth {
            1 | 4 | 8 => vk::Format::R8_UNORM,
            24 | 32 => vk::Format::B8G8R8A8_UNORM,
            other => {
                log::warn!(
                    "render PlatformBackend::format_for_depth: unhandled depth {other} → \
                     defaulting to B8G8R8A8_UNORM",
                );
                vk::Format::B8G8R8A8_UNORM
            }
        }
    }

    /// Allocate a fresh server-owned [`Storage`] for the
    /// [`DrawableStore`]. DEVICE_LOCAL memory; tiling=OPTIMAL;
    /// usage covers Stage 2c (TRANSFER_SRC/DST, COLOR_ATTACHMENT,
    /// SAMPLED). Initial layout = `UNDEFINED`.
    ///
    /// # Errors
    ///
    /// Returns `ERROR_INITIALIZATION_FAILED` if Vk is not
    /// available (test fixture). Propagates `vkCreateImage` /
    /// `vkAllocateMemory` / `vkBindImageMemory` /
    /// `vkCreateImageView` failures.
    pub(crate) fn allocate_drawable_storage(
        &self,
        width: u16,
        height: u16,
        depth: u8,
    ) -> Result<Storage, vk::Result> {
        let vk = self
            .vk
            .as_ref()
            .ok_or(vk::Result::ERROR_INITIALIZATION_FAILED)?;
        let format = Self::format_for_depth(depth);
        let extent = vk::Extent2D {
            width: u32::from(width.max(1)),
            height: u32::from(height.max(1)),
        };

        // Stage 3f.10: try the recycle pool before falling through to
        // a fresh Vk allocate. v1's pool keys on
        // (width, height, format); the usage flag set is constant
        // across all server-owned pixmaps (matches v1).
        if let Some(pool) = self.pixmap_pool.as_ref() {
            let key = crate::kms::vk::pixmap_pool::PixmapPoolKey {
                width: extent.width,
                height: extent.height,
                format,
            };
            if let Some(pooled) = pool.try_take(key) {
                // The pool stores only the attachment-side
                // (IDENTITY) view; the sample-side view is
                // depth-specific (a recycled depth-32 BGRA8
                // image can serve a fresh depth-24 request and
                // vice versa, since the pool key is format only),
                // so always build a fresh sample_view for the
                // current request's depth. View creation is cheap;
                // pooling the image + memory is where the win is.
                let pooled_image = pooled.image;
                let sample_view = match Self::build_sample_view(vk, pooled_image, format, depth) {
                    Ok(v) => v,
                    Err(e) => {
                        // Couldn't build a sample_view: return the
                        // pooled triple back to the pool and fall
                        // through to fresh allocate (which also
                        // tries to build a sample_view and may also
                        // fail — but the diagnostic path is
                        // uniform that way).
                        let _ = pool.try_return(key, pooled);
                        return Err(e);
                    }
                };
                return Ok(Storage::from_pooled(
                    pooled,
                    sample_view,
                    extent,
                    format,
                    depth,
                ));
            }
        }

        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width: extent.width,
                height: extent.height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(
                vk::ImageUsageFlags::COLOR_ATTACHMENT
                    | vk::ImageUsageFlags::TRANSFER_DST
                    | vk::ImageUsageFlags::TRANSFER_SRC
                    | vk::ImageUsageFlags::SAMPLED,
            )
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image = unsafe { vk.device.create_image(&image_info, None)? };

        let mem_reqs = unsafe { vk.device.get_image_memory_requirements(image) };
        let mem_props = unsafe {
            vk.instance
                .get_physical_device_memory_properties(vk.physical_device)
        };
        let memory_type_index = (0..mem_props.memory_type_count).find(|&i| {
            mem_reqs.memory_type_bits & (1 << i) != 0
                && mem_props.memory_types[i as usize]
                    .property_flags
                    .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
        });
        let Some(mt) = memory_type_index else {
            unsafe { vk.device.destroy_image(image, None) };
            return Err(vk::Result::ERROR_FEATURE_NOT_PRESENT);
        };

        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_reqs.size)
            .memory_type_index(mt);
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

        let view_info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format)
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
                    vk.device.free_memory(memory, None);
                    vk.device.destroy_image(image, None);
                }
                return Err(e);
            }
        };

        // Sample-side view with format/depth-aware swizzle. The
        // scene compositor and the engine view-cache fall back to
        // this view for sampling instead of `view` (IDENTITY) so
        // depth-24 BGRA8 storage reads α=ONE per X11 PictFormat
        // semantics. Built unconditionally — for depth-32 the
        // swizzle is identity, but a distinct VkImageView keeps
        // Storage's ownership story uniform.
        let sample_view = match Self::build_sample_view(vk, image, format, depth) {
            Ok(v) => v,
            Err(e) => {
                unsafe {
                    vk.device.destroy_image_view(view, None);
                    vk.device.free_memory(memory, None);
                    vk.device.destroy_image(image, None);
                }
                return Err(e);
            }
        };

        Ok(Storage::new_server_owned(
            image,
            memory,
            view,
            sample_view,
            extent,
            format,
            depth,
        ))
    }

    /// Phase A: append a paint CB to the open submit group. Returns
    /// `Ok(())` once the append is recorded. NEVER auto-flushes —
    /// flush is the engine's responsibility.
    ///
    /// `signal_fence` is IGNORED — the group's shared ticket owns the
    /// fence. The parameter stays in the signature for source
    /// compatibility with the engine; remove in Phase B.
    pub(crate) fn submit_paint_cb(
        &mut self,
        cb: vk::CommandBuffer,
        _signal_fence: vk::Fence,
    ) -> Result<(), vk::Result> {
        self.submit_paint_cb_with_semaphore(cb, vk::Fence::null(), None)
    }

    /// Phase A: append a paint CB to the open submit group, optionally
    /// attaching a completion semaphore that will be signaled in the
    /// eventual group flush. NEVER auto-flushes — flush is the
    /// engine's responsibility.
    ///
    /// `signal_fence` is IGNORED — the group's shared ticket owns the
    /// fence. The parameter stays in the signature for source
    /// compatibility with the engine; remove in Phase B.
    pub(crate) fn submit_paint_cb_with_semaphore(
        &mut self,
        cb: vk::CommandBuffer,
        _signal_fence: vk::Fence,
        completion_signal: Option<vk::Semaphore>,
    ) -> Result<(), vk::Result> {
        if self.vk.is_none() {
            return Err(vk::Result::ERROR_INITIALIZATION_FAILED);
        }
        self.submit_group.append(cb, completion_signal);
        Ok(())
    }

    pub(crate) fn acquire_present_completion_signal(
        &self,
    ) -> Result<PresentCompletionSignal, vk::Result> {
        let vk = self
            .vk
            .as_ref()
            .ok_or(vk::Result::ERROR_INITIALIZATION_FAILED)?;
        create_present_completion_signal(Arc::clone(vk))
    }

    /// Submit no command buffers, only signal `completion_signal` and
    /// `signal_fence`.
    /// Same-queue ordering makes this signal happen after all prior
    /// copy/render submits, which is sufficient for the non-COW
    /// PRESENT fallback where the copy already submitted before the
    /// completion was enqueued.
    pub(crate) fn submit_present_completion_signal(
        &mut self,
        completion_signal: &PresentCompletionSignal,
        signal_fence: vk::Fence,
    ) -> Result<(), vk::Result> {
        let Some(vk) = self.vk.as_ref() else {
            return Err(vk::Result::ERROR_INITIALIZATION_FAILED);
        };
        let sig_info = [vk::SemaphoreSubmitInfo::default()
            .semaphore(completion_signal.semaphore())
            .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)];
        let submit = [vk::SubmitInfo2::default().signal_semaphore_infos(&sig_info)];
        crate::vk_count!(queue_submit2);
        match unsafe {
            vk.device
                .queue_submit2(vk.graphics_queue, &submit, signal_fence)
        } {
            Ok(()) => Ok(()),
            Err(e) => {
                self.renderer_failed = true;
                Err(e)
            }
        }
    }

    // ── Phase A: SubmitGroup API ─────────────────────────────────

    /// Phase A: count of CBs pending in the open submit group. Tests
    /// + telemetry consult this; 0 when the group is empty.
    pub(crate) fn submit_group_size(&self) -> usize {
        self.submit_group.size()
    }

    /// Phase A: true if any CB has been appended since the last flush.
    pub(crate) fn submit_group_is_open(&self) -> bool {
        self.submit_group.is_open()
    }

    /// Phase A: max capacity of the submit group before auto-flush.
    pub(crate) fn submit_group_max_size(&self) -> usize {
        self.submit_group.max_size()
    }

    /// Phase A T8: override the SubmitGroup max-size cap.  Exposed as
    /// a non-test `pub(crate)` method so `KmsBackend` integration
    /// tests (in `tests/`) can set the cap without needing
    /// `#[cfg(test)]`-gated visibility.
    pub(crate) fn submit_group_set_max_size_for_tests(&mut self, n: usize) {
        self.submit_group.set_max_size(n);
    }

    /// Phase A T9: peek at the SubmitGroup's buffered entries in
    /// append order. Allows ordering-invariant tests to assert that
    /// CBs land in the group in chronological submission order without
    /// requiring a flush that would destroy the snapshot.
    #[cfg(test)]
    pub(crate) fn submit_group_peek_entries_for_tests(&self) -> &[super::submit_group::GroupEntry] {
        self.submit_group.peek_entries()
    }

    /// Phase A T10: arm the fault-injection latch so the next
    /// `flush_submit_group` routes through `abort_flush` instead of the
    /// real `vkQueueSubmit2`. Not `#[cfg(test)]`-gated so that the
    /// `pub` wrapper on `KmsBackend` is reachable from the external
    /// `acceptance` integration-test crate.
    pub(crate) fn force_next_submit_failure_for_integration_tests(&mut self) {
        self.force_next_submit_failure = true;
    }

    /// Phase A: explicit flush of any buffered submit group. Issues one
    /// `vkQueueSubmit2` with all buffered CBs + signal semaphores,
    /// signaling the group's shared fence. Empty group → `Ok(FlushOutcome {
    /// flushed_entries: 0 })`. Vk-less fixture → same.
    ///
    /// Sets `renderer_failed` on `queue_submit2` failure (Phase A fatal
    /// policy for drawable state; SubmittedOp rollback is engine-side
    /// via `pending_group_ops`).
    pub(crate) fn flush_submit_group(
        &mut self,
        reason: FlushReason,
    ) -> Result<FlushOutcome, vk::Result> {
        self.flush_submit_group_with_exports(reason, &[])
    }

    /// GLX-TFP (Task 2.3): flush variant that performs bidirectional
    /// dma-buf implicit sync around the submit for the `exported_writes`
    /// drawables (their dma-buf fds, deduped by the caller):
    ///
    /// 1. **read→write wait** — before `vkQueueSubmit2`, export each
    ///    dma-buf's WRITE-scope sync-file, import it as a temporary Vulkan
    ///    semaphore, and wait in the submission so request/input dispatch
    ///    never blocks while a GL consumer is still sampling the buffer.
    /// 2. **signal semaphore** — when the list is non-empty, attach an
    ///    exportable SYNC_FD signal semaphore to the submit.
    /// 3. **write→read publish** — after submit, export that semaphore's
    ///    sync_file and IMPORT it onto each exported dma-buf as a WRITE
    ///    fence, so Mesa's implicit-sync GL read waits on our write.
    pub(crate) fn flush_submit_group_with_exports(
        &mut self,
        reason: FlushReason,
        exported_writes: &[(std::os::fd::BorrowedFd<'_>, bool)],
    ) -> Result<FlushOutcome, vk::Result> {
        // Empty-group fast path: do NOT consume the ticket.  An open
        // cow/render_batch may still be mid-recording (ticket Some,
        // entries empty).  Dropping the ticket here would force the
        // batch's eventual append to land in a ticket-less group,
        // tripping the "non-empty group has ticket" expect below.
        if self.submit_group.size() == 0 {
            let outcome = FlushOutcome {
                flushed_entries: 0,
                reason,
                aborted: false,
            };
            self.last_flush_outcome = Some(outcome);
            return Ok(outcome);
        }
        let (entries, ticket) = self.submit_group.take();
        let n = entries.len();
        // entries is guaranteed non-empty here (early-returned above).
        let Some(vk) = self.vk.as_ref() else {
            // Vk-less test fixture: drop entries + ticket on the floor.
            let outcome = FlushOutcome {
                flushed_entries: n,
                reason,
                aborted: false,
            };
            self.last_flush_outcome = Some(outcome);
            return Ok(outcome);
        };
        let ticket = ticket.expect("non-empty group has ticket");
        // Test-only fault injection: simulate a queue_submit2 failure.
        // The latch is always compiled (field is not cfg(test)) so the
        // `pub` wrapper on `KmsBackend` is reachable from the external
        // `acceptance` integration-test crate. In production the
        // field is initialised `false` and never set, so this branch is
        // never taken.
        if self.force_next_submit_failure {
            self.force_next_submit_failure = false;
            return self.abort_flush(entries, n, reason, vk::Result::ERROR_DEVICE_LOST);
        }
        // GLX-TFP read→write wait: snapshot every exported dma-buf's
        // WRITE-scope reservation fences and import them as temporary
        // Vulkan semaphore payloads. The GPU submission waits for active
        // GL readers; the single-threaded X request/input loop does not.
        let mut imported_wait_semaphores = Vec::with_capacity(exported_writes.len());
        for &(fd, prewaited) in exported_writes {
            if prewaited {
                continue;
            }
            use crate::kms::vk::dri3::{ExportedSyncFile, export_dmabuf_write_access_sync_file};
            match export_dmabuf_write_access_sync_file(fd) {
                ExportedSyncFile::Idle | ExportedSyncFile::Unsupported => {}
                ExportedSyncFile::Fd(sync_fd) => {
                    match crate::kms::vk::sync::import_sync_file(vk, sync_fd) {
                        Ok(semaphore) => imported_wait_semaphores.push(semaphore),
                        Err(e) => log::warn!(
                            "glx-tfp: failed to import exported-backing WRITE fence for fd {}: \
                             {e:?}; proceeding without the wait",
                            fd.as_raw_fd()
                        ),
                    }
                }
            }
        }
        // GLX-TFP write→read publish: when any exported drawable is
        // written, attach an exportable SYNC_FD signal semaphore to THIS
        // submit so its completion can be re-imported onto the dma-bufs
        // as a WRITE fence after submit.
        let export_signal: Option<PresentCompletionSignal> = if exported_writes.is_empty() {
            None
        } else {
            match create_present_completion_signal(Arc::clone(vk)) {
                Ok(sig) => Some(sig),
                Err(e) => {
                    log::warn!("glx-tfp: failed to create export signal semaphore: {e:?}");
                    None
                }
            }
        };
        let cb_infos: Vec<vk::CommandBufferSubmitInfo<'_>> = entries
            .iter()
            .map(|e| vk::CommandBufferSubmitInfo::default().command_buffer(e.cb))
            .collect();
        let mut sig_infos: Vec<vk::SemaphoreSubmitInfo<'_>> = entries
            .iter()
            .filter_map(|e| {
                e.signal.map(|s| {
                    vk::SemaphoreSubmitInfo::default()
                        .semaphore(s)
                        .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                })
            })
            .collect();
        if let Some(sig) = export_signal.as_ref() {
            sig_infos.push(
                vk::SemaphoreSubmitInfo::default()
                    .semaphore(sig.semaphore())
                    .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS),
            );
        }
        let wait_infos: Vec<vk::SemaphoreSubmitInfo<'_>> = imported_wait_semaphores
            .iter()
            .map(|&semaphore| {
                vk::SemaphoreSubmitInfo::default()
                    .semaphore(semaphore)
                    .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
            })
            .collect();
        let submit = [{
            let s = vk::SubmitInfo2::default()
                .command_buffer_infos(&cb_infos)
                .wait_semaphore_infos(&wait_infos);
            if sig_infos.is_empty() {
                s
            } else {
                s.signal_semaphore_infos(&sig_infos)
            }
        }];
        crate::vk_count!(queue_submit2);
        match unsafe {
            vk.device
                .queue_submit2(vk.graphics_queue, &submit, ticket.fence())
        } {
            Ok(()) => {
                ticket.retain_imported_wait_semaphores(imported_wait_semaphores);
                // GLX-TFP write→read publish: export the submit's
                // completion sync_file and import it onto every exported
                // dma-buf the group wrote.
                if let Some(sig) = export_signal.as_ref() {
                    Self::publish_export_write_fences(sig, exported_writes);
                }
                let outcome = FlushOutcome {
                    flushed_entries: n,
                    reason,
                    aborted: false,
                };
                self.last_flush_outcome = Some(outcome);
                Ok(outcome)
            }
            Err(e) => {
                unsafe {
                    for semaphore in imported_wait_semaphores {
                        vk.device.destroy_semaphore(semaphore, None);
                    }
                }
                self.abort_flush(entries, n, reason, e)
            }
        }
    }

    /// GLX-TFP (Task 2.3 Step 3): export `signal`'s completed-write
    /// sync_file and IMPORT it as a WRITE fence onto each exported
    /// dma-buf, so an implicit-sync GL read on the imported texture waits
    /// on yserver's write before sampling. `Unsupported` (old
    /// kernel/driver) is silently tolerated; other errors warn.
    fn publish_export_write_fences(
        signal: &PresentCompletionSignal,
        exported_writes: &[(std::os::fd::BorrowedFd<'_>, bool)],
    ) {
        let sync_fd = match signal.export_sync_file_fd() {
            Ok(Some(fd)) => fd,
            Ok(None) => return,
            Err(e) => {
                log::warn!("glx-tfp: export_sync_file for write-fence publish failed: {e:?}");
                return;
            }
        };
        for &(fd, _) in exported_writes {
            match crate::kms::vk::dri3::import_dmabuf_write_fence(fd, sync_fd.as_fd()) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::Unsupported => {}
                Err(e) => log::warn!("glx-tfp: import write fence failed: {e}"),
            }
        }
    }

    /// Phase A: shared abort path. Frees the just-taken CBs, stashes
    /// the `aborted: true` `FlushOutcome`, sets `renderer_failed`, and
    /// surfaces the underlying `vk::Result`. Both the real
    /// `queue_submit2 Err` arm and the test-only fault injection
    /// (Task 3 Step 7) route through this helper so cleanup is uniform.
    fn abort_flush(
        &mut self,
        entries: Vec<super::submit_group::GroupEntry>,
        n: usize,
        reason: FlushReason,
        err: vk::Result,
    ) -> Result<FlushOutcome, vk::Result> {
        self.renderer_failed = true;
        if let (Some(vk), Some(pool)) = (self.vk.as_ref(), self.ops_command_pool_handle()) {
            let cbs: Vec<vk::CommandBuffer> = entries.iter().map(|e| e.cb).collect();
            if !cbs.is_empty() {
                unsafe { vk.device.free_command_buffers(pool, &cbs) };
            }
        }
        let outcome = FlushOutcome {
            flushed_entries: n,
            reason,
            aborted: true,
        };
        self.last_flush_outcome = Some(outcome);
        Err(err)
    }

    /// Phase A: seed the group's shared ticket if not open, then return
    /// a clone for the caller to stash on its `SubmittedOp`. Mirrors the
    /// per-op ticket acquisition from today's `begin_op_cb` but the same
    /// ticket is handed back to every appender in the group.
    pub(crate) fn submit_group_ticket_or_open(&mut self) -> Result<FenceTicket, vk::Result> {
        if let Some(t) = self.submit_group.ticket() {
            return Ok(t.clone());
        }
        let fresh = self.acquire_fence_ticket()?;
        Ok(self.submit_group.open_with(fresh))
    }

    /// Phase A: consume the last `FlushOutcome` stored by
    /// `flush_submit_group`. Returns `None` if no flush has occurred
    /// since the last call.
    pub(crate) fn take_last_flush_outcome(&mut self) -> Option<FlushOutcome> {
        self.last_flush_outcome.take()
    }

    // ── I6a: FenceTicket primitives ─────────────────────────────

    /// Acquire a fresh, unsignaled fence. Caller passes
    /// `ticket.fence()` to `vkQueueSubmit2` as the signal fence.
    /// Cloned across consumers; final-drop recycles or leaks.
    ///
    /// # Errors
    ///
    /// Returns `Err` if Vk is not initialised (test fixture) or
    /// fence creation fails.
    pub(crate) fn acquire_fence_ticket(&self) -> Result<FenceTicket, vk::Result> {
        let pool = self
            .fence_pool
            .as_ref()
            .ok_or(vk::Result::ERROR_INITIALIZATION_FAILED)?;
        pool.acquire()
    }

    // ── I6b: scanout BO management ──────────────────────────────

    /// Pick the next BO to render into for `output_idx`, or
    /// `None` if all BOs are still in flight (the SceneCompositor
    /// should retry next core-loop iteration).
    ///
    /// The token carries `last_present_generation` and
    /// `content_invalidated` so the buffer-age algorithm in
    /// SceneCompositor doesn't need to reach into the pool.
    pub(crate) fn acquire_scanout_bo(&mut self, output_idx: usize) -> Option<ScanoutBoToken> {
        let pool = self.scanout_pools.get_mut(output_idx)?.as_mut()?;
        let gens = self.bo_generations.get(output_idx)?;
        for (bo_idx, bo) in pool.bos.iter().enumerate() {
            if bo.state.phase == BoPhase::Free {
                let entry = gens.get(bo_idx).copied().unwrap_or_default();
                return Some(ScanoutBoToken {
                    output_idx,
                    bo_idx,
                    extent: vk::Extent2D {
                        width: bo.width,
                        height: bo.height,
                    },
                    last_present_generation: entry.last_present_generation,
                    content_invalidated: entry.content_invalidated,
                });
            }
        }
        None
    }

    /// Framebuffer last presented by the scene compositor for this output.
    /// During M2 direct scanout the pool deliberately keeps this BO marked
    /// `OnScreen`: it is the known-good per-output target for one atomic
    /// transition back from the shared client framebuffer.
    pub(crate) fn retained_composed_framebuffer(
        &self,
        output_idx: usize,
    ) -> Option<::drm::control::framebuffer::Handle> {
        self.scanout_pools
            .get(output_idx)?
            .as_ref()?
            .bos
            .iter()
            .find(|bo| bo.state.phase == BoPhase::OnScreen)?
            .fb_handle
    }

    /// Mark a BO's content tracking as invalidated. Called by
    /// SceneCompositor on the 9b atomic-commit-failed path —
    /// the GPU rendered into the BO but KMS rejected the flip,
    /// so the BO contents are indeterminate.
    pub(crate) fn invalidate_bo(&mut self, output_idx: usize, bo_idx: usize) {
        if let Some(gens) = self.bo_generations.get_mut(output_idx)
            && let Some(g) = gens.get_mut(bo_idx)
        {
            g.content_invalidated = true;
            g.last_present_generation = None;
        }
    }

    /// Recycle a scanout BO whose GPU work was submitted but whose
    /// atomic commit was rejected. The caller must only invoke this
    /// after the compose fence has signaled, otherwise the BO could
    /// be rendered into again while the previous command buffer is
    /// still writing it.
    pub(crate) fn recycle_failed_submit_bo(&mut self, output_idx: usize, bo_idx: usize) {
        let Some(bo) = self
            .scanout_pools
            .get_mut(output_idx)
            .and_then(Option::as_mut)
            .and_then(|pool| pool.bos.get_mut(bo_idx))
        else {
            return;
        };
        bo.state = BoState::default();
    }

    /// VT-switch suspend: force every scanout BO on every output back to
    /// `BoPhase::Free` and reset its content tracking.
    ///
    /// A pageflip submitted just before a VT switch never gets its
    /// page-flip-complete event once DRM master is lost, so its BO would
    /// stay stuck in `Pending`/`OnScreen` forever. Combined with the
    /// scene draining its `pending_acks`, the platform pool would then
    /// leak a BO per VT round until `acquire_scanout_bo` starves and the
    /// output wedges (observed: `tick skip reason=NoBO` after a few VT
    /// switches; also the `on_page_flip_complete: >1 pending BO` warning
    /// from stale Pending BOs). `drain_all_pending` device-wait-idles and
    /// transitions each BO to `Free`, closing any held dma-buf fences.
    ///
    /// Content is marked invalidated so the post-resume full-damage
    /// repaint does a full redraw rather than trusting a stale buffer-age
    /// generation. Safe to call while still master (no DRM ioctl here —
    /// only Vulkan idle + fence-fd close).
    pub(crate) fn reset_scanout_bos_for_suspend(&mut self) {
        let Some(vk) = self.vk.clone() else {
            return;
        };
        for pool in self.scanout_pools.iter_mut().flatten() {
            pool.drain_all_pending(&vk);
        }
        for gens in &mut self.bo_generations {
            for g in gens {
                g.last_present_generation = None;
                g.content_invalidated = true;
            }
        }
    }

    /// Disable a single connector: issue a DRM `disable_output` for the
    /// matching `ActiveOutput`, free/drop its scanout pool entry, and
    /// remove it from `self.outputs` / parallel vecs.  Recomputes
    /// `fb_w`/`fb_h` from the remaining outputs (2-D, no recompact —
    /// client-driven layouts are preserved). Does NOT touch the
    /// `RandrIdAllocator` registry; callers update it after we return.
    ///
    /// Returns `Ok(true)` when the connector was found and disabled,
    /// `Ok(false)` when it was not currently in the active output list
    /// (already off — no-op), or `Err` on a DRM-level failure.
    pub(crate) fn disable_connector(&mut self, output_key: &OutputKey) -> io::Result<bool> {
        let connector = &output_key.connector_name;
        let idx = match self
            .outputs
            .iter()
            .position(|layout| &layout.key == output_key)
        {
            Some(i) => i,
            None => return Ok(false),
        };
        let device = Rc::clone(
            &self
                .device_for_output(output_key)
                .ok_or_else(|| io::Error::other(format!("no DRM device for {output_key:?}")))?
                .device,
        );

        // DRM disable (ALLOW_MODESET atomic commit zeroing the CRTC).
        if let Err(e) = crate::drm::modeset::disable_output(&device, &self.outputs[idx].output) {
            log::error!("render disable_connector: disable_output({connector}) failed: {e}");
            return Err(e);
        }

        self.remove_connector_at(idx);

        log::info!(
            "render disable_connector: {connector} disabled; fb now {}×{}",
            self.fb_w,
            self.fb_h
        );
        Ok(true)
    }

    /// Remove a connector after the caller has successfully disabled the
    /// complete old CRTC set. This is the topology-mutation counterpart to
    /// `disable_connector`: it must not issue a second ioctl against an
    /// already-off (or newly disconnected) connector object.
    pub(crate) fn remove_connector_after_all_off(&mut self, output_key: &OutputKey) -> bool {
        let Some(idx) = self
            .outputs
            .iter()
            .position(|layout| &layout.key == output_key)
        else {
            return false;
        };
        self.remove_connector_at(idx);
        true
    }

    fn remove_connector_at(&mut self, idx: usize) {
        let changed_device = self.outputs[idx].key.device_key;
        // Drop the scanout pool for this output so its VkImages are freed.
        if idx < self.scanout_pools.len() {
            self.scanout_pools.remove(idx);
        }
        if idx < self.bo_generations.len() {
            self.bo_generations.remove(idx);
        }
        if idx < self.first_pageflip_logged.len() {
            self.first_pageflip_logged.remove(idx);
        }
        self.outputs.remove(idx);

        // Recompute the virtual framebuffer extent from surviving outputs.
        // Do NOT recompact — other outputs may be client-positioned.
        let layouts: Vec<(i32, i32, u16, u16)> = self
            .outputs
            .iter()
            .map(|l| (l.x, l.y, l.width, l.height))
            .collect();
        let (fb_w, fb_h) = recompute_fb_extent_from(&layouts);
        self.fb_w = fb_w;
        self.fb_h = fb_h;
        self.prune_present_clocks_to_live_outputs();
        self.refresh_cursor_topology_for_devices(&HashSet::from([changed_device]));
    }

    /// Enable (or reconfigure) a single connector at `(x, y)` with
    /// the given `ModeSpec`.  Resolves the `ModeSpec` against the
    /// connector's discovered `Output::modes` list, (re)allocates the
    /// `ScanoutBoPool` when the resolution changes or the output was
    /// previously off, commits the modeset, and adds/updates the
    /// `ActiveOutput` in `self.outputs` and the parallel vecs.
    ///
    /// The `Output` for `connector` must be pre-discovered with the live
    /// routes of every same-device survivor reserved. The selected `Output`
    /// is consumed.
    ///
    /// On any failure after pool allocation, the pool is freed and the
    /// output stays off (no partial enable), leaving `self` consistent.
    ///
    /// Returns `Ok(())` on success.
    pub(crate) fn enable_connector(
        &mut self,
        output_key: &OutputKey,
        output: crate::platform::drm::Output,
        mode_spec: yserver_core::backend::ModeSpec,
        x: i32,
        y: i32,
    ) -> io::Result<()> {
        self.enable_connector_with_cursor_factory(
            output_key,
            output,
            mode_spec,
            x,
            y,
            initialize_cursor_plane_for_device,
        )
    }

    fn enable_connector_with_cursor_factory<F>(
        &mut self,
        output_key: &OutputKey,
        mut output: crate::platform::drm::Output,
        mode_spec: yserver_core::backend::ModeSpec,
        x: i32,
        y: i32,
        mut cursor_factory: F,
    ) -> io::Result<()>
    where
        F: FnMut(&mut KmsDevice, &[::drm::control::crtc::Handle], &str),
    {
        let connector = output.connector_name.clone();
        debug_assert_eq!(output_key.connector_name, connector);
        let device = Rc::clone(
            &self
                .device_for_output(output_key)
                .ok_or_else(|| io::Error::other(format!("no DRM device for {output_key:?}")))?
                .device,
        );

        if let Some(conflict) = self.outputs.iter().find(|layout| {
            layout.key.device_key == output_key.device_key
                && layout.key != *output_key
                && (layout.output.encoder == output.encoder
                    || layout.output.crtc == output.crtc
                    || layout.output.plane == output.plane)
        }) {
            return Err(io::Error::other(format!(
                "enable_connector {connector}: proposed encoder {:?}/CRTC {:?}/plane {:?} conflicts with \
                 live output {} on the same DRM device",
                output.encoder, output.crtc, output.plane, conflict.output.connector_name
            )));
        }

        // Resolve ModeSpec → the DRM mode on the output.
        // `Output::modes` is the full advertised list (preferred-first).
        let matched = output
            .modes
            .iter()
            .find(|m| {
                m.width == mode_spec.width
                    && m.height == mode_spec.height
                    && m.vrefresh == mode_spec.vrefresh
            })
            .cloned();
        let mode_local = matched.ok_or_else(|| {
            io::Error::other(format!(
                "connector {connector}: mode {}×{}@{} not in advertised list",
                mode_spec.width, mode_spec.height, mode_spec.vrefresh
            ))
        })?;

        // Find the matching DRM mode by index (modes / drm_mode are
        // co-indexed in finalize_output).  We need `output.mode` to be
        // the DRM-level blob for commit_modeset.
        // `Output.modes` was sorted preferred-first by discover_outputs,
        // so we need the raw connector_info modes.  Instead, we search
        // by name+size+vrefresh in the already-set `output.picked` /
        // `output.modes` list with the picked index trick reused from
        // finalize_output: find mode_local by (name,w,h,vrefresh).
        //
        // The simplest reliable approach: the DRM mode is exactly the
        // one that was stored in `output.mode` when `discover_outputs`
        // called `finalize_output`, which selects via `pick_mode`.
        // For a client-requested mode that differs from the picked one,
        // we must force the mode.  `Output` doesn't carry the full
        // DrmMode list — it only carries `mode` (the picked one) and
        // `modes` (the logical Mode structs).
        //
        // Strategy: set `output.picked = mode_local` and reconstruct
        // the DRM mode from the `output.mode` field ONLY when it matches,
        // otherwise we need a DRM-level lookup.  Since we hold the raw
        // `Output` returned by `discover_outputs` (which runs
        // `get_connector` under the hood), and `Output::mode` is the
        // DRM mode for `picked`, we detect the match:
        if output.picked.width != mode_spec.width
            || output.picked.height != mode_spec.height
            || output.picked.vrefresh != mode_spec.vrefresh
        {
            // Client requested a non-picked mode.  We need to re-derive
            // the DRM mode for it.  The cleanest safe path: re-run
            // discover_outputs limited to this connector is expensive;
            // instead we call get_connector inline to fetch the full
            // DRM mode list, then match by (width, height, vrefresh).
            use ::drm::control::Device as ControlDevice;
            let resources = device.resource_handles().map_err(|e| {
                io::Error::other(format!("enable_connector: resource_handles failed: {e}"))
            })?;
            let mut drm_mode_opt: Option<::drm::control::Mode> = None;
            'outer: for &handle in resources.connectors() {
                let info = match device.get_connector(handle, false) {
                    Ok(i) => i,
                    Err(_) => continue,
                };
                if format!("{info}") != connector {
                    continue;
                }
                for m in info.modes() {
                    let (w, h) = m.size();
                    if w == mode_spec.width
                        && h == mode_spec.height
                        && m.vrefresh() == mode_spec.vrefresh
                    {
                        drm_mode_opt = Some(*m);
                        break 'outer;
                    }
                }
            }
            let drm_mode = drm_mode_opt.ok_or_else(|| {
                io::Error::other(format!(
                    "connector {connector}: DRM mode {}×{}@{} not found via kernel",
                    mode_spec.width, mode_spec.height, mode_spec.vrefresh
                ))
            })?;
            output.mode = drm_mode;
            output.picked = mode_local;
        }
        // At this point output.mode is the DRM mode for the requested spec.

        let w = mode_spec.width;
        let h = mode_spec.height;

        // Check whether this connector is already in the active set and
        // whether its resolution matches.
        let existing_idx = self
            .outputs
            .iter()
            .position(|layout| &layout.key == output_key);
        let needs_pool_realloc = match existing_idx {
            Some(idx) => self.outputs[idx].width != w || self.outputs[idx].height != h,
            None => true,
        };

        // (Re)allocate the scanout pool if needed.
        let mut new_pool = if needs_pool_realloc {
            if let Some(vk) = self.vk.as_ref().cloned() {
                match ScanoutBoPool::allocate(
                    Arc::clone(&vk),
                    Rc::clone(&device),
                    u32::from(w),
                    u32::from(h),
                    3,
                    &output.scanout_modifiers,
                ) {
                    Ok(pool) => Some(Some(pool)),
                    Err(e) => {
                        log::warn!(
                            "render enable_connector: scanout pool alloc failed for {connector} ({}×{}): {e:?}",
                            w,
                            h
                        );
                        // Pool allocation failed — leave output off,
                        // return error to caller.
                        return Err(io::Error::other(format!(
                            "enable_connector {connector}: scanout pool alloc failed: {e:?}"
                        )));
                    }
                }
            } else {
                // Test fixture: no Vk; pool stays None.
                Some(None)
            }
        } else {
            None // keep existing pool
        };

        // Build an initial fb for the modeset commit.  Pick the
        // OnScreen BO from the existing pool (if unchanged), or the
        // first BO in the new pool.  Fall back to a legacy dumb buffer
        // if nothing is available.
        let fb_id = {
            let pool_ref: Option<&ScanoutBoPool> = if needs_pool_realloc {
                new_pool.as_ref().and_then(|p| p.as_ref())
            } else {
                existing_idx
                    .and_then(|i| self.scanout_pools.get(i))
                    .and_then(|p| p.as_ref())
            };
            pool_ref.and_then(|pool| {
                use crate::kms::vk::scanout::BoPhase;
                pool.bos
                    .iter()
                    .find(|bo| bo.state.phase == BoPhase::OnScreen)
                    .and_then(|bo| bo.fb_handle)
                    .or_else(|| pool.bos.iter().find_map(|bo| bo.fb_handle))
            })
        };

        let fb_for_commit = fb_id.ok_or_else(|| {
            // Free the newly-allocated pool before returning error.
            // (new_pool would be dropped by going out of scope, which
            //  is the desired free.)
            io::Error::other(format!(
                "enable_connector {connector}: no fb handle available for initial modeset"
            ))
        });

        let fb_for_commit = match fb_for_commit {
            Ok(fb) => fb,
            Err(e) => {
                // new_pool dropped here (freed).
                return Err(e);
            }
        };

        // Commit the modeset.  On failure, pool is freed (dropped below).
        if let Err(e) = crate::drm::modeset::commit_modeset(&device, &output, fb_for_commit) {
            log::error!(
                "render enable_connector: commit_modeset for {connector} ({}×{}@{}) at ({x},{y}) failed: {e}",
                mode_spec.width,
                mode_spec.height,
                mode_spec.vrefresh
            );
            // new_pool dropped here (freed on stack unwind).
            return Err(e);
        }

        // A synchronous modeset has already latched this framebuffer; unlike
        // an ordinary page flip no completion event will promote it from
        // Pending. Reserve it now so the compositor cannot immediately acquire
        // and render into the live front buffer.
        let mark_front = |pool: &mut ScanoutBoPool| {
            if let Some(bo) = pool
                .bos
                .iter_mut()
                .find(|bo| bo.fb_handle == Some(fb_for_commit))
            {
                bo.state.mark_on_screen_after_modeset();
            }
        };
        if needs_pool_realloc {
            if let Some(pool) = new_pool.as_mut().and_then(Option::as_mut) {
                mark_front(pool);
            }
        } else if let Some(pool) = existing_idx
            .and_then(|idx| self.scanout_pools.get_mut(idx))
            .and_then(Option::as_mut)
        {
            mark_front(pool);
        }

        // Commit succeeded — install the output into the active set.
        if let Some(idx) = existing_idx {
            // Update in-place.
            self.outputs[idx].output = output;
            self.outputs[idx].x = x;
            self.outputs[idx].y = y;
            self.outputs[idx].width = w;
            self.outputs[idx].height = h;
            if let Some(pool) = new_pool {
                if idx < self.scanout_pools.len() {
                    self.scanout_pools[idx] = pool;
                }
                if idx < self.bo_generations.len() {
                    self.bo_generations[idx] = self
                        .scanout_pools
                        .get(idx)
                        .and_then(|p| p.as_ref())
                        .map(|p| vec![BoGenerationEntry::default(); p.bos.len()])
                        .unwrap_or_default();
                }
            }
        } else {
            // New output — push to end.
            self.outputs.push(ActiveOutput::new(
                output_key.device_key,
                output,
                drm::Swapchain::empty_for_tests(),
                x,
                y,
            ));
            let pool = new_pool.unwrap_or(None);
            let gens = pool
                .as_ref()
                .map(|p| vec![BoGenerationEntry::default(); p.bos.len()])
                .unwrap_or_default();
            self.scanout_pools.push(pool);
            self.bo_generations.push(gens);
            self.first_pageflip_logged.push(false);
        }

        // Recompute virtual framebuffer extent (2-D, no recompact).
        let layouts: Vec<(i32, i32, u16, u16)> = self
            .outputs
            .iter()
            .map(|l| (l.x, l.y, l.width, l.height))
            .collect();
        let (fb_w, fb_h) = recompute_fb_extent_from(&layouts);
        self.fb_w = fb_w;
        self.fb_h = fb_h;
        self.prune_present_clocks_to_live_outputs();
        let changed_devices = HashSet::from([output_key.device_key]);
        // Refresh first. If a previous first-output attempt failed
        // transiently, this later explicit topology boundary retries it. A
        // genuinely deferred device is intentionally skipped here so a
        // failure in the new lazy attempt cannot be retried twice at the same
        // boundary.
        self.refresh_cursor_topology_for_devices_with(&changed_devices, &mut cursor_factory);
        self.initialize_headless_cursor_for_device_with(
            output_key.device_key,
            "first successful explicit RANDR enable",
            |device, crtcs, boundary| cursor_factory(device, crtcs, boundary),
        );

        log::info!(
            "render enable_connector: {connector} enabled {}×{}@{} at ({x},{y}); fb now {}×{}",
            mode_spec.width,
            mode_spec.height,
            mode_spec.vrefresh,
            fb_w,
            fb_h
        );
        Ok(())
    }

    /// Called by the SceneCompositor's tick after `present_scanout`
    /// returns Ok. Records that `bo_idx` is now pending the next
    /// page-flip-complete event for `output_idx`, and assigns the
    /// generation number for the in-flight frame.
    ///
    /// Returns the freshly-allocated generation.
    pub(crate) fn record_present(&mut self, _output_idx: usize, _bo_idx: usize) -> u64 {
        self.next_present_generation = self
            .next_present_generation
            .checked_add(1)
            .expect("next_present_generation overflow");
        self.next_present_generation
    }

    /// Page-flip-complete callback. Walks the output's BOs, finds
    /// the one currently `Pending` (just retired by the kernel),
    /// transitions its state, and returns the retirement info.
    /// `None` means no flip was pending — a spurious or
    /// startup-flushed event.
    ///
    /// The caller (SceneCompositor) then advances the matching
    /// `bo_generations[output_idx][bo_idx].last_present_generation`
    /// via [`Self::commit_bo_present`].
    pub(crate) fn on_page_flip_complete(
        &mut self,
        output_idx: usize,
    ) -> Option<PageFlipRetirement> {
        let pool = self.scanout_pools.get_mut(output_idx)?.as_mut()?;
        // First pass: find any BO currently `Pending`. Walk only
        // — don't mutate during the search.
        let mut pending: Option<usize> = None;
        let mut on_screen: Option<usize> = None;
        for (i, bo) in pool.bos.iter().enumerate() {
            match bo.state.phase {
                BoPhase::Pending => {
                    if let Some(prev) = pending {
                        // More than one pending — shouldn't
                        // happen; the kernel flips one at a time.
                        log::warn!(
                            "render on_page_flip_complete: output {output_idx} has >1 pending BO; \
                             retiring first found ({prev})",
                        );
                    } else {
                        pending = Some(i);
                    }
                }
                BoPhase::OnScreen => {
                    on_screen = Some(i);
                }
                _ => {}
            }
        }
        let presented = pending?;
        // Transitions:
        //   - the previously OnScreen bo goes Retiring → Free
        //   - the previously Pending bo goes OnScreen
        // Doing it in this order matches v1's compositor.
        let retired = if let Some(prev) = on_screen {
            pool.bos[prev].state.transition_to_retiring();
            let released = pool.bos[prev].state.transition_to_free_after_retire();
            if let Some(fd) = released {
                // SAFETY: the release fence fd was owned by us;
                // close it now that the BO is free.
                unsafe { libc::close(fd) };
            }
            Some(prev)
        } else {
            None
        };
        pool.bos[presented].state.transition_to_on_screen();

        let logged_first = self
            .first_pageflip_logged
            .get_mut(output_idx)
            .map(|f| std::mem::replace(f, true))
            .unwrap_or(true);
        if !logged_first {
            log::info!("render: first pageflip complete on output {output_idx} (bo {presented})",);
        } else {
            log::debug!("render: pageflip complete on output {output_idx} (bo {presented})",);
        }
        Some(PageFlipRetirement {
            retired_bo_idx: retired,
            presented_bo_idx: presented,
            generation: 0, // assigned by record_present; this is informational
        })
    }

    /// SceneCompositor calls this on page-flip-complete after
    /// `on_page_flip_complete` to write the new
    /// `last_present_generation` and clear `content_invalidated`.
    pub(crate) fn commit_bo_present(&mut self, output_idx: usize, bo_idx: usize, generation: u64) {
        if let Some(gens) = self.bo_generations.get_mut(output_idx)
            && let Some(g) = gens.get_mut(bo_idx)
        {
            g.last_present_generation = Some(generation);
            g.content_invalidated = false;
        }
    }

    // ── Disable output ──────────────────────────────────────────

    /// Best-effort wait for all in-flight GPU work to complete, bounded
    /// to 5 seconds (matching the `FenceTicket::wait` / `device_wait_idle`
    /// convention used at shutdown). Called by `KmsBackend::run_suspend`
    /// before DRM master is dropped, so in-flight submits don't race a
    /// kernel-side scanout teardown.
    ///
    /// Errors from `device_wait_idle` are logged and swallowed: the VT
    /// release path must always continue even if the wait times out or the
    /// device is already lost.
    pub(crate) fn wait_idle_bounded(&self) {
        // `device_wait_idle` is inherently blocking; 5 s is the same bound
        // used by FenceTicket::wait in the pool destructor.  We do not set a
        // real timeout here because ash's `device_wait_idle` wraps
        // `vkDeviceWaitIdle` which has no timeout parameter — on a lost
        // device it returns VK_ERROR_DEVICE_LOST promptly.  The 5-second
        // comment in the plan refers to the *practical* upper bound the
        // driver enforces on a wedged device; real quiescence is typically
        // sub-millisecond.
        if let Some(vk) = self.vk.as_ref() {
            let result = unsafe { vk.device.device_wait_idle() };
            if let Err(e) = result {
                log::warn!("kms: wait_idle_bounded: device_wait_idle failed: {e:?}");
            }
        }
    }

    /// Post-loop teardown — disable each output, leaving the
    /// scanout BOs in a state where their Drop can clean up
    /// (or, on atomic disable failure, disarm them so we leak
    /// rather than confuse KMS — same shape as v1).
    ///
    /// # Errors
    ///
    /// Propagates the first per-output `disable_output` failure;
    /// subsequent outputs still attempted.
    pub(crate) fn disable_output(&mut self) -> io::Result<()> {
        self.shutting_down = true;

        // Best-effort: drain all in-flight GPU work before
        // pulling the modeset.
        if let Some(vk) = self.vk.as_ref() {
            unsafe {
                let _ = vk.device.device_wait_idle();
            }
        }

        // Stage 3f.10: drain the pixmap pool so the recycled
        // image/memory/view triples don't leak through the
        // VkContext destruction path. Safe to drain here: every
        // in-flight CB has been waited on by device_wait_idle.
        if let Some(pool) = self.pixmap_pool.as_ref() {
            pool.drain();
        }

        let mut first_err: Option<io::Error> = None;
        for (i, layout) in self.outputs.iter().enumerate() {
            let Some(device) = self
                .device_for_output(&layout.key)
                .map(|device| Rc::clone(&device.device))
            else {
                log::warn!(
                    "render disable_output: no DRM device {} for {}",
                    layout.key.device_key,
                    layout.output.connector_name,
                );
                continue;
            };
            if let Err(e) = drm::modeset::disable_output(&device, &layout.output) {
                log::warn!(
                    "render disable_output: failed for {} (output {i}): {e}",
                    layout.output.connector_name,
                );
                // Disarm the matching scanout pool so its Drop
                // doesn't try to destroy framebuffers KMS may
                // still hold (matches v1's behaviour).
                if let Some(pool) = self.scanout_pools.get_mut(i).and_then(|p| p.as_mut()) {
                    for bo in &mut pool.bos {
                        bo.disarm();
                    }
                }
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Drive every output to a binary KMS power state. Used by
    /// DPMS — collapses Standby/Suspend/Off to "outputs inactive"
    /// and On to "outputs active". Unlike the post-loop
    /// `disable_output` it does NOT set `shutting_down`, NOT call
    /// `device_wait_idle`, and NOT disarm scanout pools — DPMS is
    /// reversible.
    ///
    /// # Errors
    ///
    /// Collects the first per-output failure, continues with the
    /// rest, then returns it. The caller (KmsBackend::set_dpms_power)
    /// logs and advances the in-memory DPMS state regardless.
    pub(crate) fn dpms_set_outputs_active(&mut self, active: bool) -> io::Result<()> {
        let mut first_err: Option<io::Error> = None;
        if active {
            // Re-commit modeset. Pick the OnScreen BO (last frame
            // before blank) or any registered fb — same selection
            // logic as `requery_outputs_and_modeset` at :2030.
            for (i, layout) in self.outputs.iter().enumerate() {
                let Some(device) = self
                    .device_for_output(&layout.key)
                    .map(|device| Rc::clone(&device.device))
                else {
                    let error = io::Error::new(
                        io::ErrorKind::NotFound,
                        format!(
                            "dpms_set_outputs_active(true): no DRM device {} for {}",
                            layout.key.device_key, layout.output.connector_name,
                        ),
                    );
                    log::error!("{error}");
                    if first_err.is_none() {
                        first_err = Some(error);
                    }
                    continue;
                };
                let front =
                    self.scanout_pools
                        .get(i)
                        .and_then(|p| p.as_ref())
                        .and_then(|pool| {
                            use crate::kms::vk::scanout::BoPhase;
                            pool.bos
                                .iter()
                                .enumerate()
                                .find(|(_, bo)| bo.state.phase == BoPhase::OnScreen)
                                .and_then(|(bo_idx, bo)| bo.fb_handle.map(|fb| (bo_idx, fb)))
                                .or_else(|| {
                                    pool.bos.iter().enumerate().find_map(|(bo_idx, bo)| {
                                        bo.fb_handle.map(|fb| (bo_idx, fb))
                                    })
                                })
                        });
                let Some((bo_idx, fb_id)) = front else {
                    let error = io::Error::other(format!(
                        "dpms_set_outputs_active(true): no framebuffer for output {}; \
                         an ordinary page flip cannot restore MODE_ID/ACTIVE",
                        layout.output.connector_name
                    ));
                    log::error!("{error}");
                    if first_err.is_none() {
                        first_err = Some(error);
                    }
                    continue;
                };
                if let Err(e) = crate::drm::modeset::commit_modeset(&device, &layout.output, fb_id)
                {
                    log::error!(
                        "dpms_set_outputs_active(true): commit_modeset for {} failed: {e}",
                        layout.output.connector_name,
                    );
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                } else if let Some(bo) = self
                    .scanout_pools
                    .get_mut(i)
                    .and_then(Option::as_mut)
                    .and_then(|pool| pool.bos.get_mut(bo_idx))
                {
                    bo.state.mark_on_screen_after_modeset();
                }
            }
        } else {
            for layout in &self.outputs {
                let Some(device) = self
                    .device_for_output(&layout.key)
                    .map(|device| Rc::clone(&device.device))
                else {
                    let error = io::Error::new(
                        io::ErrorKind::NotFound,
                        format!(
                            "dpms_set_outputs_active(false): no DRM device {} for {}",
                            layout.key.device_key, layout.output.connector_name,
                        ),
                    );
                    log::error!("{error}");
                    if first_err.is_none() {
                        first_err = Some(error);
                    }
                    continue;
                };
                if let Err(e) = crate::drm::modeset::disable_output(&device, &layout.output) {
                    log::error!(
                        "dpms_set_outputs_active(false): disable_output for {} failed: {e}",
                        layout.output.connector_name,
                    );
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    // ── VT-switch resume helpers (Task 12) ─────────────────────────
    //
    // Called from `KmsBackend::run_resume` after direct VT acquire has
    // restored DRM master on the same DRM fd opened at startup.

    /// Pack outputs left-to-right, but leave client-configured outputs
    /// where the client placed them (Task 5.1). A client SetCrtcConfig/
    /// SetScreenSize "pins" an output's `(x, y)`; the auto-layout must not
    /// flatten it back to the boot extend-right arrangement on a rescan or
    /// VT-resume. Auto (unpinned) outputs still pack sequentially, advancing
    /// past any pinned output's extent so the common left-to-right case
    /// doesn't overlap. (Mixed pinned+auto with gaps is refined later if a
    /// real workload needs it; the common case is all-auto at boot or
    /// all-pinned after the desktop configures the layout.)
    fn recompact_horizontal_layout(&mut self, client_configured: &HashSet<OutputKey>) {
        let mut next_x: i32 = 0;
        for layout in &mut self.outputs {
            if client_configured.contains(&layout.key) {
                next_x = next_x.max(layout.x.saturating_add(i32::from(layout.width)));
                continue;
            }
            layout.x = next_x;
            layout.y = 0;
            next_x = next_x.saturating_add(i32::from(layout.width));
        }
    }

    /// Apply a previously gathered all-device connector snapshot. Callers must
    /// quiesce GPU/page-flip state before invoking this method: it may remove
    /// scanout pools and ActiveOutputs for disconnected connectors.
    pub(crate) fn apply_connector_snapshot(
        &mut self,
        connected: Vec<ConnectorSnapshot>,
        client_configured: &HashSet<OutputKey>,
        known_connected: &HashSet<OutputKey>,
    ) -> RescanResult {
        let connected_order: Vec<OutputKey> = connected
            .iter()
            .map(|snapshot| snapshot.key.clone())
            .collect();
        let connected_keys: HashSet<OutputKey> = connected_order.iter().cloned().collect();
        // A connector can be physically connected yet advertise no usable
        // mode. Keep it connected in RANDR, but it cannot retain a live CRTC
        // route or scanout pool.
        let active_survivor_keys: HashSet<OutputKey> = self
            .outputs
            .iter()
            .filter(|output| {
                connected
                    .iter()
                    .any(|snapshot| snapshot.preserves_active_output(output))
            })
            .map(|output| output.key.clone())
            .collect();
        let snapshot_by_key: HashMap<OutputKey, ConnectorSnapshot> = connected
            .iter()
            .map(|snapshot| (snapshot.key.clone(), snapshot.clone()))
            .collect();

        let mut rescan = RescanResult {
            added_keys: connected_order
                .iter()
                .filter(|key| !known_connected.contains(*key))
                .cloned()
                .collect(),
            dropped_keys: known_connected
                .difference(&connected_keys)
                .cloned()
                .collect(),
            connected,
            ..RescanResult::default()
        };
        for (idx, layout) in self.outputs.iter().enumerate() {
            if active_survivor_keys.contains(&layout.key) {
                continue;
            }
            log::warn!(
                "render rescan: output {} disconnected — dropping active scanout",
                layout.output.connector_name,
            );
            rescan.dropped_old_indices.push(idx);
            // A preceding forced RANDR probe may already have marked this
            // connector disconnected in the registry. Keep the active-output
            // identity in the transition result anyway so its Enabled config
            // and client-configured bit are retired when the physical rescan
            // removes the live CRTC.
            rescan.dropped_keys.push(layout.key.clone());
        }
        rescan.dropped_keys.sort();
        rescan.dropped_keys.dedup();
        rescan.dropped_old_indices.sort_unstable_by(|a, b| b.cmp(a));
        let cursor_changed_devices: HashSet<_> = rescan
            .dropped_old_indices
            .iter()
            .filter_map(|idx| self.outputs.get(*idx))
            .map(|output| output.key.device_key)
            .collect();
        for idx in rescan.dropped_old_indices.iter().copied() {
            self.outputs.remove(idx);
            if idx < self.scanout_pools.len() {
                self.scanout_pools.remove(idx);
            }
            if idx < self.bo_generations.len() {
                self.bo_generations.remove(idx);
            }
            if idx < self.first_pageflip_logged.len() {
                self.first_pageflip_logged.remove(idx);
            }
        }

        for layout in &mut self.outputs {
            if let Some(snapshot) = snapshot_by_key.get(&layout.key) {
                // A probe does not commit a route. Preserve the exact live
                // connector/CRTC/plane handles, property IDs, picked mode and
                // DRM mode blob; refresh only connector-owned metadata. The
                // next explicit RANDR modeset discovers and commits any
                // changed assignment on this output's owning card.
                layout.output.modes.clone_from(&snapshot.modes);
                layout.output.mm_width = snapshot.mm_width;
                layout.output.mm_height = snapshot.mm_height;
                layout.output.edid.clone_from(&snapshot.edid);
                layout
                    .output
                    .connector_type
                    .clone_from(&snapshot.connector_type);
            }
        }

        // Runtime discovery never auto-enables a newly-connected connector.
        // It enters the registry connected-but-Off; SetCrtcConfig performs
        // the expensive assignment/scanout allocation if a client enables it.
        rescan.added_count = rescan.added_keys.len();

        if !rescan.dropped_old_indices.is_empty() {
            self.recompact_horizontal_layout(client_configured);
            let layouts: Vec<(i32, i32, u16, u16)> = self
                .outputs
                .iter()
                .map(|layout| (layout.x, layout.y, layout.width, layout.height))
                .collect();
            let (fb_w, fb_h) = recompute_fb_extent_from(&layouts);
            self.fb_w = fb_w;
            self.fb_h = fb_h;
            self.prune_present_clocks_to_live_outputs();
            self.refresh_cursor_topology_for_devices(&cursor_changed_devices);
        }
        rescan
    }

    /// Re-arm the hardware cursor plane on every CRTC that was
    /// showing the cursor before the suspend. This restores the
    /// kernel-side cursor binding that the VT switch tore down.
    ///
    /// Called after a connector snapshot has been applied so the output list
    /// is up to date. The cursor position and hotspot come from the
    /// caller (backend passes `core.cursor_x/y` and the effective
    /// cursor's hotspot).
    pub(crate) fn rearm_cursor(&mut self, hot_x: u16, hot_y: u16, x: i32, y: i32) {
        self.refresh_cursor_topology();
        let routes: Vec<_> = self
            .outputs
            .iter()
            .enumerate()
            .filter_map(|(output_idx, layout)| {
                let device = self.device_for_key(layout.key.device_key)?;
                device
                    .cursor
                    .plane
                    .as_ref()
                    .is_some_and(|plane| plane.is_visible_on(layout.output.crtc))
                    .then_some((output_idx, device.key, layout.output.crtc))
            })
            .collect();
        log::info!(
            "render resume rearm_cursor: outputs={} initialized_devices={} visible_routes={} hot=({hot_x},{hot_y}) pos=({x},{y})",
            self.outputs.len(),
            self.devices
                .iter()
                .filter(|device| device.cursor.plane.is_some())
                .count(),
            routes.len(),
        );
        let mut shown = 0usize;
        let mut failed = 0usize;
        for (output_idx, device_key, crtc) in routes {
            match self.cursor_plane_show_on_crtc(output_idx, hot_x, hot_y, x, y) {
                Ok(()) => {
                    shown += 1;
                    log::info!(
                        "render resume rearm_cursor: device {device_key} CRTC={crtc:?} show ok"
                    );
                }
                Err(error) => {
                    failed += 1;
                    log::warn!(
                        "render resume rearm_cursor: device {device_key} CRTC={crtc:?} show failed: {error}"
                    );
                }
            }
        }
        log::info!("render resume rearm_cursor: done — shown={shown} failed={failed}");
    }

    /// Mark the scene compositor dirty so every output gets a
    /// full-damage repaint on the next composite tick. Called after
    /// `vt_state` commits to `Active` so the scanout gate is open
    /// when `composite_and_flip` runs.
    pub(crate) fn post_full_damage_all_outputs(&mut self) {
        self.shutting_down = false; // ensure shutting_down doesn't suppress the repaint
        // `wake_for_damage` sets `scene_structure_dirty = true`; the
        // SceneCompositor picks this up on the next `tick` and repaints
        // every output with a full-screen damage rect.
        // (Accessed indirectly through KmsBackend::scene; the caller
        // on backend.rs calls self.scene.wake_for_damage() directly —
        // this stub exists to satisfy the plan's "three helpers on
        // PlatformBackend" requirement; in practice the scene field
        // lives on KmsBackend, not PlatformBackend, so the backend
        // calls the scene method directly and this fn is not used
        // for the scene part. It IS the right place to clear any
        // platform-level inhibit flags on resume.)
    }
}

fn cursor_root_to_crtc_local(
    x: i32,
    y: i32,
    layout_x: i32,
    layout_y: i32,
    hot_x: u16,
    hot_y: u16,
) -> (i32, i32) {
    (
        x - layout_x - i32::from(hot_x),
        y - layout_y - i32::from(hot_y),
    )
}

/// Whether the cursor footprint `[dx, dx+cw) × [dy, dy+ch)` (in
/// output-local coordinates) overlaps the output's `[0, w) × [0, h)`
/// region. This is the boolean form of `cursor_footprint_rect`'s
/// non-empty condition, kept in sync so `cursor_crtc_membership_dirty`
/// decides on-output membership exactly as the scene does.
fn cursor_footprint_intersects_output(dx: i32, dy: i32, cw: i32, ch: i32, w: i32, h: i32) -> bool {
    dx < w && dx + cw > 0 && dy < h && dy + ch > 0
}

/// Decide whether KMS bring-up may proceed, given how many outputs were
/// enumerated versus how many got a live scanout pool.
///
/// Refuse only when there is at least one connected output but *none* of
/// them can be driven (`output_count > 0 && live_pool_count == 0`) — that
/// is the "display attached but we can't put anything on it" case, which
/// otherwise manifests as a silent black screen (e.g. RPi 4/400 split-GPU
/// scanout with no shared modifier). Zero outputs is not a failure
/// (headless start; runtime hotplug may attach one later), and partial
/// success (some outputs live) proceeds on the ones that work.
fn check_scanout_liveness(
    output_count: usize,
    live_pool_count: usize,
    errors: &[String],
) -> Result<(), String> {
    if output_count > 0 && live_pool_count == 0 {
        return Err(format!(
            "no displayable output: all {output_count} connected output(s) failed scanout \
             buffer allocation, so nothing can be shown. Per-output errors: [{}]",
            errors.join("; ")
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drm_key(minor: u32) -> crate::platform::drm::DrmDeviceKey {
        crate::platform::drm::DrmDeviceKey { major: 226, minor }
    }

    #[test]
    fn verified_render_device_accepts_its_advertised_node() {
        assert!(
            validate_render_node_attachment(RenderDeviceId::DrmRender(drm_key(128)), drm_key(128))
                .is_ok()
        );
    }

    #[test]
    fn verified_render_device_rejects_a_different_opened_node() {
        let error =
            validate_render_node_attachment(RenderDeviceId::DrmRender(drm_key(128)), drm_key(129))
                .expect_err("a verified renderer must not acquire another node's resources");
        assert!(error.to_string().contains("226:128"));
        assert!(error.to_string().contains("226:129"));
    }

    #[test]
    fn unverified_fallback_can_attach_the_resolved_node_without_claiming_identity() {
        assert!(
            validate_render_node_attachment(RenderDeviceId::UnverifiedFallback, drm_key(129))
                .is_ok()
        );
    }

    fn test_kms_device(
        key: crate::platform::drm::DrmDeviceKey,
        nvidia_policy_disabled: bool,
    ) -> KmsDevice {
        KmsDevice {
            key,
            device: Rc::new(drm::Device::for_tests().expect("test DRM device")),
            cursor: KmsCursorState::new(nvidia_policy_disabled),
        }
    }

    #[test]
    fn renderer_primary_relationship_distinguishes_unknown_from_different() {
        let kms = test_kms_device(drm_key(0), false);
        let renderer = |primary| RenderDevice {
            id: RenderDeviceId::UnverifiedFallback,
            physical_device: vk::PhysicalDevice::default(),
            advertised_primary_node: primary,
            advertised_render_node: None,
            render_node: None,
            render_node_device: None,
            syncobj_timeline: false,
        };

        assert_eq!(renderer(None).is_same_device_as(&kms), None);
        assert_eq!(
            renderer(Some(drm_key(0))).is_same_device_as(&kms),
            Some(true)
        );
        assert_eq!(
            renderer(Some(drm_key(1))).is_same_device_as(&kms),
            Some(false)
        );
    }

    fn install_test_cursor_plane(
        device: &mut KmsDevice,
        crtcs: &[::drm::control::crtc::Handle],
        boundary: &str,
    ) {
        let plane = crate::kms::cursor_plane::CursorPlane::for_tests_stub(
            Rc::clone(&device.device),
            64,
            64,
        );
        install_cursor_plane_for_device(device, crtcs, boundary, plane);
    }

    fn test_active_output_for(
        device_key: crate::platform::drm::DrmDeviceKey,
        connector_name: &str,
        raw_crtc: u32,
    ) -> ActiveOutput {
        let mut seed = PlatformBackend::for_tests();
        let mut output = seed.outputs.remove(0);
        output.key = OutputKey::new(device_key, connector_name);
        output.output.connector_name = connector_name.to_string();
        output.output.connector = ::drm::control::from_u32(raw_crtc).unwrap();
        output.output.encoder = ::drm::control::from_u32(raw_crtc).unwrap();
        output.output.crtc = ::drm::control::from_u32(raw_crtc).unwrap();
        output.output.plane = ::drm::control::from_u32(raw_crtc).unwrap();
        output
    }

    #[test]
    fn initial_scanout_rollback_guard_fires_and_can_be_disarmed() {
        let mut platform = PlatformBackend::for_tests();
        let calls = Rc::new(Cell::new(0_u32));

        {
            let calls = Rc::clone(&calls);
            let _guard = InitialScanoutRollbackGuard::new_with(
                &platform.devices,
                &mut platform.outputs,
                move |_device, _output| {
                    calls.set(calls.get() + 1);
                    Ok(())
                },
            );
        }
        assert_eq!(calls.get(), 1, "armed guard must run rollback on drop");

        {
            let calls = Rc::clone(&calls);
            let mut guard = InitialScanoutRollbackGuard::new_with(
                &platform.devices,
                &mut platform.outputs,
                move |_device, _output| {
                    calls.set(calls.get() + 1);
                    Ok(())
                },
            );
            guard.disarm();
        }
        assert_eq!(calls.get(), 1, "disarmed guard must not roll back");
    }

    #[test]
    fn zero_device_platform_has_no_drm_poll_source_or_rescan_work() {
        let mut platform = PlatformBackend::for_tests();
        platform.devices.clear();
        platform.outputs.clear();

        assert!(platform.primary_device().is_none());
        assert!(
            platform
                .poll_fds()
                .iter()
                .all(|(_, kind)| !matches!(kind, BackendFdKind::Drm)),
            "a zero-device platform must not register a DRM poll source"
        );

        let snapshot = platform
            .probe_connector_snapshot()
            .expect("zero-device probe is an empty no-op");
        let rescan = platform.apply_connector_snapshot(snapshot, &HashSet::new(), &HashSet::new());
        assert!(rescan.added_keys.is_empty());
        assert!(rescan.dropped_keys.is_empty());
        assert!(rescan.dropped_old_indices.is_empty());
        assert_eq!(rescan.added_count, 0);
    }

    #[test]
    fn old_event_drain_accepts_an_empty_zero_device_epoch() {
        let mut platform = PlatformBackend::for_tests();
        platform.devices.clear();

        platform
            .discard_old_drm_events_after_all_off(&HashSet::new(), std::time::Duration::ZERO)
            .expect("an empty topology epoch has no DRM event to retire");
    }

    #[test]
    fn old_event_drain_rejects_pending_work_without_an_owning_device() {
        let mut platform = PlatformBackend::for_tests();
        let pending = HashSet::from([CrtcKey::for_output(&platform.outputs[0])]);
        platform.devices.clear();

        let error = platform
            .discard_old_drm_events_after_all_off(&pending, std::time::Duration::ZERO)
            .expect_err("pending work cannot be proven retired without its DRM fd");
        assert!(matches!(
            error.kind(),
            io::ErrorKind::TimedOut | io::ErrorKind::NotFound
        ));
    }

    #[test]
    fn connector_snapshot_refreshes_live_metadata_without_changing_its_route() {
        let mut platform = PlatformBackend::for_tests();
        let key = platform.outputs[0].key.clone();
        let old_crtc = platform.outputs[0].output.crtc;
        let old_plane = platform.outputs[0].output.plane;
        let snapshot = ConnectorSnapshot {
            key: key.clone(),
            modes: vec![
                crate::platform::drm::Mode {
                    name: "800x600".into(),
                    width: 800,
                    height: 600,
                    vrefresh: 60,
                    preferred: true,
                    ..Default::default()
                },
                crate::platform::drm::Mode {
                    name: "1024x768".into(),
                    width: 1024,
                    height: 768,
                    vrefresh: 75,
                    preferred: false,
                    ..Default::default()
                },
            ],
            mm_width: 520,
            mm_height: 290,
            edid: vec![1, 2, 3, 4],
            connector_type: "DisplayPort".into(),
        };
        let known_connected = HashSet::from([key]);

        let rescan =
            platform.apply_connector_snapshot(vec![snapshot], &HashSet::new(), &known_connected);

        assert!(rescan.dropped_old_indices.is_empty());
        let output = &platform.outputs[0].output;
        assert_eq!(output.crtc, old_crtc);
        assert_eq!(output.plane, old_plane);
        assert_eq!((output.mm_width, output.mm_height), (520, 290));
        assert_eq!(output.edid, vec![1, 2, 3, 4]);
        assert_eq!(output.connector_type, "DisplayPort");
        assert_eq!(output.modes[1].vrefresh, 75);
    }

    #[test]
    fn metadata_only_snapshot_preserves_active_layout_and_extent() {
        let mut platform = PlatformBackend::for_tests();
        let key = platform.outputs[0].key.clone();
        platform.outputs[0].x = 123;
        platform.outputs[0].y = 45;
        platform.fb_w = 923;
        platform.fb_h = 645;
        let snapshot = ConnectorSnapshot {
            key: key.clone(),
            modes: platform.outputs[0].output.modes.clone(),
            mm_width: 520,
            mm_height: 290,
            edid: vec![1, 2, 3, 4],
            connector_type: "DisplayPort".into(),
        };

        let rescan = platform.apply_connector_snapshot(
            vec![snapshot],
            &HashSet::new(),
            &HashSet::from([key]),
        );

        assert!(rescan.dropped_old_indices.is_empty());
        assert_eq!((platform.outputs[0].x, platform.outputs[0].y), (123, 45));
        assert_eq!(platform.fb_dimensions(), (923, 645));
    }

    #[test]
    fn connector_snapshot_preserves_a_live_route_across_mode_list_replacement() {
        let mut platform = PlatformBackend::for_tests();
        let key = platform.outputs[0].key.clone();
        let snapshot = ConnectorSnapshot {
            key: key.clone(),
            modes: vec![crate::platform::drm::Mode {
                name: "1024x768".into(),
                width: 1024,
                height: 768,
                vrefresh: 75,
                preferred: true,
                ..Default::default()
            }],
            mm_width: 520,
            mm_height: 290,
            edid: vec![1, 2, 3, 4],
            connector_type: "DisplayPort".into(),
        };

        let rescan = platform.apply_connector_snapshot(
            vec![snapshot],
            &HashSet::new(),
            &HashSet::from([key.clone()]),
        );

        assert_eq!(platform.outputs.len(), 1);
        assert!(rescan.dropped_old_indices.is_empty());
        assert!(rescan.dropped_keys.is_empty());
        assert_eq!(
            rescan.connected.len(),
            1,
            "the connector remains physically connected"
        );
    }

    #[test]
    fn crtc_identity_and_present_clocks_include_the_drm_device() {
        let mut platform = PlatformBackend::for_tests();
        let live = CrtcKey::for_output(&platform.outputs[0]);
        let colliding = CrtcKey::new(
            crate::platform::drm::DrmDeviceKey {
                major: live.device_key.major,
                minor: live.device_key.minor + 1,
            },
            live.crtc,
        );

        assert_ne!(live, colliding);
        assert_eq!(platform.output_index_for_crtc(live), Some(0));
        assert_eq!(platform.output_index_for_crtc(colliding), None);

        for key in [live, colliding] {
            platform.ust_msc.insert(key, (7, 11));
            platform.completion_clocks.insert(
                key,
                PresentClockSample {
                    msc: 7,
                    ust: 11,
                    source: PresentClockSource::PageFlip,
                },
            );
            platform.software_msc.insert(key, 7);
        }

        platform.prune_present_clocks_to_live_outputs();
        assert_eq!(platform.ust_msc.keys().copied().collect::<Vec<_>>(), [live]);
        assert_eq!(
            platform
                .completion_clocks
                .keys()
                .copied()
                .collect::<Vec<_>>(),
            [live]
        );
        assert_eq!(
            platform.software_msc.keys().copied().collect::<Vec<_>>(),
            [live]
        );
    }

    #[test]
    fn cursor_route_qualifies_colliding_raw_crtcs_by_device() {
        let mut platform = PlatformBackend::for_tests();
        let raw_crtc = platform.outputs[0].output.crtc;
        let second_key = crate::platform::drm::DrmDeviceKey {
            major: 226,
            minor: 9,
        };
        platform.devices.push(KmsDevice {
            key: second_key,
            device: Rc::new(drm::Device::for_tests().expect("second test DRM device")),
            cursor: KmsCursorState::new(false),
        });
        platform.outputs[0].key.device_key = second_key;

        assert_eq!(platform.outputs[0].output.crtc, raw_crtc);
        assert_eq!(platform.cursor_output_route(0).unwrap().0, 1);
    }

    #[test]
    fn drm_device_fd_lookup_distinguishes_same_kind_poll_sources() {
        let mut platform = PlatformBackend::for_tests();
        let first_fd = platform.devices[0].device.as_fd().as_raw_fd();
        let second_device = Rc::new(drm::Device::for_tests().expect("second test DRM device"));
        let second_fd = second_device.as_fd().as_raw_fd();
        platform.devices.push(KmsDevice {
            key: crate::platform::drm::DrmDeviceKey {
                major: 226,
                minor: 1,
            },
            device: second_device,
            cursor: KmsCursorState::new(false),
        });

        assert_ne!(first_fd, second_fd);
        assert_eq!(platform.drm_device_index_for_fd(first_fd), Some(0));
        assert_eq!(platform.drm_device_index_for_fd(second_fd), Some(1));
        assert_eq!(platform.drm_device_index_for_fd(-1), None);
    }

    /// A connected output that yielded no scanout pool must abort
    /// bring-up rather than run invisibly (the RPi 4/400 split-GPU
    /// black-screen case).
    #[test]
    fn one_output_no_live_pool_refuses() {
        let err = check_scanout_liveness(1, 0, &["output 0 (1360x768): boom".to_string()])
            .expect_err("1 output, 0 live pools must refuse");
        assert!(err.contains("no displayable output"), "message: {err}");
        assert!(
            err.contains("boom"),
            "must surface the per-output error: {err}"
        );
    }

    /// A working output proceeds.
    #[test]
    fn one_output_one_live_pool_proceeds() {
        assert!(check_scanout_liveness(1, 1, &[]).is_ok());
    }

    /// Partial success (one of two outputs live) proceeds on the good one.
    #[test]
    fn partial_liveness_proceeds() {
        assert!(check_scanout_liveness(2, 1, &["output 1: nope".to_string()]).is_ok());
    }

    /// Zero outputs is a headless start, not a failure — runtime hotplug
    /// may attach a display later.
    #[test]
    fn zero_outputs_is_not_fatal() {
        assert!(check_scanout_liveness(0, 0, &[]).is_ok());
    }

    /// HW-cursor auto-fallback policy: an ioctl error that means the
    /// driver doesn't implement the (legacy) cursor ioctls must latch
    /// the strategy off so the scene falls back to the SW cursor.
    /// Apple's DCP driver (Asahi) returns `ENXIO`; some drivers return
    /// `ENODEV` / `EOPNOTSUPP`. Recoverable errors (`EBUSY`) must NOT
    /// latch — those are transient and latching would needlessly kill
    /// the HW cursor on drivers that DO support it (e.g. amdgpu).
    #[test]
    fn cursor_err_disables_hw_only_for_unsupported_errnos() {
        use std::io::Error;
        // Asahi / Apple DCP: legacy cursor ioctl unimplemented.
        assert!(cursor_err_disables_hw(&Error::from_raw_os_error(
            libc::ENXIO
        )));
        assert!(cursor_err_disables_hw(&Error::from_raw_os_error(
            libc::ENODEV
        )));
        assert!(cursor_err_disables_hw(&Error::from_raw_os_error(
            libc::EOPNOTSUPP
        )));
        // Transient / recoverable: must keep the HW path alive.
        assert!(!cursor_err_disables_hw(&Error::from_raw_os_error(
            libc::EBUSY
        )));
        // Generic / ambiguous: don't latch on these either.
        assert!(!cursor_err_disables_hw(&Error::from_raw_os_error(
            libc::EINVAL
        )));
        assert!(!cursor_err_disables_hw(&Error::other("not an os error")));
    }

    #[test]
    fn cursor_failure_pair_uses_permanent_precedence_and_clears_pending() {
        let crtc = ::drm::control::from_u32(17).unwrap();

        let mut transient = KmsCursorState::new(false);
        transient.pending_move = Some((1, 2, 3, 4));
        assert_eq!(
            transient.note_cursor_failure_pair(
                crtc,
                &io::Error::from_raw_os_error(libc::EINVAL),
                Some(&io::Error::from_raw_os_error(libc::EIO)),
            ),
            CursorFailureDisposition::Transient
        );
        assert!(!transient.permanently_disabled);
        assert_eq!(transient.pending_move, None);
        assert_eq!(
            transient.transient_fallback_crtcs[&crtc].remaining_sw_retires, 1,
            "one operation+rollback pair records exactly one failure"
        );

        for (operation_errno, rollback_errno) in
            [(libc::EINVAL, libc::ENODEV), (libc::ENODEV, libc::EINVAL)]
        {
            let mut permanent = KmsCursorState::new(false);
            permanent.pending_move = Some((1, 2, 3, 4));
            assert_eq!(
                permanent.note_cursor_failure_pair(
                    crtc,
                    &io::Error::from_raw_os_error(operation_errno),
                    Some(&io::Error::from_raw_os_error(rollback_errno)),
                ),
                CursorFailureDisposition::Permanent
            );
            assert!(permanent.permanently_disabled);
            assert_eq!(permanent.pending_move, None);
            assert!(permanent.transient_fallback_crtcs.is_empty());
        }

        let mut unchanged = KmsCursorState::new(false);
        unchanged.pending_move = Some((1, 2, 3, 4));
        assert_eq!(
            unchanged.note_cursor_failure_pair(
                crtc,
                &io::Error::from_raw_os_error(libc::EBUSY),
                Some(&io::Error::from_raw_os_error(libc::EIO)),
            ),
            CursorFailureDisposition::Unchanged
        );
        assert_eq!(unchanged.pending_move, Some((1, 2, 3, 4)));
        assert!(unchanged.transient_fallback_crtcs.is_empty());
    }

    #[test]
    fn still_visible_show_failure_records_owning_fallback_without_stale_move() {
        let crtc = ::drm::control::from_u32(18).unwrap();
        let mut transient = KmsCursorState::new(false);
        transient.pending_move = Some((1, 2, 3, 4));
        let bind_einval = crate::kms::cursor_plane::CursorShowError::StillVisible {
            operation_error: io::Error::from_raw_os_error(libc::EINVAL),
            rollback_error: None,
        };
        assert_eq!(
            apply_cursor_show_failure_state(&mut transient, crtc, &bind_einval, (10, 20, 5, 6),),
            CursorFailureDisposition::Transient
        );
        assert_eq!(transient.pending_move, None);
        assert_eq!(
            transient.transient_fallback_crtcs[&crtc].remaining_sw_retires,
            1
        );

        let mut unsupported = KmsCursorState::new(false);
        let bind_enodev = crate::kms::cursor_plane::CursorShowError::StillVisible {
            operation_error: io::Error::from_raw_os_error(libc::ENODEV),
            rollback_error: None,
        };
        assert_eq!(
            apply_cursor_show_failure_state(&mut unsupported, crtc, &bind_enodev, (10, 20, 5, 6),),
            CursorFailureDisposition::Permanent
        );
        assert!(unsupported.permanently_disabled);

        let mut rollback_wins = KmsCursorState::new(false);
        let move_einval_hide_enodev = crate::kms::cursor_plane::CursorShowError::StillVisible {
            operation_error: io::Error::from_raw_os_error(libc::EINVAL),
            rollback_error: Some(io::Error::from_raw_os_error(libc::ENODEV)),
        };
        assert_eq!(
            apply_cursor_show_failure_state(
                &mut rollback_wins,
                crtc,
                &move_einval_hide_enodev,
                (10, 20, 5, 6),
            ),
            CursorFailureDisposition::Permanent
        );
        assert!(rollback_wins.permanently_disabled);
        assert!(rollback_wins.transient_fallback_crtcs.is_empty());
        assert_eq!(rollback_wins.pending_move, None);
    }

    #[test]
    fn hide_failure_classification_is_bounded_and_success_does_not_clear_it() {
        let crtc = ::drm::control::from_u32(19).unwrap();
        let mut state = KmsCursorState::new(false);
        let einval = Err(io::Error::from_raw_os_error(libc::EINVAL));
        assert_eq!(
            apply_cursor_operation_result(&mut state, crtc, &einval),
            CursorFailureDisposition::Transient
        );
        assert_eq!(
            state.transient_fallback_crtcs[&crtc].remaining_sw_retires,
            1
        );
        assert_eq!(
            apply_cursor_operation_result(&mut state, crtc, &einval),
            CursorFailureDisposition::Transient
        );
        assert_eq!(
            state.transient_fallback_crtcs[&crtc].remaining_sw_retires,
            2
        );

        assert_eq!(
            apply_cursor_operation_result(&mut state, crtc, &Ok(())),
            CursorFailureDisposition::Unchanged
        );
        assert_eq!(
            state.transient_fallback_crtcs[&crtc].remaining_sw_retires, 2,
            "a successful hide is not proof that a later full Show is valid"
        );

        let other_crtc = ::drm::control::from_u32(20).unwrap();
        assert_eq!(
            apply_cursor_operation_result(
                &mut state,
                other_crtc,
                &Err(io::Error::from_raw_os_error(libc::EBUSY)),
            ),
            CursorFailureDisposition::Unchanged
        );
        assert!(!state.transient_fallback_crtcs.contains_key(&other_crtc));

        assert_eq!(
            apply_cursor_operation_result(
                &mut state,
                crtc,
                &Err(io::Error::from_raw_os_error(libc::ENODEV)),
            ),
            CursorFailureDisposition::Permanent
        );
        assert!(state.permanently_disabled);
        assert!(state.transient_fallback_crtcs.is_empty());
    }

    #[test]
    fn actual_upload_invalid_input_enters_one_bounded_local_fallback() {
        let crtc = ::drm::control::from_u32(21).unwrap();
        let mut state = KmsCursorState::new(false);
        let upload = Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cursor bytes shorter than width*height*4",
        ));
        assert_eq!(
            apply_cursor_operation_result(&mut state, crtc, &upload),
            CursorFailureDisposition::Transient
        );
        assert_eq!(state.transient_fallback_crtcs.len(), 1);
        assert_eq!(
            state.transient_fallback_crtcs[&crtc].remaining_sw_retires,
            1
        );
        assert!(!state.permanently_disabled);
    }

    #[test]
    fn cursor_failure_classification_isolated_across_cards_with_same_raw_crtc() {
        let crtc = ::drm::control::from_u32(22).unwrap();
        let mut card_a = KmsCursorState::new(false);
        let card_b = KmsCursorState::new(false);
        card_a.note_cursor_failure_pair(crtc, &io::Error::from_raw_os_error(libc::EINVAL), None);
        assert!(card_a.transient_fallback_crtcs.contains_key(&crtc));
        assert!(card_b.transient_fallback_crtcs.is_empty());
        assert!(!card_b.permanently_disabled);

        card_a.note_cursor_failure_pair(crtc, &io::Error::from_raw_os_error(libc::ENODEV), None);
        assert!(card_a.permanently_disabled);
        assert!(card_a.transient_fallback_crtcs.is_empty());
        assert!(card_b.transient_fallback_crtcs.is_empty());
        assert!(!card_b.permanently_disabled);
    }

    #[test]
    fn active_startup_transient_init_retries_only_at_explicit_active_boundary() {
        let mut state = KmsCursorState::new(false);
        assert!(
            !state.should_retry_initialization(true),
            "a genuinely headless-deferred device is not a lifecycle retry"
        );
        assert!(state.should_initialize_headless_deferred(true));

        state.note_initialization_failure(&io::Error::from_raw_os_error(libc::ENOMEM));
        assert!(!state.headless_deferred);
        assert!(!state.permanently_disabled);
        assert!(!state.should_retry_initialization(false));
        assert!(state.should_retry_initialization(true));

        state.note_initialization_failure(&io::Error::from_raw_os_error(libc::ENODEV));
        assert!(state.permanently_disabled);
        assert!(!state.should_retry_initialization(true));
    }

    #[test]
    fn primary_first_explicit_enable_initializes_and_exposes_fresh_upload_state() {
        let mut platform = PlatformBackend::for_tests();
        let key = platform.devices[0].key;
        let crtc = platform.outputs[0].output.crtc;
        assert!(platform.devices[0].cursor.headless_deferred);

        let calls = Cell::new(0_u32);
        assert!(platform.initialize_headless_cursor_for_device_with(
            key,
            "test primary first enable",
            |device, crtcs, boundary| {
                calls.set(calls.get() + 1);
                assert_eq!(device.key, key);
                assert_eq!(crtcs, &[crtc]);
                assert_eq!(boundary, "test primary first enable");
                install_test_cursor_plane(device, crtcs, boundary);
            },
        ));
        assert_eq!(calls.get(), 1);
        assert!(!platform.devices[0].cursor.headless_deferred);
        assert!(platform.cursor_plane_available_for_output(0));
        assert_eq!(platform.cursor_plane_uploaded_version_for_output(0), None);

        let bytes = vec![0_u8; 16 * 16 * 4];
        platform
            .cursor_plane_upload_image_for_output(0, 7, 16, 16, &bytes)
            .expect("fresh lazy plane accepts its first retire-time upload");
        assert_eq!(
            platform.cursor_plane_uploaded_version_for_output(0),
            Some(7)
        );
    }

    #[test]
    fn secondary_first_enable_uses_owning_device_despite_raw_crtc_collision() {
        let mut platform = PlatformBackend::for_tests();
        let primary_key = platform.devices[0].key;
        let raw_crtc = platform.outputs[0].output.crtc;
        let secondary_key = crate::platform::drm::DrmDeviceKey {
            major: 226,
            minor: 93,
        };
        platform.devices.push(test_kms_device(secondary_key, false));
        let primary_fd = platform.devices[0].device.as_fd().as_raw_fd();
        let secondary_fd = platform.devices[1].device.as_fd().as_raw_fd();
        platform.outputs.push(test_active_output_for(
            secondary_key,
            "secondary",
            u32::from(raw_crtc),
        ));

        let calls = Cell::new(0_u32);
        assert!(platform.initialize_headless_cursor_for_device_with(
            secondary_key,
            "test secondary first enable",
            |device, crtcs, boundary| {
                calls.set(calls.get() + 1);
                assert_eq!(device.key, secondary_key);
                assert_eq!(device.device.as_fd().as_raw_fd(), secondary_fd);
                assert_ne!(device.device.as_fd().as_raw_fd(), primary_fd);
                assert_eq!(crtcs, &[raw_crtc]);
                install_test_cursor_plane(device, crtcs, boundary);
            },
        ));

        assert_eq!(calls.get(), 1);
        assert_eq!(platform.devices[0].key, primary_key);
        assert!(platform.devices[0].cursor.plane.is_none());
        assert!(platform.devices[0].cursor.headless_deferred);
        assert!(platform.devices[1].cursor.plane.is_some());
        assert!(platform.cursor_plane_available_for_output(1));
        assert!(!platform.cursor_plane_available_for_output(0));
    }

    #[test]
    fn failed_enable_never_reaches_deferred_cursor_factory() {
        let mut platform = PlatformBackend::for_tests();
        let active = platform.outputs.remove(0);
        let output_key = active.key;
        let output = active.output;
        platform.scanout_pools.clear();
        platform.bo_generations.clear();
        platform.first_pageflip_logged.clear();
        let mode = yserver_core::backend::ModeSpec {
            width: output.picked.width,
            height: output.picked.height,
            vrefresh: output.picked.vrefresh,
        };
        let calls = Cell::new(0_u32);

        let result = platform.enable_connector_with_cursor_factory(
            &output_key,
            output,
            mode,
            0,
            0,
            |_device, _crtcs, _boundary| calls.set(calls.get() + 1),
        );

        assert!(result.is_err(), "test fixture has no initial fb handle");
        assert_eq!(calls.get(), 0);
        assert!(platform.outputs.is_empty());
        assert!(platform.devices[0].cursor.headless_deferred);
    }

    #[test]
    fn deferred_init_failure_policy_is_device_local_and_retries_later_only() {
        let mut platform = PlatformBackend::for_tests();
        let primary_key = platform.devices[0].key;
        let secondary_key = crate::platform::drm::DrmDeviceKey {
            major: 226,
            minor: 94,
        };
        platform.devices.push(test_kms_device(secondary_key, false));
        platform
            .outputs
            .push(test_active_output_for(secondary_key, "secondary", 2));

        let first_calls = Cell::new(0_u32);
        assert!(platform.initialize_headless_cursor_for_device_with(
            secondary_key,
            "first explicit enable",
            |device, _crtcs, _boundary| {
                first_calls.set(first_calls.get() + 1);
                device
                    .cursor
                    .note_initialization_failure(&io::Error::from_raw_os_error(libc::ENOMEM));
            },
        ));
        assert_eq!(first_calls.get(), 1);
        assert!(!platform.devices[1].cursor.headless_deferred);
        assert!(platform.devices[1].cursor.initialization_retryable);
        assert!(!platform.devices[1].cursor.permanently_disabled);
        assert!(!platform.initialize_headless_cursor_for_device_with(
            secondary_key,
            "same boundary must not retry",
            |_device, _crtcs, _boundary| first_calls.set(first_calls.get() + 1),
        ));
        assert_eq!(first_calls.get(), 1);

        let retry_calls = Cell::new(0_u32);
        platform.refresh_cursor_topology_for_devices_with(
            &HashSet::from([secondary_key]),
            |device, crtcs, boundary| {
                retry_calls.set(retry_calls.get() + 1);
                assert_eq!(device.key, secondary_key);
                assert_eq!(boundary, "lifecycle retry");
                install_test_cursor_plane(device, crtcs, boundary);
            },
        );
        assert_eq!(retry_calls.get(), 1);
        assert!(platform.devices[1].cursor.plane.is_some());
        assert!(!platform.devices[1].cursor.initialization_retryable);
        assert!(platform.devices[0].cursor.headless_deferred);
        assert!(!platform.devices[0].cursor.permanently_disabled);

        // A separate card's permanent failure latches only that owner.
        let tertiary_key = crate::platform::drm::DrmDeviceKey {
            major: 226,
            minor: 95,
        };
        platform.devices.push(test_kms_device(tertiary_key, false));
        platform
            .outputs
            .push(test_active_output_for(tertiary_key, "tertiary", 3));
        assert!(platform.initialize_headless_cursor_for_device_with(
            tertiary_key,
            "first explicit enable",
            |device, _crtcs, _boundary| {
                device
                    .cursor
                    .note_initialization_failure(&io::Error::from_raw_os_error(libc::ENODEV));
            },
        ));
        assert!(platform.devices[2].cursor.permanently_disabled);
        assert!(!platform.devices[2].cursor.initialization_retryable);
        assert!(platform.devices[2].cursor.plane.is_none());
        assert!(platform.devices[1].cursor.plane.is_some());
        assert!(!platform.devices[1].cursor.permanently_disabled);
        assert_eq!(platform.devices[0].key, primary_key);
    }

    #[test]
    fn initialized_cursor_plane_persists_but_reuploads_across_last_disable_and_reenable() {
        let mut platform = PlatformBackend::for_tests();
        let key = platform.devices[0].key;
        assert!(platform.initialize_headless_cursor_for_device_with(
            key,
            "first explicit enable",
            install_test_cursor_plane,
        ));
        platform
            .cursor_plane_upload_image_for_output(0, 9, 16, 16, &[0_u8; 16 * 16 * 4])
            .unwrap();
        assert_eq!(
            platform.devices[0]
                .cursor
                .plane
                .as_ref()
                .and_then(crate::kms::cursor_plane::CursorPlane::uploaded_version),
            Some(9)
        );

        platform
            .cursor_plane_hide_all()
            .expect("topology quiesce retains the plane while invalidating its upload");
        assert_eq!(
            platform.devices[0]
                .cursor
                .plane
                .as_ref()
                .and_then(crate::kms::cursor_plane::CursorPlane::uploaded_version),
            None
        );

        platform.remove_connector_at(0);
        assert!(platform.outputs.is_empty());
        assert!(platform.devices[0].cursor.plane.is_some());
        assert!(!platform.devices[0].cursor.headless_deferred);
        assert_eq!(
            platform.devices[0]
                .cursor
                .plane
                .as_ref()
                .and_then(crate::kms::cursor_plane::CursorPlane::uploaded_version),
            None,
            "last-output removal retains the allocation, not stale pixels"
        );

        platform
            .outputs
            .push(test_active_output_for(key, "reenabled", 1));
        platform.refresh_cursor_topology_for_devices(&HashSet::from([key]));
        assert!(platform.cursor_plane_available_for_output(0));
        assert_eq!(platform.cursor_plane_uploaded_version_for_output(0), None);
        platform
            .cursor_plane_upload_image_for_output(0, 10, 16, 16, &[0_u8; 16 * 16 * 4])
            .expect("first retirement after re-enable refreshes retained storage");
        assert_eq!(
            platform.cursor_plane_uploaded_version_for_output(0),
            Some(10)
        );
    }

    #[test]
    fn connected_off_probe_and_zero_card_do_not_run_deferred_factory() {
        let mut platform = PlatformBackend::for_tests();
        let key = platform.outputs[0].key.clone();
        platform.outputs.clear();
        platform.scanout_pools.clear();
        platform.bo_generations.clear();
        platform.first_pageflip_logged.clear();
        let snapshot = ConnectorSnapshot {
            key: key.clone(),
            modes: vec![crate::platform::drm::Mode {
                name: "800x600".into(),
                width: 800,
                height: 600,
                vrefresh: 60,
                preferred: true,
                ..Default::default()
            }],
            mm_width: 520,
            mm_height: 290,
            edid: vec![1, 2, 3, 4],
            connector_type: "DisplayPort".into(),
        };

        let rescan =
            platform.apply_connector_snapshot(vec![snapshot], &HashSet::new(), &HashSet::new());
        assert_eq!(rescan.added_keys, vec![key.clone()]);
        assert!(platform.outputs.is_empty());
        assert!(platform.devices[0].cursor.headless_deferred);
        assert!(platform.devices[0].cursor.plane.is_none());

        let calls = Cell::new(0_u32);
        platform.devices.clear();
        assert!(!platform.initialize_headless_cursor_for_device_with(
            key.device_key,
            "zero-card",
            |_device, _crtcs, _boundary| calls.set(calls.get() + 1),
        ));
        assert_eq!(calls.get(), 0);
    }

    #[test]
    fn cursor_capacity_is_evaluated_per_card() {
        assert!(cursor_dimensions_fit(128, 128, 96, 96));
        assert!(!cursor_dimensions_fit(64, 64, 96, 96));
    }

    #[test]
    fn nvidia_policy_follows_output_owner_not_device_order() {
        let mut platform = PlatformBackend::for_tests();
        let mesa_key = platform.devices[0].key;
        let nvidia_key = crate::platform::drm::DrmDeviceKey {
            major: 226,
            minor: 77,
        };
        platform.devices.push(test_kms_device(nvidia_key, true));

        let policy_disabled = |platform: &PlatformBackend| {
            let output = &platform.outputs[0];
            platform
                .device_for_key(output.key.device_key)
                .unwrap()
                .cursor
                .nvidia_policy_disabled
        };
        platform.outputs[0].key.device_key = mesa_key;
        assert!(!policy_disabled(&platform));
        platform.outputs[0].key.device_key = nvidia_key;
        assert!(policy_disabled(&platform));

        platform.devices.swap(0, 1);
        assert!(policy_disabled(&platform));
        platform.outputs[0].key.device_key = mesa_key;
        assert!(!policy_disabled(&platform));
    }

    #[test]
    fn topology_refresh_on_card_b_preserves_card_a_cursor_retry_state() {
        let mut platform = PlatformBackend::for_tests();
        let card_a = platform.devices[0].key;
        let raw_crtc = platform.outputs[0].output.crtc;
        let card_b = crate::platform::drm::DrmDeviceKey {
            major: 226,
            minor: 88,
        };
        platform.devices.push(test_kms_device(card_b, false));
        platform.devices[0].cursor.pending_move = Some((100, 200, 3, 4));
        platform.devices[0].cursor.note_einval(raw_crtc);
        platform.devices[1].cursor.pending_move = Some((300, 400, 5, 6));
        platform.devices[1].cursor.note_einval(raw_crtc);

        platform.refresh_cursor_topology_for_devices(&HashSet::from([card_b]));

        assert_eq!(platform.devices[0].key, card_a);
        assert_eq!(
            platform.devices[0].cursor.pending_move,
            Some((100, 200, 3, 4))
        );
        assert_eq!(
            platform.devices[0]
                .cursor
                .transient_fallback_crtcs
                .get(&raw_crtc)
                .map(|retry| retry.remaining_sw_retires),
            Some(1)
        );
        assert_eq!(platform.devices[1].cursor.pending_move, None);
        assert!(
            platform.devices[1]
                .cursor
                .transient_fallback_crtcs
                .is_empty()
        );
    }

    #[test]
    fn einval_backoff_advances_only_on_owning_output_retirements() {
        let mut platform = PlatformBackend::for_tests();
        let raw_crtc = platform.outputs[0].output.crtc;
        let card_b = crate::platform::drm::DrmDeviceKey {
            major: 226,
            minor: 89,
        };
        platform.devices.push(test_kms_device(card_b, false));
        platform.devices[0].cursor.note_einval(raw_crtc);
        platform.devices[1].cursor.note_einval(raw_crtc);

        assert!(platform.cursor_plane_note_composed_retirement(0));
        assert_eq!(
            platform.devices[0].cursor.transient_fallback_crtcs[&raw_crtc].remaining_sw_retires,
            0
        );
        assert_eq!(
            platform.devices[1].cursor.transient_fallback_crtcs[&raw_crtc].remaining_sw_retires,
            1
        );

        platform.devices[0].cursor.note_einval(raw_crtc);
        assert_eq!(
            platform.devices[0].cursor.transient_fallback_crtcs[&raw_crtc].remaining_sw_retires, 2,
            "a repeated EINVAL is rate-limited for two own-card SW retirements"
        );
    }

    #[test]
    fn failed_move_hide_rollback_records_fallback_and_scene_owned_retry() {
        let crtc = ::drm::control::from_u32(17).unwrap();
        let mut state = KmsCursorState::new(false);
        state.pending_move = Some((1, 2, 3, 4));
        let mut outcome = CursorMoveOutcome::default();
        let keep_pending = apply_cursor_move_rollback_result(
            &mut state,
            crtc,
            &io::Error::from_raw_os_error(libc::EINVAL),
            Err(io::Error::from_raw_os_error(libc::EIO)),
            &mut outcome,
        );

        assert!(!keep_pending);
        assert!(outcome.retry_required);
        assert!(outcome.fallback_changed);
        assert!(!state.permanently_disabled);
        assert_eq!(state.pending_move, None);
        assert_eq!(
            state.transient_fallback_crtcs[&crtc].remaining_sw_retires,
            1
        );
    }

    #[test]
    fn failed_move_hide_rollback_uses_permanent_precedence() {
        let crtc = ::drm::control::from_u32(23).unwrap();
        for (move_errno, hide_errno) in [(libc::EINVAL, libc::ENODEV), (libc::ENODEV, libc::EINVAL)]
        {
            let mut state = KmsCursorState::new(false);
            state.pending_move = Some((1, 2, 3, 4));
            let mut outcome = CursorMoveOutcome::default();
            let keep_pending = apply_cursor_move_rollback_result(
                &mut state,
                crtc,
                &io::Error::from_raw_os_error(move_errno),
                Err(io::Error::from_raw_os_error(hide_errno)),
                &mut outcome,
            );
            assert!(!keep_pending);
            assert!(outcome.retry_required);
            assert!(outcome.fallback_changed);
            assert!(state.permanently_disabled);
            assert_eq!(state.pending_move, None);
            assert!(state.transient_fallback_crtcs.is_empty());
        }
    }

    #[test]
    fn cursor_move_outcome_merge_keeps_cross_device_retry_liveness() {
        let mut aggregate = CursorMoveOutcome {
            ebusy_count: 2,
            fallback_changed: false,
            retry_required: false,
        };
        aggregate.merge(CursorMoveOutcome {
            ebusy_count: 3,
            fallback_changed: true,
            retry_required: true,
        });
        assert_eq!(aggregate.ebusy_count, 5);
        assert!(aggregate.fallback_changed);
        assert!(aggregate.retry_required);
    }

    /// Once a show/bind fails with an unsupported errno, the plane is
    /// no longer reported available, so `tick_one_output`'s `hw_can_run`
    /// gate closes and `build_scene` collapses every assignment to SW.
    /// The latch is sticky across subsequent queries.
    #[test]
    fn unsupported_cursor_failure_latches_plane_unavailable() {
        let mut p = PlatformBackend::for_tests();
        let key = p.devices[0].key;
        let crtc = p.outputs[0].output.crtc;
        assert!(!p.hw_cursor_disabled_for_device(key));
        assert!(p.note_unbound_cursor_failure(
            0,
            crtc,
            &std::io::Error::from_raw_os_error(libc::ENXIO)
        ));
        assert!(p.hw_cursor_disabled_for_device(key));
        assert!(!p.cursor_plane_available());
        assert!(!p.note_unbound_cursor_failure(
            0,
            crtc,
            &std::io::Error::from_raw_os_error(libc::EBUSY)
        ));
        assert!(p.hw_cursor_disabled_for_device(key));
    }

    /// Test fixture works at all: open `for_tests`, query
    /// dimensions, query poll_fds, no Vk required.
    #[test]
    fn for_tests_constructs() {
        let p = PlatformBackend::for_tests();
        assert_eq!(p.fb_dimensions(), (800, 600));
        assert_eq!(p.outputs.len(), 1);
        assert!(p.vk.is_none()); // for_tests skips Vk
        let fds = p.poll_fds();
        // No input_ctx, one DRM fd.
        assert!(fds.iter().any(|(_, k)| matches!(k, BackendFdKind::Drm)));
    }

    #[test]
    fn recompute_fb_extent_matches_issue9_dual_2560x1440() {
        // Side-by-side (y=0): fb = 5120x1440.
        let layouts = &[
            (0i32, 0i32, 2560u16, 1440u16),
            (2560i32, 0i32, 2560u16, 1440u16),
        ];
        assert_eq!(super::recompute_fb_extent_from(layouts), (5120, 1440));
    }

    #[test]
    fn recompute_fb_extent_2d_vertical_stack() {
        // Stacked (second monitor below at y=1440): fb = 2560x2880.
        let layouts = &[
            (0i32, 0i32, 2560u16, 1440u16),
            (0i32, 1440i32, 2560u16, 1440u16),
        ];
        assert_eq!(super::recompute_fb_extent_from(layouts), (2560, 2880));
    }

    /// Fence acquire on a no-Vk fixture returns the
    /// "init failed" error (since fence_pool is None). This
    /// confirms the guard is wired; real fence allocation is
    /// covered by Stage 2c+ Vk-backed tests.
    #[test]
    fn for_tests_fence_acquire_errors_without_vk() {
        let p = PlatformBackend::for_tests();
        let result = p.acquire_fence_ticket();
        assert!(matches!(
            result,
            Err(vk::Result::ERROR_INITIALIZATION_FAILED)
        ));
    }

    /// BO acquire on a no-Vk fixture returns None (the single
    /// stub output has no pool).
    #[test]
    fn for_tests_scanout_acquire_returns_none() {
        let mut p = PlatformBackend::for_tests();
        assert!(p.acquire_scanout_bo(0).is_none());
    }

    /// Pending-move slot is None on a fresh backend and stays None
    /// when the cursor plane is unavailable (the for_tests fixture
    /// has no real DRM device, so `cursor_plane_move` returns Err
    /// and never touches the slot).
    #[test]
    fn cursor_pending_move_starts_empty_and_unavailable_path_does_not_set_it() {
        let mut p = PlatformBackend::for_tests();
        assert_eq!(p.devices[0].cursor.pending_move, None);
        // Unavailable plane → Err return → pending stays None.
        assert!(p.cursor_plane_move(100, 200, 0, 0).is_err());
        assert_eq!(p.devices[0].cursor.pending_move, None);
        // Drain on empty slot is Ok(0) (early-exit before any
        // plane access). The path that returns Err is only the
        // populated-slot retry that hits the unavailable plane —
        // tested separately in `cursor_pending_move_is_latest_wins`.
        assert_eq!(
            p.cursor_plane_drain_pending_move_for_output(0).ok(),
            Some(CursorMoveOutcome::default())
        );
        assert_eq!(p.devices[0].cursor.pending_move, None);
    }

    /// Hide-all clears any pending move (VT-leave invariant).
    #[test]
    fn cursor_plane_hide_all_clears_pending_move() {
        let mut p = PlatformBackend::for_tests();
        p.devices[0].cursor.pending_move = Some((123, 456, 7, 9));
        // hide_all returns Err on the unavailable fixture, but the
        // pending-clear MUST happen before the early-return so a
        // hide-failure mid-recovery leaves no stale pending.
        let _ = p.cursor_plane_hide_all();
        assert_eq!(p.devices[0].cursor.pending_move, None);
    }

    /// Latest-wins: explicitly setting pending then overwriting
    /// reflects the latest position. This is the same in-place mutation
    /// that `cursor_plane_move` does internally on EBUSY — by exercising
    /// it directly (since we can't drive a real EBUSY without a kernel),
    /// we lock in the "old pending is discarded" invariant.
    #[test]
    fn cursor_pending_move_is_latest_wins() {
        let mut p = PlatformBackend::for_tests();
        p.devices[0].cursor.pending_move = Some((100, 100, 1, 2));
        p.devices[0].cursor.pending_move = Some((200, 250, 7, 9));
        assert_eq!(p.devices[0].cursor.pending_move, Some((200, 250, 7, 9)));
        // Drain consumes; on the unavailable fixture this errors but
        // the test's invariant is the slot mechanics, not the drain.
        let _ = p.cursor_plane_drain_pending_move_for_output(0);
        // Slot still holds because drain Err'd before clearing.
        assert_eq!(p.devices[0].cursor.pending_move, Some((200, 250, 7, 9)));
    }

    #[test]
    fn cursor_root_to_crtc_local_subtracts_hotspot() {
        assert_eq!(
            cursor_root_to_crtc_local(200, 300, 10, 20, 7, 9),
            (183, 271)
        );
    }

    /// `cursor_footprint_intersects_output` is the membership rule the
    /// pointer fast path uses to detect a CRTC-boundary crossing
    /// (regression: cursor stayed frozen on screen 1, invisible on
    /// screen 2, once the idle compositor stopped reassigning it).
    /// Modelled on a side-by-side dual-head layout: left [0,0,2560,1440],
    /// right [2560,0,2560,1440], a 64×64 sprite, hotspot (0,0).
    #[test]
    fn cursor_footprint_intersects_output_dual_head_seam() {
        // root-space x relative to each output's origin (hotspot 0).
        let on_left =
            |rx: i32, ry: i32| cursor_footprint_intersects_output(rx, ry, 64, 64, 2560, 1440);
        let on_right = |rx: i32, ry: i32| {
            cursor_footprint_intersects_output(rx - 2560, ry, 64, 64, 2560, 1440)
        };

        // Fully on the left screen.
        assert!(on_left(100, 100));
        assert!(!on_right(100, 100));

        // Fully on the right screen.
        assert!(!on_left(3000, 100));
        assert!(on_right(3000, 100));

        // Straddling the seam — present on BOTH screens (matches the
        // scene clipping a 64px sprite onto both outputs).
        assert!(on_left(2540, 100));
        assert!(on_right(2540, 100));

        // Below both outputs (y past height) — on neither.
        assert!(!on_left(100, 2000));
        assert!(!on_right(3000, 2000));
    }

    /// `invalidate_bo` on a missing entry is a no-op (doesn't
    /// panic). With no pool entries there's nothing to flag,
    /// but the call must remain safe.
    #[test]
    fn for_tests_invalidate_bo_is_noop_on_missing_entry() {
        let mut p = PlatformBackend::for_tests();
        p.invalidate_bo(0, 0); // empty bo_generations[0]
        p.invalidate_bo(99, 0); // out-of-range output_idx
    }

    /// `on_page_flip_complete` without a prior `present_scanout`
    /// is a no-op (no Pending BO to retire).
    #[test]
    fn for_tests_on_page_flip_complete_without_pending_is_none() {
        let mut p = PlatformBackend::for_tests();
        assert!(p.on_page_flip_complete(0).is_none());
    }

    /// `record_present` advances `next_present_generation`
    /// monotonically.
    #[test]
    fn record_present_advances_generation() {
        let mut p = PlatformBackend::for_tests();
        let g1 = p.record_present(0, 0);
        let g2 = p.record_present(0, 0);
        assert_eq!(g1 + 1, g2);
        assert!(g1 > 0); // first generation is 1, not 0
    }

    /// `commit_bo_present` is a no-op on a missing entry, but
    /// the `record_present` counter still advances and survives
    /// a subsequent successful entry write.
    #[test]
    fn commit_bo_present_is_safe_on_missing_entry() {
        let mut p = PlatformBackend::for_tests();
        let g = p.record_present(0, 0);
        p.commit_bo_present(0, 0, g); // bo_generations[0] is empty — no-op
        p.commit_bo_present(99, 99, g); // out-of-range — no-op
    }

    #[test]
    fn platform_starts_with_empty_closed_submit_group() {
        let p = PlatformBackend::for_tests();
        assert!(!p.submit_group_is_open(), "fresh platform has closed group");
        assert_eq!(p.submit_group_size(), 0);
    }

    #[test]
    fn flush_submit_group_empty_is_noop() {
        let mut p = PlatformBackend::for_tests();
        // Fixture has no Vk; should NOT attempt queue_submit2.
        let outcome = p
            .flush_submit_group(FlushReason::SceneCompose)
            .expect("empty-group flush is always Ok");
        assert_eq!(outcome.flushed_entries, 0);
        assert!(!p.submit_group_is_open());
    }

    // ── Task 3 test helpers ──────────────────────────────────────

    #[cfg(test)]
    impl PlatformBackend {
        pub(crate) fn submit_group_max_size_for_tests(&self) -> usize {
            self.submit_group.max_size()
        }

        pub(crate) fn queue_submit2_count_for_tests(&self) -> u64 {
            crate::kms::vk::call_stats::queue_submit2_count()
        }

        pub(crate) fn force_next_submit_failure_for_tests(&mut self) {
            self.force_next_submit_failure = true;
        }
    }

    #[test]
    fn present_completion_epfd_present_at_init_and_poll_fds() {
        // Use the headless fixture — production VkContext init isn't
        // required to exercise the inner-epoll FD.
        let p = PlatformBackend::for_tests();
        let fds = p.poll_fds();
        let present_kind = yserver_core::backend::BackendFdKind::PresentCompletion;
        assert!(
            fds.iter().any(|(_, k)| *k == present_kind),
            "platform.poll_fds() must report a PresentCompletion FD"
        );
        // The FD should be stable: a second call returns the same raw value.
        let raw1 = fds.iter().find(|(_, k)| *k == present_kind).unwrap().0;
        let raw2 = p
            .poll_fds()
            .iter()
            .find(|(_, k)| *k == present_kind)
            .unwrap()
            .0;
        assert_eq!(
            raw1, raw2,
            "the inner epfd is stable across poll_fds() calls"
        );
    }

    /// Mirrors `descriptor_pool_ring::tests::vk_or_skip` — needed
    /// because `VkContext::new()` requires a live Vulkan ICD which
    /// isn't always available in CI.
    fn vk_or_skip() -> Option<Arc<VkContext>> {
        match VkContext::new() {
            Ok(vk) => Some(vk),
            Err(e) => {
                eprintln!("skipping: no Vk: {e:?}");
                None
            }
        }
    }

    /// Regression: `KmsBackend`'s field-drop order runs `platform`
    /// (containing `fence_pool`) BEFORE `store` / `engine` / `scene`,
    /// all of which hold `FenceTicket`s. Pre-fix those tickets
    /// dropped after the pool was gone, `FenceTicketInner::drop`
    /// bailed on `Weak::upgrade() == None`, and leaked every VkFence
    /// handle (1471 leaked at SIGTERM on bee/MATE 2026-05-31, all
    /// `VkFence` per the validation layer's first-10 list). Fix
    /// added a strong `Arc<VkContext>` on `FenceTicketInner` so the
    /// fallback `Drop` path destroys the fence directly. This test
    /// simulates the order bug by dropping the pool first and then
    /// the ticket; it verifies the device is still usable after
    /// (which a leaked-handle path would still allow, but a
    /// use-after-free wouldn't). Validation-layer leak verification
    /// is via the smoke recipe with VK_LAYER_KHRONOS_validation.
    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn fence_ticket_destroys_fence_when_pool_dropped_first() {
        let Some(vk) = vk_or_skip() else { return };
        let pool = FencePool::new(Arc::clone(&vk));
        let ticket = pool.acquire().expect("acquire");

        // Simulate KmsBackend's drop-order bug: pool drops while
        // ticket is still alive (held by store/engine/scene state).
        drop(pool);

        // The ticket's strong Arc<VkContext> + ours keep the device
        // alive. Pre-fix this drop leaked the fence handle; post-fix
        // it calls destroy_fence directly.
        drop(ticket);

        // Device still usable — wait_idle returns Ok and we can
        // create + destroy another fence cleanly.
        unsafe { vk.device.device_wait_idle().expect("wait_idle") };
        let f = unsafe {
            vk.device
                .create_fence(&vk::FenceCreateInfo::default(), None)
                .expect("create_fence")
        };
        unsafe { vk.device.destroy_fence(f, None) };
    }

    /// `for_tests_stub` constructs a `FenceTicket` with no real
    /// device. The fallback `Drop` path must no-op cleanly in that
    /// case (null fence + `vk: None`) and not segfault attempting
    /// to call `destroy_fence` on a null Arc.
    #[test]
    fn for_tests_stub_drops_cleanly_without_vk() {
        let ticket = FenceTicket::for_tests_stub();
        // Drop runs at end of scope; no-op expected.
        drop(ticket);
    }

    /// Imported SYNC_FD wait semaphores must attach to the shared ticket
    /// inner, so every clone observes the same submission-lifetime pins.
    #[test]
    fn fence_ticket_retains_imported_wait_semaphores_across_clones() {
        let ticket = FenceTicket::for_tests_stub();
        let clone = ticket.clone();
        ticket.retain_imported_wait_semaphores(vec![vk::Semaphore::null()]);
        assert_eq!(clone.inner.imported_wait_semaphores.borrow().len(), 1);
    }
}
