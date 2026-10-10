use super::*;

impl PlatformBackend {
    // ── I6b: scanout BO management ──────────────────────────────

    pub(super) fn debug_assert_scanout_pool_route(&self, output_idx: usize) {
        if let Some(pool) = self.scanout_pools.get(output_idx).and_then(Option::as_ref) {
            debug_assert_eq!(
                self.outputs
                    .get(output_idx)
                    .map(|output| output.scanout_route),
                Some(pool.route()),
                "scanout pool must stay paired with its output's renderer-to-KMS route"
            );
        }
    }

    /// Pick the next BO to render into for `output_idx`, or
    /// `None` if all BOs are still in flight (the SceneCompositor
    /// should retry next core-loop iteration).
    ///
    /// The token carries `last_present_generation` and
    /// `content_invalidated` so the buffer-age algorithm in
    /// SceneCompositor doesn't need to reach into the pool.
    pub(crate) fn acquire_scanout_bo(&mut self, output_idx: usize) -> Option<ScanoutBoToken> {
        self.debug_assert_scanout_pool_route(output_idx);
        let scanout = self.scanout_pools.get_mut(output_idx)?.as_mut()?;
        let gens = self.bo_generations.get(output_idx)?;
        for (bo_idx, bo) in scanout.display_pool().bos.iter().enumerate() {
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

    /// Advance one copied frame from a completed source-render submission to
    /// the sink copy and KMS page flip.  Poll readiness is only a scheduling
    /// boundary: the source `sync_file` is still imported and waited by the
    /// sink Vulkan submission, whose exported completion becomes KMS's
    /// `IN_FENCE_FD`.
    pub(crate) fn submit_copied_scanout(
        &mut self,
        output_idx: usize,
        bo_idx: usize,
        render_completion: Option<OwnedFd>,
    ) -> io::Result<()> {
        self.debug_assert_scanout_pool_route(output_idx);
        let output_key = self
            .outputs
            .get(output_idx)
            .map(|output| output.key.clone())
            .ok_or_else(|| io::Error::other("copied scanout output index out of range"))?;
        let device = self
            .device_for_output(&output_key)
            .map(|device| Rc::clone(&device.device))
            .ok_or_else(|| io::Error::other("copied scanout KMS device disappeared"))?;

        let mut recovery_failed = false;
        let result = (|| {
            // `Output` deliberately is not Clone: its DRM handles belong to
            // the owning open fd. Borrow the output and its independently
            // indexed scanout pool through disjoint PlatformBackend fields.
            let output = &self
                .outputs
                .get(output_idx)
                .ok_or_else(|| io::Error::other("copied scanout output disappeared"))?
                .output;
            let copied = self
                .scanout_pools
                .get_mut(output_idx)
                .and_then(Option::as_mut)
                .and_then(OutputScanout::copied_mut)
                .ok_or_else(|| io::Error::other("render completion targeted a shared output"))?;
            let framebuffer = copied
                .destinations
                .bos
                .get(bo_idx)
                .ok_or_else(|| io::Error::other("copied destination index out of range"))?
                .fb_handle
                .ok_or_else(|| io::Error::other("copied destination has no framebuffer"))?;

            let copy_completion = match copied.submit_copy(bo_idx, render_completion) {
                Ok(fd) => fd,
                Err(error) => {
                    if let Err(recovery_error) = copied.recover_copy_failure(bo_idx) {
                        recovery_failed = true;
                        log::error!(
                            "copied scanout submission failed ({error}) and the sink could not be quiesced: {recovery_error}"
                        );
                        return Err(io::Error::new(recovery_error.kind(), recovery_error));
                    }
                    return Err(error);
                }
            };
            let destination = copied
                .destinations
                .bos
                .get_mut(bo_idx)
                .expect("copied destination was checked before copy submission");
            let in_fence_fd = copy_completion.map_or(-1, IntoRawFd::into_raw_fd);
            destination.state.transition_to_submitted(in_fence_fd);
            let mut out_fence_fd = -1;
            match crate::drm::page_flip::submit_flip_with_fences(
                &device,
                output,
                framebuffer,
                in_fence_fd,
                &mut out_fence_fd,
            ) {
                Ok(()) => {
                    if let Some(fd) = destination.state.transition_to_pending(out_fence_fd) {
                        // SAFETY: transition_to_pending transfers the uniquely
                        // owned input-fence fd back to this caller.
                        unsafe { libc::close(fd) };
                    }
                    Ok(())
                }
                Err(error) => {
                    if let Some(fd) = destination
                        .state
                        .transition_to_recording_after_atomic_reject()
                    {
                        // SAFETY: the state transition returns unique fd
                        // ownership after the rejected atomic commit.
                        unsafe { libc::close(fd) };
                    }
                    if out_fence_fd >= 0 {
                        // SAFETY: the kernel wrote a uniquely-owned fd into
                        // our out-fence slot even though the commit failed.
                        unsafe { libc::close(out_fence_fd) };
                    }
                    if let Err(recovery_error) = copied.recover_copy_failure(bo_idx) {
                        recovery_failed = true;
                        log::error!(
                            "copied scanout atomic commit failed ({error}) and the sink could not be quiesced: {recovery_error}"
                        );
                        Err(io::Error::new(recovery_error.kind(), recovery_error))
                    } else {
                        Err(error)
                    }
                }
            }
        })();

        if recovery_failed
            || result
                .as_ref()
                .is_err_and(crate::kms::vk::scanout::scanout_error_is_device_lost)
        {
            // The failing operation used a live renderer or sink transfer
            // device.  Neither uncertain image state may be reused.
            self.renderer_failed = true;
        }
        result
    }

    /// Framebuffer last presented by the scene compositor for this output.
    /// During M2 direct scanout the pool deliberately keeps this BO marked
    /// `OnScreen`: it is the known-good per-output target for one atomic
    /// transition back from the shared client framebuffer.
    pub(crate) fn retained_composed_framebuffer(
        &self,
        output_idx: usize,
    ) -> Option<::drm::control::framebuffer::Handle> {
        self.debug_assert_scanout_pool_route(output_idx);
        self.scanout_pools
            .get(output_idx)?
            .as_ref()?
            .display_pool()
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
    pub(crate) fn recycle_failed_submit_bo(
        &mut self,
        output_idx: usize,
        bo_idx: usize,
    ) -> io::Result<()> {
        self.debug_assert_scanout_pool_route(output_idx);
        let Some(scanout) = self
            .scanout_pools
            .get_mut(output_idx)
            .and_then(Option::as_mut)
        else {
            return Ok(());
        };
        match scanout {
            OutputScanout::Shared(pool) => {
                let Some(bo) = pool.bos.get_mut(bo_idx) else {
                    return Ok(());
                };
                bo.rearm_export_semaphore_after_quiescence()
                    .map_err(|result| {
                        io::Error::other(format!(
                            "rearm shared scanout export semaphore after failed handoff: {result:?}",
                        ))
                    })?;
                bo.state = BoState::default();
            }
            OutputScanout::Copied(pool) => {
                let source = pool
                    .sources
                    .get_mut(bo_idx)
                    .ok_or_else(|| io::Error::other("copied source index out of range"))?;
                source.recover_failed_cycle_after_renderer_quiescence()?;
                let destination = pool
                    .destinations
                    .bos
                    .get_mut(bo_idx)
                    .ok_or_else(|| io::Error::other("copied destination index out of range"))?;
                destination.state = BoState::default();
            }
        }
        Ok(())
    }

    /// Abandon a target that never reached `vkQueueSubmit2`. No semaphore
    /// payload or ownership release was executed, so only the display-pool
    /// reservation needs to be undone. Any prepared temporary import remains
    /// valid and may be reused by the next recording attempt.
    pub(crate) fn cancel_scanout_bo_recording(&mut self, output_idx: usize, bo_idx: usize) {
        self.debug_assert_scanout_pool_route(output_idx);
        let Some(bo) = self
            .scanout_pools
            .get_mut(output_idx)
            .and_then(Option::as_mut)
            .and_then(|pool| pool.display_pool_mut().bos.get_mut(bo_idx))
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
    pub(crate) fn reset_scanout_bos_for_suspend(&mut self) -> io::Result<()> {
        self.clear_scanout_render_completions();
        let mut first_error = None;
        for output_idx in 0..self.scanout_pools.len() {
            if let Err(error) = self.drain_scanout_pool_at(output_idx) {
                log::error!(
                    "suspend could not quiesce scanout output {output_idx}: {error}; \
                     copied resources remain quarantined"
                );
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        for gens in &mut self.bo_generations {
            for g in gens {
                g.last_present_generation = None;
                g.content_invalidated = true;
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Quiesce one output's renderer and (for copied scanout) sink devices.
    /// Any failure makes the live renderer path unusable; the copied pool
    /// keeps uncertain resources quarantined and its Drop path leaks them
    /// instead of freeing memory that a GPU may still reference.
    pub(super) fn drain_scanout_pool_at(&mut self, output_idx: usize) -> io::Result<()> {
        let Some(vk) = self.vk.clone() else {
            return Ok(());
        };
        let result = self
            .scanout_pools
            .get_mut(output_idx)
            .and_then(Option::as_mut)
            .map_or(Ok(()), |pool| pool.drain_all_pending(&vk));
        if result.is_err() {
            self.renderer_failed = true;
        }
        result
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
pub(super) fn check_scanout_liveness(
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
