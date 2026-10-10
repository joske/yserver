use super::*;

impl InFlightStage {
    fn matches_render_completion(self, job_id: u64) -> bool {
        self == Self::WaitingForRenderCompletion { job_id }
    }

    fn is_kms_flip_pending(self) -> bool {
        self == Self::KmsFlipPending
    }
}

pub(super) fn copied_render_completion_matches(
    stage: InFlightStage,
    pending_bo_idx: usize,
    completion_job_id: u64,
    completion_bo_idx: usize,
) -> bool {
    pending_bo_idx == completion_bo_idx && stage.matches_render_completion(completion_job_id)
}

pub(super) fn kms_retirement_matches(
    stage: InFlightStage,
    pending_bo_idx: usize,
    presented_bo_idx: usize,
) -> bool {
    pending_bo_idx == presented_bo_idx && stage.is_kms_flip_pending()
}

impl SceneCompositor {
    /// True while any output has an atomic pageflip awaiting retirement.
    /// Present completion pacing uses this with pending compose damage to
    /// decide whether a standalone CRTC sequence is a genuine idle fallback.
    pub(crate) fn has_pending_page_flips(&self) -> bool {
        #[cfg(test)]
        if let Some(v) = self.test_flip_in_flight_override {
            return v;
        }
        self.inner
            .as_ref()
            .is_some_and(|inner| inner.outputs.iter().any(|o| !o.pending_acks.is_empty()))
    }

    /// True while one specific output has an atomic pageflip awaiting
    /// retirement. Present scheduling is CRTC-domain-specific: activity on a
    /// different card/output must not change the selected output's immediate
    /// target rule.
    pub(crate) fn has_pending_page_flip(&self, output_idx: usize) -> bool {
        #[cfg(test)]
        if let Some(v) = self.test_flip_in_flight_override {
            return v;
        }
        self.inner
            .as_ref()
            .and_then(|inner| inner.outputs.get(output_idx))
            .is_some_and(|output| !output.pending_acks.is_empty())
    }

