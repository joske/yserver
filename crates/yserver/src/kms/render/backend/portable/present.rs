//! Deferred PRESENT completions: the batch queue, retained wakes and
//! Present source pins.

use crate::kms::render::{
    backend::{XBackend, seam::Platform},
    target::PaintTarget,
};
use yserver_core::backend::Backend as _;

impl<P: Platform> XBackend<P> {
    /// Stage 5 Task 6.1: pick up any PRESENT completions that were
    /// queued past `disable_output` so the caller (lib.rs::run) can
    /// fan them out to clients before tearing down the socket.
    pub fn take_shutdown_present_events(
        &mut self,
    ) -> Vec<yserver_core::backend::CompletedPresentEvent> {
        std::mem::take(&mut self.pending_completed_events_on_shutdown)
    }

    /// Shutdown-only: walk every drawable still in `store` and
    /// destroy its Vk handles (`VkImage` / `VkImageView` /
    /// `VkDeviceMemory`). `DrawableStore` has no `Drop` impl
    /// because `Storage::destroy` needs the platform's `GpuCore` for
    /// pool-return + DRI3-import handling — both fields are
    /// owned by the backend, so this method bridges them.
    /// Without this, MATE's resident pixmaps at SIGTERM leak
    /// (948 VkDeviceMemory observed on bee/MATE 2026-05-31
    /// post-FenceTicket fix). Caller is `lib.rs`'s explicit
    /// shutdown block, after PRESENT completions are drained
    /// and before `backend` drops.
    pub fn shutdown_destroy_drawables(&mut self) {
        self.store.shutdown_destroy_all(self.platform.gpu());
    }

    /// Signal (and release) every wake pin still retained after the last
    /// drain. `fire_pending_present_entry` now retains instead of signalling,
    /// so at teardown these must be flushed to release the buffers a client is
    /// blocked on. Called in the shutdown sequence before
    /// `shutdown_destroy_drawables`.
    pub fn signal_all_retained_present_wakes(&mut self) {
        let ids: Vec<u64> = self.retained_present_wakes.keys().copied().collect();
        for id in ids {
            self.signal_present_wake(id);
        }
    }

    fn fire_pending_present_entry(
        &mut self,
        entry: Box<dyn crate::kms::render::present_batch::PresentCompletionEntry>,
    ) -> yserver_core::backend::CompletedPresentEvent {
        // Do NOT signal here. Retain the pin keyed by present_id so core can
        // release it at the target vblank (frame pacing). PinnedWake::None
        // entries need no retention.
        // Destructure so `wake_pin` is moved exactly once (PinnedWake is not
        // Copy — a `matches!(entry.wake_pin, ..)` guard would move it first and
        // fail to compile).
        use crate::kms::render::present_completion::{
            PendingPresentEntry, PinnedWake, into_pending_entry,
        };
        let PendingPresentEntry { wake_pin, event } = into_pending_entry(entry);
        if !matches!(wake_pin, PinnedWake::None) {
            self.retained_present_wakes
                .insert(event.present_id, wake_pin);
        }
        event
    }

    fn register_pending_present_batch(
        &mut self,
        mut batch: crate::kms::render::present_batch::PendingPresentBatch,
    ) {
        use crate::kms::render::present_batch::PresentBatchWait;

        if batch.events.is_empty() {
            return;
        }

        batch.wait = P::register_present_batch_wait(self, batch.wait);
        let should_wake = !matches!(batch.wait, PresentBatchWait::Fd(_));

        self.pending_present_batches.push_back(batch);
        if should_wake {
            P::wake_present_completion(self, "register_pending_present_batch");
        }
    }

    pub(in crate::kms::render::backend) fn drain_engine_present_batches(&mut self) {
        for batch in self.engine.drain_present_batches() {
            self.register_pending_present_batch(batch);
        }
    }

