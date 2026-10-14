//! The platform-neutral half of a composite tick.

use crate::kms::render::{
    backend::{PENDING_RETIRE_POLL_INTERVAL, XBackend, seam::Platform},
    submit_trace::{
        Flags as SubmitFlags, Op as SubmitOp, SrcClass, SubmitEvent, SubmitKind, TargetKind,
    },
};
use yserver_core::backend::Backend as _;

impl<P: Platform> XBackend<P> {
    /// The scene needs a (re)compose when its structure changed
    /// (map/unmap/restack/redirect — `scene_structure_dirty`) OR a
    /// client painted a scene-participating window since the last
    /// compose (undrained presentation damage). `next_wakeup` and
    /// `maybe_composite` MUST use the same predicate: if `next_wakeup`
    /// armed a wake that `maybe_composite` then declined, the loop
    /// would busy-spin; if `maybe_composite` composed state that
    /// `next_wakeup` never wakes for, the paint would be stranded (the
    /// xfce submenu bug — a menu painted after its map-compose sat
    /// off-screen until an unrelated event poked the loop).
    pub(crate) fn scene_wants_scene(&self) -> bool {
        self.scene.scene_structure_dirty
            || self.store.has_pending_presentation_damage()
            // An invalidated or never-presented frame owes a repaint no producer
            // reports; without this the tick is never driven to paint it.
            || self.scene.owes_repaint()
    }

    /// One compose pass: a new frame id, the paint flushes the compose
    /// samples after, `scene.tick` and its telemetry. Returns the outputs
    /// that submitted a frame, `None` when no tick ran or it failed.
    pub(crate) fn compose_tick(&mut self) -> Option<Vec<usize>> {
        // One main-loop tick = one frame_id. Submit events
        // recorded between calls share the surrounding tick's
        // id; the scene_compose event of this tick (if it
        // submits) also carries this id.
        self.telemetry.advance_frame();
        let can_submit_scene =
            self.scene_wants_scene() && self.scene.has_output_ready_for_submit(&self.platform);
        // #214 telemetry: did this tick's compose close submit paint, and
        // did the tick then compose anything?
        let mut legacy_close_submitted = false;
        if can_submit_scene {
            // Stage 5 Task 3 (render-composite generalization): flush
            // the render batch — scene.tick samples dst.
            self.drain_engine_present_batches();
            if let Err(e) = self.engine.flush_render_batch(
                &mut self.store,
                self.platform.gpu_mut(),
                crate::kms::render::engine::RenderFlushReason::Present,
            ) {
                log::warn!("render maybe_composite: flush_render_batch failed: {e:?}");
            }
            // Phase B Invariant M3: close any open frame BEFORE legacy compose
            // records. compose samples drawable storage at record time
            // (scene.rs:1307), so the open frame's layout + ticket-touch overlays
            // must be committed before the compose CB lands. Retires at sub-phase
            // B.4 when compose itself ports into the frame builder.
            // NOTE: integration test for M3 lives in Task 23's mixed-sequence
            // smoke (frame_builder_mixed_sequence_smoke); Task 13 only adds
            // the wiring. Until Task 15 ports composite_glyphs into the frame
            // builder, no frame can be open, so this call is a no-op.
            match self.engine.close_open_frame(
                &mut self.store,
                self.platform.gpu_mut(),
                crate::kms::render::frame_builder::CloseReason::LegacyScCompose,
            ) {
                Ok(crate::kms::render::frame_builder::CloseOutcome::Submitted { .. }) => {
                    legacy_close_submitted = true;
                }
                Ok(crate::kms::render::frame_builder::CloseOutcome::AlreadyClosed) => {}
                Err(e) => log::warn!("render maybe_composite: close_open_frame failed: {e:?}"),
            }
            // Phase A Task 4: flush the SubmitGroup so scene.tick
            // observes all paint CBs already submitted to the queue.
            // Compose stays on its own dedicated `vkQueueSubmit2`
            // (record_compose) — only the buffered paint group is
            // flushed here. Drive through the engine wrapper so
            // parked `pending_group_ops` commit too.
            if let Err(e) = self.engine.flush_submit_group(
                &mut self.store,
                self.platform.gpu_mut(),
                crate::kms::render::submit_group::FlushReason::SceneCompose,
            ) {
                log::warn!("render maybe_composite: flush_submit_group failed: {e:?}");
            }
        }
        // Task 4 inserts ready direct-candidate arbitration here. Direct
        // Present callbacks keep their existing admission/submission paths.
        let mut submitted = None;
        if can_submit_scene {
            let cow_host_xid = self.cow_host_xid();
            let outputs = self.platform.output_layouts();
            let input = crate::kms::render::backend::scene_input(
                &self.core,
                &self.windows,
                &outputs,
                self.platform.fb_size(),
            );
            match self.scene.tick(
                &input,
                &mut self.store,
                &mut self.platform,
                &mut self.telemetry,
                cow_host_xid,
            ) {
                Ok(composed_outputs) => {
                    if legacy_close_submitted {
                        crate::kms::vk::submit_stats::SUBMITS
                            .record_legacy_sc_tick(!composed_outputs.is_empty());
                    }
                    for &output_idx in &composed_outputs {
                        self.telemetry.record_composite_submit();
                        // One scene_compose event per output that presented
                        // this tick, keyed by the exact output index.
                        self.telemetry.record_submit_event(SubmitEvent {
                            frame_id: 0,
                            kind: SubmitKind::SceneCompose,
                            target_kind: TargetKind::Output,
                            target_id: u64::try_from(output_idx).unwrap_or(0),
                            batch_size: 1,
                            op: SubmitOp::None,
                            src_class: SrcClass::None,
                            mask_class: SrcClass::None,
                            pipeline_id: None,
                            flags: SubmitFlags::NONE,
                        });
                    }
                    submitted = Some(composed_outputs);
                }
                Err(e) => {
                    if legacy_close_submitted {
                        crate::kms::vk::submit_stats::SUBMITS.record_legacy_sc_tick(false);
                    }
                    log::warn!("render maybe_composite: scene.tick failed: {e:?}");
                }
            }
        }
        submitted
    }

