use super::*;

impl PlatformBackend {
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
        self.queue_submits += 1;
        match crate::kms::vk::submit_stats::timed(
            crate::kms::vk::submit_stats::SubmitCause::PresentSignal,
            0,
            false,
            || unsafe {
                vk.device
                    .queue_submit2(vk.graphics_queue, &submit, signal_fence)
            },
        ) {
            Ok(()) => Ok(()),
            Err(e) => {
                self.renderer_failed = true;
                Err(e)
            }
        }
    }

    // ── Phase A: SubmitGroup API ─────────────────────────────────

    /// Count the next group flush (#214 telemetry) under `cause` instead of the
    /// cause its `FlushReason` maps to. Consumed by that flush.
    pub(crate) fn set_next_submit_cause(
        &mut self,
        cause: crate::kms::vk::submit_stats::SubmitCause,
    ) {
        self.next_submit_cause = Some(cause);
    }

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
    pub(crate) fn submit_group_peek_entries_for_tests(
        &self,
    ) -> &[crate::kms::render::submit_group::GroupEntry] {
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

    /// Arm a recording failure in the next frame close (integration tests).
    pub(crate) fn force_next_frame_record_failure_for_integration_tests(&mut self) {
        self.force_next_frame_record_failure = true;
    }

    /// Consume the latch armed by
    /// [`Self::force_next_frame_record_failure_for_integration_tests`].
    pub(crate) fn take_forced_frame_record_failure(&mut self) -> bool {
        std::mem::take(&mut self.force_next_frame_record_failure)
    }

    /// Real `vkQueueSubmit2` calls this platform made for paint groups and
    /// Present signals.
    pub(crate) fn queue_submit_count(&self) -> u64 {
        self.queue_submits
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
        let cause = self.next_submit_cause.take().unwrap_or(match reason {
            FlushReason::SyncBoundary => crate::kms::vk::submit_stats::SubmitCause::GroupSync,
            FlushReason::SceneCompose => crate::kms::vk::submit_stats::SubmitCause::GroupCompose,
            FlushReason::PresentCompletionSignal => {
                crate::kms::vk::submit_stats::SubmitCause::GroupPresent
            }
            FlushReason::FrameBuilder => crate::kms::vk::submit_stats::SubmitCause::FrameOther,
            FlushReason::PageflipRetire | FlushReason::MaxSize | FlushReason::Shutdown => {
                crate::kms::vk::submit_stats::SubmitCause::GroupOther
            }
        });
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
            return self.abort_flush(entries, n, reason, vk::Result::ERROR_DEVICE_LOST, false);
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
        self.queue_submits += 1;
        match crate::kms::vk::submit_stats::timed(cause, n, export_signal.is_some(), || unsafe {
            vk.device
                .queue_submit2(vk.graphics_queue, &submit, ticket.fence())
        }) {
            Ok(()) => {
                ticket.retain_imported_wait_semaphores(imported_wait_semaphores);
                // GLX-TFP write→read publish: export the submit's
                // completion sync_file and import it onto every exported
                // dma-buf the group wrote.
                if let Some(sig) = export_signal {
                    Self::publish_export_write_fences(&sig, exported_writes);
                    // The semaphore is a signal operation of the submit
                    // just queued. Dropping it here — as this did before —
                    // destroyed it while the queue was still using it,
                    // once per write to any exported pixmap. The ticket
                    // destroys it when the fence retires.
                    ticket.retain_signal_semaphore(sig.into_raw());
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
                let may_be_pending = submit_error_may_leave_pending(e);
                if !may_be_pending {
                    unsafe {
                        for semaphore in imported_wait_semaphores {
                            vk.device.destroy_semaphore(semaphore, None);
                        }
                    }
                }
                self.abort_flush(entries, n, reason, e, may_be_pending)
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
    ///
    /// `may_be_pending` (the submit failed with a lost device) leaks the
    /// CBs instead: freeing them would be
    /// VUID-vkFreeCommandBuffers-pCommandBuffers-00047.
    fn abort_flush(
        &mut self,
        entries: Vec<crate::kms::render::submit_group::GroupEntry>,
        n: usize,
        reason: FlushReason,
        err: vk::Result,
        may_be_pending: bool,
    ) -> Result<FlushOutcome, vk::Result> {
        self.renderer_failed = true;
        if may_be_pending {
            log::error!("flush_submit_group: submit failed ({err:?}); leaking {n} command buffers");
        } else if let (Some(vk), Some(pool)) = (self.vk.as_ref(), self.ops_command_pool_handle()) {
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
}