    fn pending_present_batch_ready(
        batch: &crate::kms::render::present_batch::PendingPresentBatch,
        vk: Option<&std::sync::Arc<crate::kms::vk::device::VkContext>>,
    ) -> bool {
        // B.2-context fix (vkdebug VUID-vkDestroySemaphore-semaphore-05149):
        // sync_file readiness alone is NOT sufficient to drop the batch
        // (which destroys `batch.signal` — the export semaphore — via
        // PresentCompletionSignal::Drop). Vulkan requires the queue
        // submit fence to signal before any of its semaphores are
        // destroyed; the sync_file becomes readable as soon as the
        // KMS pageflip retires, which can be before the GPU compose
        // CB's fence signals. Gate every wait variant additionally on
        // the ticket having signaled when one is present.
        if !batch.wait.signaled() {
            return false;
        }
        // Even if the sync_file signaled / Ready variant, the
        // Vulkan-side fence must have signaled too before we let the
        // batch (and its export semaphore) drop. Skip the ticket
        // check only when no ticket was attached (degraded path).
        match (&batch.ticket, vk) {
            (Some(ticket), Some(v)) => ticket.poll_signaled(v),
            (Some(_), None) => false,
            (None, _) => true,
        }
    }

    pub(in crate::kms::render::backend) fn force_drain_all_present_batches(
        &mut self,
    ) -> Vec<yserver_core::backend::CompletedPresentEvent> {
        let mut completed = Vec::new();
        while let Some(batch) = self.pending_present_batches.pop_front() {
            P::release_present_batch_wait(self, &batch);
            for entry in batch.events {
                completed.push(self.fire_pending_present_entry(entry));
            }
        }
        completed
    }

    /// Stage 5 Task 6.1 — algorithm body for `Backend::
    /// drain_completed_present_events`. Walks the front of
    /// `pending_present_batches`, popping whole batches whose exported
    /// sync_file is readable, whose ready sentinel fired, or whose
    /// degraded polling ticket has signaled. Fires wake signals via the
    /// Arc-pinned handles and returns the public event payloads.
    pub(in crate::kms::render::backend) fn drain_completed_present_events_impl(
        &mut self,
    ) -> Vec<yserver_core::backend::CompletedPresentEvent> {
        self.drain_engine_present_batches();
        P::clear_present_completion_wake(self);

        let renderer_failed = self.platform.gpu().renderer_failed;
        if renderer_failed && !self.pending_present_batches.is_empty() {
            let pending_events: usize = self
                .pending_present_batches
                .iter()
                .map(|batch| batch.events.len())
                .sum();
            log::warn!("renderer_failed: force-firing {pending_events} pending PRESENT entries");
        }

        // Clone the Arc<VkContext> so the loop body can borrow `self`
        // mutably (for `dri3_*_via_handle`) without a conflict with
        // `self.platform.vk`.
        let vk = self.platform.gpu().vk.as_ref().cloned();

        let mut completed = Vec::new();
        while let Some(front) = self.pending_present_batches.front() {
            let ready = renderer_failed || Self::pending_present_batch_ready(front, vk.as_ref());
            if !ready {
                break;
            }
            let entry = self
                .pending_present_batches
                .pop_front()
                .expect("just peeked");
            P::release_present_batch_wait(self, &entry);
            for event in entry.events {
                completed.push(self.fire_pending_present_entry(event));
            }
        }
        completed
    }

    pub(in crate::kms::render::backend) fn backend_present_pin_present_source(
        &mut self,
        host_xid: u32,
    ) -> Option<u64> {
        let id = self.store.lookup(host_xid)?;
        self.store.incref(id);
        let pin_id = self.next_present_source_pin_id;
        self.next_present_source_pin_id = self.next_present_source_pin_id.wrapping_add(1).max(1);
        self.present_source_pins.insert(pin_id, id);
        Some(pin_id)
    }

    pub(in crate::kms::render::backend) fn backend_present_release_present_source(
        &mut self,
        pin_id: u64,
    ) {
        let Some(id) = self.present_source_pins.remove(&pin_id) else {
            return;
        };
        self.store_decref_with_invalidate(id);
    }

