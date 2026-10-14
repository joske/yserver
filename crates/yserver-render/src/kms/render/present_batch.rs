//! Render-side half of the deferred PRESENT completion edge.
//!
//! The engine collects completion entries on the open frame and hands them
//! back as submitted batches. It sees an entry only through
//! [`PresentCompletionEntry`]; the concrete payload (core's event and wake
//! handles) lives in `present_completion`, which recovers it by downcast.

use std::os::fd::OwnedFd;

use ash::vk;

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
    /// Vulkan stacks. The only path on a device without SYNC_FD.
    Poll,
}

impl PresentBatchWait {
    /// Whether the wait itself is satisfied: a readable (or errored)
    /// sync_file, `Ready`, or `Poll` (which defers to the batch ticket).
    #[must_use]
    pub fn signaled(&self) -> bool {
        use std::os::fd::AsRawFd as _;
        match self {
            Self::Ready | Self::Poll => true,
            Self::Fd(fd) => {
                let mut pfd = libc::pollfd {
                    fd: fd.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                match unsafe { libc::poll(&raw mut pfd, 1, 0) } {
                    0 => false,
                    n if n > 0 => pfd.revents & (libc::POLLIN | libc::POLLERR) != 0,
                    _ => {
                        log::warn!(
                            "deferred PRESENT: poll(sync_file fd): {}",
                            std::io::Error::last_os_error()
                        );
                        true
                    }
                }
            }
        }
    }

    /// Wait for a batch whose completion semaphore `export`s a sync_file.
    /// Without SYNC_FD support (`exportable == false`) export is never tried.
    pub fn from_signal_export(
        context: &str,
        exportable: bool,
        export: impl FnOnce() -> Result<Option<OwnedFd>, vk::Result>,
    ) -> Self {
        if !exportable {
            return Self::Poll;
        }
        match export() {
            Ok(Some(fd)) => Self::Fd(fd),
            Ok(None) => Self::Ready,
            Err(e) => {
                log::warn!(
                    "{context}: vkGetSemaphoreFdKHR(SYNC_FD) failed: {e:?}; \
                     falling back to FenceTicket polling"
                );
                Self::Poll
            }
        }
    }
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

#[cfg(test)]
mod tests {
    use super::PresentBatchWait;
    use ash::vk;

    #[test]
    fn without_sync_fd_the_batch_polls_and_never_exports() {
        let wait = PresentBatchWait::from_signal_export("test", false, || {
            panic!("no SYNC_FD export may be attempted without device support")
        });
        assert!(matches!(wait, PresentBatchWait::Poll));
    }

    #[test]
    fn sync_file_wait_is_signaled_once_readable() {
        let (rx, tx) = std::os::unix::net::UnixStream::pair().expect("socketpair");
        let wait = PresentBatchWait::Fd(rx.into());
        assert!(!wait.signaled(), "nothing to read yet");
        std::io::Write::write_all(&mut &tx, b"x").expect("write");
        assert!(wait.signaled());
        assert!(PresentBatchWait::Ready.signaled());
        assert!(PresentBatchWait::Poll.signaled());
    }

    #[test]
    fn exportable_signal_maps_export_outcomes() {
        assert!(matches!(
            PresentBatchWait::from_signal_export("test", true, || Ok(None)),
            PresentBatchWait::Ready
        ));
        assert!(matches!(
            PresentBatchWait::from_signal_export("test", true, || Err(
                vk::Result::ERROR_EXTENSION_NOT_PRESENT
            )),
            PresentBatchWait::Poll
        ));
    }
}