    /// Handle a DRM page-flip-complete event for `output_idx`.
    /// Pops the matching pending-ack, ack's its damage snapshots
    /// against the store, releases the descriptor-pool slot,
    /// then advances the platform's BO state machine via
    /// `on_page_flip_complete`. Engine retirement happens after
    /// (driven by the backend wrapper to keep the borrows clean).
    pub(crate) fn handle_page_flip_complete(
        &mut self,
        output_idx: usize,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
    ) -> bool {
        let Some(inner) = self.inner.as_mut() else {
            return false;
        };
        let expected = inner
            .outputs
            .get(output_idx)
            .and_then(|state| state.pending_acks.front())
            .map(|ack| (ack.stage, ack.bo_idx));
        let Some(retire) = platform.on_page_flip_complete(output_idx) else {
            return false;
        };
        let Some(state) = inner.outputs.get_mut(output_idx) else {
            return false;
        };
        if !expected.is_some_and(|(stage, bo_idx)| {
            kms_retirement_matches(stage, bo_idx, retire.presented_bo_idx)
        }) {
            log::warn!(
                "render scene: page-flip-complete on output {output_idx} presented bo {} \
                 but the pending scene frame was {expected:?}",
                retire.presented_bo_idx,
            );
            // `platform.on_page_flip_complete` above already advanced the BO
            // phase machine (previous OnScreen -> Free, Pending -> OnScreen), and
            // we are about to return without popping the ack. Platform and scene
            // have diverged, so neither `missing` nor any staged frame can be
            // trusted: fall back to "the whole output is stale", which costs one
            // full repaint and cannot show a stale pixel.
            //
            // The retained ack itself is a pre-existing wedge (the tick's
            // flip-pending gate will skip this output until another flip event
            // arrives); this is deliberately not the change that addresses it.
            state.damage.invalidate();
            return false;
        }
        if let Some(ack) = state.pending_acks.pop_front() {
            // Ack each per-drawable damage snapshot. Snapshots
            // from paint that landed after the tick's peek
            // survive (per I5 epoch semantics).
            for snap in ack.drawable_snapshots {
                if tick_skip_log_enabled() {
                    log::info!(
                        "ack-diag: out{output_idx} acked drawable={:?} epoch={} rects={}",
                        snap.id,
                        snap.epoch,
                        snap.region.rects().len(),
                    );
                }
                store.ack_presentation_damage(snap);
            }
            // Subtract the submitted output-damage snapshots
            // from live state (codex round 2 point 1). Damage
            // that arrived between submit and retirement
            // (map/unmap/cursor-move while flip in flight) is
            // NOT in the snapshots and therefore survives,
            // driving the next tick.
            state
                .scene_structure_damage
                .subtract(&ack.submitted_scene_structure_damage);
            state
                .pending_repaint_after_failed_submit
                .subtract(&ack.submitted_failed_repaint);
            // Push this frame's output damage onto the
            // buffer-age history ring keyed by its generation.
            state
                .damage_history
                .push(ack.generation, ack.submitted_output_damage);
            // Step 3 — the staged frame reached the screen: its damage is now
            // stale in every OTHER BO, and what it painted is no longer missing
            // from the one it painted into. A no-op when nothing was staged,
            // which is what makes it safe to call on a copied output too.
            state.damage.retire_success();
            // Step 2 — the frame reached the screen, so it becomes the baseline
            // the next diff runs against.
            state.prev_presented = ack.submitted_participants;
            // Release the matching pool slot — but only after the
            // compose CB's Vulkan fence has signaled. Pageflip
            // retirement is driven by KMS VBLANK, not by GPU
            // completion; if the GPU is still executing the compose
            // CB when this pageflip lands, releasing the pool slot
            // immediately calls vkResetDescriptorPool on a pool
            // whose descriptors are still bound to that CB
            // (VUID-vkResetDescriptorPool-descriptorPool-00313).
            // Fence-gate: if signaled now, release immediately;
            // otherwise defer to `pending_pool_releases` for the
            // drain pass to handle on a later tick.
            if let Some(slot) = state.pool_slots.pop_front() {
                match &ack.ticket {
                    None => state.pool_ring.release(slot),
                    Some(t) => match t.poll_signaled_result(&inner.vk) {
                        Ok(true) => state.pool_ring.release(slot),
                        Ok(false) => {
                            state.pending_pool_releases.push_back((slot, t.clone()));
                        }
                        Err(error) => {
                            log::error!(
                                "render scene: compose fence status failed at pageflip \
                                 retirement: {error:?}"
                            );
                            platform.renderer_failed = true;
                            state.pending_pool_releases.push_back((slot, t.clone()));
                        }
                    },
                }
            }
            // Commit the BO's new last_present_generation in the
            // platform (the buffer-age pick uses this on next
            // acquire).
            platform.commit_bo_present(output_idx, retire.presented_bo_idx, ack.generation);

            // The KMS frame retired, but the cursor transition itself is a
            // second transaction. Record what actually happened: never claim
            // Hw after a rolled-back show and never claim Sw/Hidden while a
            // failed hide or rollback left the hardware sprite visible.
            let cursor_result = apply_cursor_transition_on_retire(
                inner,
                output_idx,
                platform,
                ack.cursor_transition,
            );
            let state = inner
                .outputs
                .get_mut(output_idx)
                .expect("range checked above");
            let resolution =
                resolve_retired_cursor_state(cursor_result, ack.cursor_mode_after_retire);
            state.force_show_retry_version = update_force_show_retry_version(
                state.force_show_retry_version,
                ack.cursor_transition,
                cursor_result,
                ack.cursor_mode_after_retire,
            );
            state.last_frame_cursor_mode = resolution.actual_mode;
            if resolution.commit_desired_metadata {
                if let Some(new_prev) = ack.cursor_prev_pos_after_retire {
                    state.cursor_prev_pos = new_prev;
                }
                state.last_present_cursor_rect = ack.last_present_cursor_rect_after_retire;
                state.last_present_cursor_version = ack.last_present_cursor_version_after_retire;
            } else {
                state.cursor_prev_pos = None;
                if resolution.clear_presented_metadata {
                    state.last_present_cursor_rect = None;
                    state.last_present_cursor_version = None;
                }
                // Otherwise preserve last-known metadata: the requested bind
                // did not fully land, so claiming the desired rect/version
                // could make later same-position damage incorrectly skip.
            }
            if resolution.force_repaint {
                force_cursor_retry_repaint(state);
            }
            let actually_sw_composed =
                matches!(ack.cursor_mode_after_retire, OutputCursorMode::Sw { .. });
            if actually_sw_composed && platform.cursor_plane_note_composed_retirement(output_idx) {
                // Consume this output/device's own EINVAL backoff one
                // composed retirement at a time. Damage keeps the sequence
                // alive until the retry becomes eligible.
                force_cursor_retry_repaint(state);
            }
            true
        } else {
            log::debug!(
                "render scene: page-flip-complete on output {output_idx} \
                 with no pending ack — startup flush or spurious event",
            );
            false
        }
    }

