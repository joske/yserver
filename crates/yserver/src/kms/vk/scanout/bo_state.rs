use super::*;

impl CopiedProbeReadback<'_> {
    pub(super) fn destination_buffer(self, transfer: &TransferResources) -> vk::Buffer {
        match self {
            Self::CpuExact => transfer.staging_buffer,
            Self::GpuDigest(digest) => digest.input_buffer(),
        }
    }
}

impl CopiedDestinationOwnership {
    pub(super) fn foreign_acquire_layouts(self) -> Option<(vk::ImageLayout, vk::ImageLayout)> {
        match self {
            Self::ForeignImportedFirstUse => {
                Some((vk::ImageLayout::UNDEFINED, vk::ImageLayout::GENERAL))
            }
            Self::ForeignRetiredByKms => Some((vk::ImageLayout::GENERAL, vk::ImageLayout::GENERAL)),
            Self::LocalFirstUse | Self::ReleasedButAtomicRejected => None,
            Self::ForeignPendingKmsFromSink | Self::ForeignPendingKmsUninitialized => None,
        }
    }

    pub(super) fn local_copy_old_layout(self) -> io::Result<vk::ImageLayout> {
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

    pub(super) fn after_lifecycle_quiescence(self) -> Self {
        let _ = self;
        // KMS is off and B is idle. Every copied frame is a guaranteed full
        // overwrite, so discard the old contents and start locally rather
        // than fabricating a release from an external owner that no longer
        // participates in the lifecycle.
        Self::ReleasedButAtomicRejected
    }

    pub(super) fn after_kms_modeset(self) -> Self {
        match self {
            Self::ForeignPendingKmsFromSink | Self::ForeignPendingKmsUninitialized => self,
            Self::ForeignRetiredByKms => Self::ForeignPendingKmsFromSink,
            Self::LocalFirstUse
            | Self::ForeignImportedFirstUse
            | Self::ReleasedButAtomicRejected => Self::ForeignPendingKmsUninitialized,
        }
    }

    pub(super) fn after_kms_retirement(self, bo_idx: usize) -> io::Result<Self> {
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
    pub(super) fn transport_preparation(self) -> io::Result<CopiedTransportPreparation> {
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

    pub(super) fn after_lifecycle_quiescence(self) -> Self {
        let _ = self;
        // A and B are both idle and the display has been taken off-screen.
        // The next copied compose is Full, so abandoning any interrupted
        // handoff and reinitializing from UNDEFINED is the safe common state.
        Self::RendererDiscard
    }
}

impl CopiedRenderTargetContents {
    pub(super) fn note_submit_succeeded(&mut self) {
        *self = Self::Initialized;
    }

    pub(super) fn invalidate(&mut self) {
        *self = Self::Uninitialized;
    }

    pub(super) fn validate_readback(self) -> io::Result<()> {
        if self == Self::Initialized {
            Ok(())
        } else {
            Err(io::Error::other(
                "copied source has no preserved local render-target pixels",
            ))
        }
    }
}

impl RetainedSyncFile {
    pub(super) fn from_optional(fd: Option<OwnedFd>) -> Self {
        match fd {
            Some(fd) => Self::Fd(fd),
            None => Self::AlreadySignalled,
        }
    }

    pub(super) fn into_optional(self) -> Option<OwnedFd> {
        match self {
            Self::AlreadySignalled => None,
            Self::Fd(fd) => Some(fd),
        }
    }
}

impl ExportSemaphoreReuseState {
    pub(super) fn begin_post_submit_export(&mut self) {
        *self = Self::NeedsRearm;
    }

    pub(super) fn finish_successful_export(&mut self) {
        *self = Self::Reusable;
    }

    pub(super) fn needs_rearm(self) -> bool {
        self == Self::NeedsRearm
    }
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
