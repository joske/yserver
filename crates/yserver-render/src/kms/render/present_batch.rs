//! Render-side half of the deferred PRESENT completion edge.
//!
//! The engine collects completion entries on the open frame and hands them
//! back as submitted batches. It sees an entry only through
//! [`PresentCompletionEntry`]; the concrete payload (core's event and wake
//! handles) lives in `present_completion`, which recovers it by downcast.

use std::os::fd::OwnedFd;

use crate::kms::render::gpu::{FenceTicket, PresentCompletionSignal};

/// One deferred PRESENT completion, opaque to the renderer.
pub trait PresentCompletionEntry: std::any::Any + std::fmt::Debug {
    /// Publish the submit's GPU completion fence into the entry's release
    /// point. Returns `true` when a release point was published.
    fn publish_release_fence(&mut self, fence: &OwnedFd) -> std::io::Result<bool>;
}

/// Readiness primitive for a submitted batch of PRESENT completions.
pub enum PresentBatchWait {
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
pub struct PendingPresentBatch {
    pub wait: PresentBatchWait,
    /// Optional internal fence for degraded polling only. The hot fd
    /// path does not need this for readiness.
    pub ticket: Option<FenceTicket>,
    /// Keeps the dedicated export-only semaphore alive until the
    /// exported sync_file has fired.
    pub signal: Option<PresentCompletionSignal>,
    pub events: Vec<Box<dyn PresentCompletionEntry>>,
}