    /// Advance one copied frame from source-render completion on renderer A
    /// to the copy + KMS submission on sink renderer B. Stable output identity,
    /// monotonic job id, and the paired BO index must all match the ledger; a
    /// stale notification never retargets a rebuilt output vector.
    pub(crate) fn handle_scanout_render_completion(
        &mut self,
        completion: ReadyScanoutRenderCompletion,
        platform: &mut PlatformBackend,
    ) -> bool {
        let Some(inner) = self.inner.as_mut() else {
            return false;
        };
        handle_scanout_render_completion_inner(inner, completion, platform)
    }
}

pub(super) fn handle_scanout_render_completion_inner(
    inner: &mut SceneCompositorInner,
    completion: ReadyScanoutRenderCompletion,
    platform: &mut PlatformBackend,
) -> bool {
    if platform.renderer_failed {
        return false;
    }
    let ReadyScanoutRenderCompletion {
        job_id,
        output_key,
        bo_idx,
        fd,
    } = completion;
    let Some(output_idx) = platform
        .outputs
        .iter()
        .position(|output| output.key == output_key)
    else {
        log::debug!(
            "render copied scanout: completion job {job_id} targeted removed output \
                 {output_key:?}",
        );
        return false;
    };
    let expected = inner
        .outputs
        .get(output_idx)
        .and_then(|state| state.pending_acks.front())
        .is_some_and(|ack| copied_render_completion_matches(ack.stage, ack.bo_idx, job_id, bo_idx));
    if !expected {
        log::warn!(
            "render copied scanout: stale completion job {job_id} for output \
                 {output_idx} bo {bo_idx}",
        );
        // A live output retaining a different ledger entry means the
        // completed source can no longer be associated safely. Fail-stop
        // rather than guessing a pool slot or making either device's
        // allocation reusable while GPU ownership is uncertain.
        platform.renderer_failed = true;
        return false;
    }

    match platform.submit_copied_scanout(output_idx, bo_idx, fd) {
        Ok(()) => {
            if let Some(ack) = inner.outputs[output_idx].pending_acks.front_mut() {
                ack.stage = InFlightStage::KmsFlipPending;
            }
            true
        }
        Err(error) => {
            log::warn!(
                "render copied scanout: sink copy/KMS submit failed for output \
                     {output_idx} bo {bo_idx}: {error}",
            );
            // The platform/resource layer synchronously idles B before
            // recovery and quarantines the pair if quiescence fails.
            // The scene may retire only A-local descriptor resources, and
            // only behind A's compose fence; damage/cursor metadata remain
            // live for a later repaint because no frame reached the screen.
            platform.invalidate_bo(output_idx, bo_idx);
            let state = &mut inner.outputs[output_idx];
            let ack = state
                .pending_acks
                .pop_front()
                .expect("completion identity was checked against the front ack");
            state.current_generation = ack.generation.saturating_sub(1);
            if let Some(rect) = ack.submitted_output_damage.bounding_rect() {
                state.pending_repaint_after_failed_submit.add(rect);
            }
            if let Some(slot) = state.pool_slots.pop_front() {
                match ack.ticket {
                    None => match platform.recycle_failed_submit_bo(output_idx, bo_idx) {
                        Ok(()) => state.pool_ring.release(slot),
                        Err(recovery_error) => {
                            log::error!(
                                "render copied scanout: renderer recovery failed for output \
                                     {output_idx} bo {bo_idx}: {recovery_error}"
                            );
                            platform.renderer_failed = true;
                        }
                    },
                    Some(ticket) => {
                        // Even if the fence is already signalled, route
                        // all post-A-submit failures through the same
                        // deferred recycler. That recycler rearms dirty
                        // export semaphores, retires the consumed prior
                        // B->A wait, and only then frees the paired slot.
                        state.failed_submit_bos.push_back(FailedSubmitBo {
                            bo_idx,
                            pool_slot: slot,
                            ticket,
                        });
                    }
                }
            } else {
                log::error!(
                    "render copied scanout: missing descriptor slot for failed output \
                         {output_idx} bo {bo_idx}"
                );
                platform.renderer_failed = true;
            }
            state.next_submit_retry_at =
                Some(std::time::Instant::now() + std::time::Duration::from_millis(100));
            false
        }
    }
}