    /// Stage 5 Task 6.1 — queue a deferred PRESENT completion.
    ///
    /// COW-targeted PRESENT attaches the completion payload to the
    /// still-open COW copy batch. When that batch submits, it signals a
    /// dedicated export-only semaphore in the same queue submission;
    /// the exported sync_file FD drives completion without touching the
    /// `FenceTicket` used for yserver's internal lifetime tracking.
    /// Non-COW PRESENT whose copy is still in the open frame does the same
    /// and closes that frame at once (#214). Otherwise it falls back to one
    /// signal-only queue submit after the already-submitted copy, relying
    /// on same-queue ordering.
    pub(in crate::kms::render::backend) fn backend_present_enqueue_present_completion(
        &mut self,
        event: yserver_core::backend::CompletedPresentEvent,
        dst_host_xid: u32,
    ) {
        use yserver_core::backend::PresentWake;

        use crate::kms::render::{
            present_batch::{PendingPresentBatch, PresentBatchWait, PresentCompletionEntry},
            present_completion::{PendingPresentEntry, PinnedWake},
        };

        let wake_pin = match &event.wake {
            PresentWake::Pixmap { idle_fence_xid } if *idle_fence_xid != 0 => {
                match self.dri3_xshmfence_handle(*idle_fence_xid) {
                    Some(h) => PinnedWake::Pixmap(h),
                    None => PinnedWake::None,
                }
            }
            PresentWake::PixmapSynced {
                release,
                release_syncobj,
                release_value,
            } if *release_syncobj != 0 => PinnedWake::PixmapSynced {
                handle: release.clone(),
                value: *release_value,
            },
            _ => PinnedWake::None,
        };

        let mut entry: Box<dyn PresentCompletionEntry> =
            Box::new(PendingPresentEntry { wake_pin, event });

        if let Some(cow_id) = self.cow_id
            && self.store.lookup(dst_host_xid) == Some(cow_id)
        {
            match self.engine.attach_present_completion(cow_id, entry) {
                Ok(()) => return,
                Err(returned) => entry = returned,
            }
        }

        // The copy wrote wherever `resolve_paint_target` routed it: a window
        // inside a redirected parent shares that ancestor's backing, not its
        // own leaf storage. Unviewable windows resolve to None (copy dropped).
        // A shared backing may also match another writer's op when this
        // copy clipped to nothing; harmless, the signal still follows it.
        let completion_target = self
            .resolve_paint_target(dst_host_xid)
            .map(PaintTarget::backing_id);

        // Phase A: close any open render batch FIRST so its CBs land
        // in the group under the same ticket the flush will consume.
        // Then ensure all prior paint is on the queue BEFORE the
        // completion signal. Engine-driven so any parked pending_group_ops
        // graduate to `submitted` atomically with the submit.
        // Spec § "Phase A — concrete scope" trigger 2 (Codex pass-3 fix).
        if let Err(e) = self.engine.flush_render_batch(
            &mut self.store,
            self.platform.gpu_mut(),
            crate::kms::render::engine::RenderFlushReason::Present,
        ) {
            log::warn!("render enqueue_present_completion: flush_render_batch failed: {e:?}");
        }

        // #214: when the open frame holds the copy (an op writing the
        // destination), attach the completion to it and close it now: the
        // export signal rides the paint submit, one vkQueueSubmit2 instead
        // of paint + a signal-only submit. The close publishes the release
        // fence and hands the batch to the completion scheduler; a close
        // that fails keeps the entry as a ready batch (never dropped).
        // Otherwise (copy already submitted, clipped to nothing, or recorded
        // another way) fall through to the signal-only submit.
        if let Some(dst_id) = completion_target {
            match self.engine.attach_present_completion(dst_id, entry) {
                Ok(()) => {
                    if let Err(e) = self.engine.close_open_frame(
                        &mut self.store,
                        self.platform.gpu_mut(),
                        crate::kms::render::frame_builder::CloseReason::PresentCompletionSignal,
                    ) {
                        log::warn!(
                            "render enqueue_present_completion: close_open_frame failed: {e:?}"
                        );
                    }
                    self.drain_frame_builder_telemetry();
                    self.drain_engine_present_batches();
                    return;
                }
                Err(returned) => entry = returned,
            }
        }
        // Phase B.1 close trigger 1b: close any open frame before the
        // signal-only submit so the semaphore-export's SYNC_FD captures a
        // queued signal-op for ANY paint work that came through the frame
        // builder. Same hazard as Task 6.1 (VUID-VkFenceGetFdInfoKHR-handleType-01457).
        if let Err(e) = self.engine.close_open_frame(
            &mut self.store,
            self.platform.gpu_mut(),
            crate::kms::render::frame_builder::CloseReason::PresentCompletionSignal,
        ) {
            log::warn!("render enqueue_present_completion: close_open_frame failed: {e:?}");
        }
        // Phase B.1 Task 21: drain frame-builder close events into telemetry.
        self.drain_frame_builder_telemetry();
        if let Err(e) = self.engine.flush_submit_group(
            &mut self.store,
            self.platform.gpu_mut(),
            crate::kms::render::submit_group::FlushReason::PresentCompletionSignal,
        ) {
            log::warn!("render enqueue_present_completion: flush_submit_group failed: {e:?}");
            // Fall through; the signal-only submit will fail with
            // renderer_failed and the caller's error handling kicks in.
        }

        let fallback_ticket = completion_target
            .or_else(|| self.store.lookup(dst_host_xid))
            .and_then(|id| self.store.get(id))
            .and_then(|d| d.last_render_ticket.clone());

        let mut batch_ticket = fallback_ticket;
        let (wait, signal) = match (
            self.platform.gpu().acquire_present_completion_signal(),
            self.platform.gpu().acquire_fence_ticket(),
        ) {
            (Ok(signal), Ok(ticket)) => {
                match self
                    .platform
                    .gpu_mut()
                    .submit_present_completion_signal(&signal, ticket.fence())
                {
                    Ok(()) => {
                        batch_ticket = Some(ticket);
                        let wait = PresentBatchWait::from_signal_export(
                            "enqueue_present_completion",
                            signal.sync_file_exportable(),
                            || {
                                let fd = signal.export_sync_file_fd()?;
                                if let Some(fd) = &fd
                                    && let Err(e) = entry.publish_release_fence(fd)
                                {
                                    log::warn!(
                                        "enqueue_present_completion: publish Present release \
                                         fence failed: {e}; falling back to host signal"
                                    );
                                }
                                Ok(fd)
                            },
                        );
                        (wait, Some(signal))
                    }
                    Err(e) => {
                        log::warn!(
                            "enqueue_present_completion: signal-only queue submit failed: {e:?}; \
                             falling back to prior FenceTicket polling"
                        );
                        (PresentBatchWait::Poll, Some(signal))
                    }
                }
            }
            (Err(e), _) => {
                log::warn!(
                    "enqueue_present_completion: completion semaphore allocation failed: {e:?}; \
                     falling back to FenceTicket polling"
                );
                (PresentBatchWait::Poll, None)
            }
            (Ok(_signal), Err(e)) => {
                log::warn!(
                    "enqueue_present_completion: completion fence allocation failed: {e:?}; \
                     falling back to prior FenceTicket polling"
                );
                (PresentBatchWait::Poll, None)
            }
        };

        self.register_pending_present_batch(PendingPresentBatch {
            wait,
            ticket: batch_ticket,
            signal,
            events: vec![entry],
        });
    }

    pub(in crate::kms::render::backend) fn backend_present_signal_present_wake(
        &mut self,
        present_id: u64,
    ) {
        use crate::kms::render::present_completion::PinnedWake;
        let Some(pin) = self.retained_present_wakes.remove(&present_id) else {
            return;
        };
        match pin {
            PinnedWake::Pixmap(h) => {
                if let Err(e) = self.dri3_trigger_fence_via_handle(&h) {
                    log::warn!("signal_present_wake: dri3_trigger_fence_via_handle failed: {e}");
                }
            }
            PinnedWake::PixmapSynced { handle, value } => {
                if let Err(e) = self.dri3_signal_syncobj_via_handle(&handle, value) {
                    log::warn!("signal_present_wake: dri3_signal_syncobj_via_handle failed: {e}");
                }
            }
            // The release point already carries the GPU completion fence.
            // Consuming the pin here drops its retained handle without
            // advancing the timeline from the host.
            PinnedWake::PixmapSyncedFencePublished {
                handle: _handle,
                value: _value,
            } => {}
            PinnedWake::None => {}
        }
    }
}
