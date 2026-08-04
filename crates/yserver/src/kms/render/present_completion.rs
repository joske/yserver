//! Deferred PRESENT completion queue (Stage 5 Task 6.1).
//!
//! Owns per-entry state for the v2 backend's `enqueue_present_completion`
//! and `drain_completed_present_events` trait impls. Internal types
//! never escape the `yserver` crate; the trait surface exchanges
//! the public `CompletedPresentEvent` only.
//!
//! Spec: `docs/superpowers/specs/2026-05-23-deferred-present-completion-design.md`.

use std::{os::fd::OwnedFd, sync::Arc, time::Instant};

use yserver_core::backend::{CompletedPresentEvent, SyncobjHandle, XshmfenceHandle};

use crate::kms::render::platform::{FenceTicket, PresentCompletionSignal};

/// One deferred PRESENT completion payload. The drain fires the
/// wake signal via `wake_pin` + returns the `event` payload to the
/// main loop.
#[derive(Debug)]
pub(crate) struct PendingPresentEntry {
    /// Backend-enqueue-to-GPU-completion age. This is the latency that determines
    /// when the client receives its idle signal and can recycle the buffer.
    pub(crate) queued_at: Instant,
    /// Lifetime pin on the underlying wake primitive. Survives a
    /// mid-flight `XFixesDestroyFence` / `FreeSyncobj`.
    pub(crate) wake_pin: PinnedWake,
    /// Public-facing event payload, returned by `drain_*` to the
    /// main loop.
    pub(crate) event: CompletedPresentEvent,
}

/// Readiness primitive for a submitted batch of PRESENT completions.
pub(crate) enum PresentBatchWait {
    /// Linux sync_file fd exported from a dedicated completion
    /// semaphore. This is the hot path.
    Fd(OwnedFd),
    /// Export returned `-1`, meaning already signaled.
    Ready,
    /// Degraded path if fd export fails. Polls `ticket` through
    /// `Backend::next_wakeup`, but should not occur on normal Linux
    /// Vulkan stacks.
    Poll,
}

/// Submitted-but-not-yet-emitted PRESENT completion batch.
pub(crate) struct PendingPresentBatch {
    pub(crate) wait: PresentBatchWait,
    /// Optional internal fence for degraded polling only. The hot fd
    /// path does not need this for readiness.
    pub(crate) ticket: Option<FenceTicket>,
    /// Keeps the dedicated export-only semaphore alive until the
    /// exported sync_file has fired.
    pub(crate) signal: Option<PresentCompletionSignal>,
    pub(crate) events: Vec<PendingPresentEntry>,
}

/// Wake-target lifetime pin variants. The drain dispatches signal
/// via the held `Arc` regardless of whether the X11 resource id is
/// still in the registry.
#[derive(Debug)]
pub(crate) enum PinnedWake {
    Pixmap(Arc<dyn XshmfenceHandle>),
    PixmapSynced {
        handle: Arc<dyn SyncobjHandle>,
        value: u64,
    },
    /// Client passed no wake object (idle_fence_xid == 0 or
    /// release_syncobj == 0). Drain skips the signal step; X11 event
    /// emission still happens.
    None,
}

#[cfg(test)]
mod tests {
    use super::*;
    use yserver_core::backend::PresentWake;
    use yserver_protocol::x11::ClientId;

    /// Smoke test that the types compile + can be constructed.
    /// Real semantics tested in `KmsBackend` integration tests.
    #[test]
    fn pinned_wake_none_constructs() {
        let pin = PinnedWake::None;
        match pin {
            PinnedWake::None => {}
            _ => panic!("unexpected variant"),
        }
    }

    #[test]
    fn completed_present_event_carries_payload() {
        let event = CompletedPresentEvent {
            client_id: ClientId(7),
            serial: 42,
            host_xid: 0x100001,
            dst_host_xid: 0xE00001,
            options: 0,
            present_id: 0,
            wake: PresentWake::Pixmap {
                idle_fence_xid: 0xCC,
            },
            completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
            emit_idle: true,
        };
        assert_eq!(event.serial, 42);
    }
}