pub(super) fn retire_failed_submit_bos(
    state: &mut OutputSceneState,
    output_idx: usize,
    platform: &mut PlatformBackend,
    vk: &crate::kms::vk::device::VkContext,
) {
    let mut remaining = VecDeque::with_capacity(state.failed_submit_bos.len());
    while let Some(failed) = state.failed_submit_bos.pop_front() {
        match failed.ticket.poll_signaled_result(vk) {
            Ok(true) => match platform.recycle_failed_submit_bo(output_idx, failed.bo_idx) {
                Ok(()) => {
                    state.pool_ring.release(failed.pool_slot);
                    log::debug!(
                        "render scene: recycled failed-submit output {output_idx} bo {} pool slot {}",
                        failed.bo_idx,
                        failed.pool_slot,
                    );
                }
                Err(error) => {
                    log::error!(
                        "render scene: failed-submit recovery failed for output {output_idx} \
                         bo {}: {error}",
                        failed.bo_idx,
                    );
                    platform.renderer_failed = true;
                    remaining.push_back(failed);
                }
            },
            Ok(false) => remaining.push_back(failed),
            Err(error) => {
                log::error!(
                    "render scene: failed-submit fence status failed for output {output_idx} \
                     bo {}: {error:?}",
                    failed.bo_idx,
                );
                platform.renderer_failed = true;
                remaining.push_back(failed);
            }
        }
    }
    state.failed_submit_bos = remaining;
}

/// Drain the deferred descriptor-pool slot releases queued by
/// `handle_page_flip_complete` when the compose fence hadn't yet
/// signaled at pageflip-retirement time. Mirrors
/// `retire_failed_submit_bos`'s walk-once-poll-or-defer shape.
/// Slots whose fence has now signaled are returned to the ring;
/// the rest stay queued for the next drain.
pub(super) fn drain_pending_pool_releases(
    state: &mut OutputSceneState,
    vk: &crate::kms::vk::device::VkContext,
    platform: &mut PlatformBackend,
) {
    if state.pending_pool_releases.is_empty() {
        return;
    }
    let mut remaining = VecDeque::with_capacity(state.pending_pool_releases.len());
    while let Some((slot, ticket)) = state.pending_pool_releases.pop_front() {
        match ticket.poll_signaled_result(vk) {
            Ok(true) => state.pool_ring.release(slot),
            Ok(false) => remaining.push_back((slot, ticket)),
            Err(error) => {
                log::error!("render scene: deferred pool fence status failed: {error:?}");
                platform.renderer_failed = true;
                remaining.push_back((slot, ticket));
            }
        }
    }
    state.pending_pool_releases = remaining;
}