    /// Reclaim GPU work whose fences signalled (the `before_block` body).
    pub(crate) fn reclaim_retired_gpu_work(&mut self) {
        // BlockHandler analog (cf. Xorg glamor_block_handler → glamor_flush):
        // every dispatch-loop iteration, just before the core loop blocks,
        // reap render-op resources whose fences have signaled. This is the
        // reclaim half of `on_page_flip_ready` (the scanout / compose / flush
        // half stays page-flip-driven), lifted onto the dispatch loop so it
        // runs even when no page-flip occurs.
        //
        // Without this, the engine `submitted` queue (one per-op command
        // buffer + any displaced images each) is drained ONLY on page-flip.
        // While the display is dark (DPMS-off / monitor standby / VT-away)
        // page-flips stop, but clients keep submitting render ops, so the
        // queue grows without bound until amdgpu can't allocate command-
        // submission memory and the device is lost
        // (project_reclamation_starvation_leak). poll_retired only frees
        // ops whose fence has signaled and is a cheap no-op on an empty
        // queue, so running it every iteration costs nothing at idle.
        self.engine.poll_retired(self.platform.gpu());
        self.poll_pending_retire_with_invalidate();
        // #196: destroy pixmap-pool entries idle past the eviction age, so
        // a burst drains back down; `next_wakeup` wakes us for it.
        if let Some(pool) = self.platform.gpu().pixmap_pool.as_ref() {
            pool.trim_idle(std::time::Instant::now());
        }
        // Diagnostic: drive the 1Hz telemetry emit from here too,
        // publishing the live `submitted`-queue depth. maybe_emit()
        // self-gates to 1Hz and is a no-op below threshold, but running
        // it every dispatch iteration means the `render_telemetry:` line (and
        // the submit-trace flush) keep ticking even while the display is
        // dark — exactly when `submitted_queue_depth` is the number worth
        // watching (project_reclamation_starvation_leak). Without this,
        // the only other maybe_emit caller is on the compose path, which
        // is gated off when dark, so telemetry went silent in the window.
        self.telemetry.maybe_emit(self.engine.pending_count());
    }

    /// Submit recorded drawing before its damage is exposed.
    pub(crate) fn flush_before_damage_exposure(&mut self) {
        // A compositor may sample a redirected backing after DamageNotify
        // or after retrieving coalesced damage via Subtract/FetchRegion.
        // Close all recording paths before publishing their producer fences.
        if let Err(error) = self.engine.flush_render_batch(
            &mut self.store,
            self.platform.gpu_mut(),
            crate::kms::render::engine::RenderFlushReason::Other,
        ) {
            log::warn!("render damage boundary batch flush failed: {error:?}");
        }
        if let Err(error) = self.engine.close_open_frame(
            &mut self.store,
            self.platform.gpu_mut(),
            crate::kms::render::frame_builder::CloseReason::DamageBoundary,
        ) {
            log::warn!("render DamageNotify submission boundary failed: {error:?}");
        }
        // Closing an already-closed frame does not drain command buffers
        // parked in the submit group (including the render batch above).
        if let Err(error) = self.engine.flush_submit_group(
            &mut self.store,
            self.platform.gpu_mut(),
            crate::kms::render::submit_group::FlushReason::SyncBoundary,
        ) {
            log::warn!("render damage boundary submit flush failed: {error:?}");
        }
    }

    /// The earliest deadline of platform-neutral GPU bookkeeping: a polled
    /// Present batch, a drawable awaiting retirement, the open paint frame's
    /// timeout and the pixmap pool's idle trim.
    pub(crate) fn gpu_work_deadline(&self, now: std::time::Instant) -> Option<std::time::Instant> {
        let needs_present_poll = self.pending_present_batches.iter().any(|batch| {
            matches!(
                batch.wait,
                crate::kms::render::present_batch::PresentBatchWait::Poll
            )
        });
        let present_deadline =
            needs_present_poll.then(|| now + std::time::Duration::from_millis(1));
        // A drawable freed while its GPU work was in flight waits in
        // `pending_retire` for `before_block` to see its fence signal; with
        // no other activity nothing would wake us to release it.
        let retire_deadline =
            (self.store.pending_retire_count() > 0).then(|| now + PENDING_RETIRE_POLL_INTERVAL);
        present_deadline
            .into_iter()
            .chain(retire_deadline)
            .chain(self.engine.open_frame_timeout_deadline())
            .chain(
                self.platform
                    .gpu()
                    .pixmap_pool
                    .as_ref()
                    .and_then(|pool| pool.next_trim_deadline()),
            )
            .min()
    }
}
