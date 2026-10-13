//! `GpuCore`: the GPU-only half of the renderer platform.
//!
//! It owns the Vulkan device handle, the ops command pool, the recyclable
//! fence pool, the pixmap pool and the submit group; `PlatformBackend`
//! holds one as `gpu` next to the DRM/KMS state. This file holds the
//! imports and every type (private fields stay visible to all children);
//! the code lives in:
//!
//! - `init`: constructors.
//! - `fence`: `FenceTicket`, `FencePool`, `PresentCompletionSignal`.
//! - `storage_alloc`: drawable image allocation and views.
//! - `submit`: paint and present submits, the submit group, flush.
//! - `tests`: unit tests.

#![allow(
    dead_code,
    reason = "carried over from platform.rs with the FenceTicket primitives"
)]

mod fence;
mod init;
mod storage_alloc;
mod submit;

#[cfg(test)]
mod tests;

use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
    sync::Arc,
};

use ash::vk;

use crate::kms::{
    render::submit_group::{FlushReason, SubmitGroup},
    vk::{device::VkContext, ops::OpsCommandPool},
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
pub struct FenceTicket {
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
pub struct PresentCompletionSignal {
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

pub struct FencePool {
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
// FlushOutcome
// ────────────────────────────────────────────────────────────────

/// Phase A: result of a `flush_submit_group` call. Same shape on
/// both Ok and Err paths; the `aborted` flag distinguishes them.
/// Task 3.5 hooks the deferred-queue drain that consumes this.
#[derive(Debug, Clone, Copy)]
pub struct FlushOutcome {
    pub flushed_entries: usize,
    pub reason: FlushReason,
    pub aborted: bool,
}

/// The GPU-only half of `PlatformBackend`: the Vulkan device, its
/// command and fence pools, the pixmap pool and the submit group.
///
/// Lifetimes follow these rules, not field order:
/// - Every Vulkan owner holds its own `Arc<VkContext>` (`OpsCommandPool`,
///   `FencePool`, `PixmapPool`, each `FenceTicket` and
///   `PresentCompletionSignal`, the engine, the scanout and copy pools),
///   and `VkContext::drop` idles and destroys the device at the last one,
///   so where `vk` sits here does not matter.
/// - Anything allocated from a pool here and held outside `GpuCore` is
///   released before `GpuCore` drops. The one such owner today is
///   `PlatformBackend::scanout_readback_op` (a command buffer from
///   `ops_command_pool`): `PlatformBackend`'s `Drop` resets it as its first
///   statement, and it is also declared before `gpu`.
/// - `FenceTicket`s may outlive `fence_pool`: a ticket holds a `Weak` pool
///   and a strong `vk`, and destroys its own fence once the pool is gone
///   (`KmsBackend` drops `platform` before `store`, `engine` and `scene`).
/// - `OpsCommandPool::drop` idles the queue, then destroys the pool, which
///   frees the engine's command buffers with it.
///
/// `GpuCore` knows nothing about KMS.
pub struct GpuCore {
    // Vulkan side. `Option` only to support test fixtures that
    // skip Vk init (`for_tests`). Production `open_with_commit`
    // always returns `Some`. v2 has no pixman fallback.
    pub vk: Option<Arc<VkContext>>,
    /// Wrapped in `Option` for the same reason. Lifetimes: see the
    /// struct doc.
    pub ops_command_pool: Option<OpsCommandPool>,
    pub fence_pool: Option<FencePool>,

    /// Stage 3f.10: recycled `(image, view, memory)` triples for
    /// CreatePixmap. Reuses v1's `PixmapPool` verbatim — its
    /// `try_take` / `try_return` API + bucket-cap + size-cap
    /// logic is backend-agnostic. Bypassed by the test fixture
    /// (`for_tests`) and on `for_tests_with_vk` (the harness
    /// constructs `RenderEngine` directly without going through
    /// `open_with_commit`).
    pub pixmap_pool: Option<Arc<crate::kms::vk::pixmap_pool::PixmapPool>>,

    /// Latched on any submit-time / pool-time Vk error. Once
    /// true, the renderer is in a stuck state and the next
    /// composite tick should bail.
    pub renderer_failed: bool,

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
