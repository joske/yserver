//! `PlatformBackend` as the scene's output and cursor-plane sink: thin
//! wrappers over the scanout BO, page-flip, render-completion and cursor-plane
//! methods, plus the frame sequence of `finish_frame` around the scene's
//! record step and the scanout targets the scene composes into.

use crate::kms::{
    render::{
        gpu::GpuCore,
        platform::PlatformBackend,
        scene::{
            ComposeRenderTarget, ComposeSubmit, CursorPlaneSink, CursorShowFailure,
            CursorUploadOutcome, FinishOutcome, FlipRetirement, FrameHandoff, FrameTargetError,
            OutputRoute, OutputSink, RecordFrame, RenderCompletion, SlotToken,
        },
    },
    vk::{
        compositor::PresentError,
        scanout::{BoPhase, BoState, CopiedRenderSource, OutputScanout, ScanoutBo},
    },
};
use ash::vk;
use std::{io, os::fd::OwnedFd, rc::Rc};

impl OutputSink for PlatformBackend {
    fn gpu(&self) -> &GpuCore {
        &self.gpu
    }

    fn gpu_mut(&mut self) -> &mut GpuCore {
        &mut self.gpu
    }

    fn cursor_plane(&self) -> &dyn CursorPlaneSink {
        self
    }

    fn cursor_plane_mut(&mut self) -> &mut dyn CursorPlaneSink {
        self
    }

    fn route(&self, output_idx: usize) -> OutputRoute {
        match self.scanout_pools.get(output_idx).and_then(Option::as_ref) {
            Some(OutputScanout::Shared(_)) => OutputRoute::Shared,
            Some(OutputScanout::Copied(_)) => OutputRoute::Copied,
            None => OutputRoute::None,
        }
    }

    fn output_commit_in_flight(&self, output_idx: usize) -> bool {
        self.cursor_commit_in_flight_for_output(output_idx)
    }

    fn slot_count(&self, output_idx: usize) -> Option<usize> {
        self.scanout_pools
            .get(output_idx)
            .and_then(|p| p.as_ref().map(|pool| pool.display_pool().bos.len()))
    }

    fn acquire_slot(&mut self, output_idx: usize) -> Option<SlotToken> {
        self.acquire_scanout_bo(output_idx)
    }

    fn slot_image(&self, output_idx: usize, bo_idx: usize) -> Option<vk::Image> {
        match self.scanout_pools.get(output_idx).and_then(|p| p.as_ref()) {
            Some(OutputScanout::Shared(pool)) => pool.bos.get(bo_idx).map(|bo| bo.vk_image),
            Some(OutputScanout::Copied(pool)) => {
                pool.sources.get(bo_idx).map(|source| source.image())
            }
            None => None,
        }
    }

    fn check_frame_target(&self, output_idx: usize) -> Result<(), FrameTargetError> {
        let output_key = &self.outputs[output_idx].key;
        if self.device_for_output(output_key).is_none() {
            return Err(FrameTargetError::NoDevice(io::Error::other(format!(
                "no DRM device for output {output_key:?}"
            ))));
        }
        if self
            .scanout_pools
            .get(output_idx)
            .and_then(Option::as_ref)
            .is_none()
        {
            return Err(FrameTargetError::NoSlot);
        }
        Ok(())
    }

    fn finish_frame(
        &mut self,
        output_idx: usize,
        bo_idx: usize,
        record: &mut RecordFrame<'_>,
    ) -> FinishOutcome {
        let output_key = &self.outputs[output_idx].key;
        let Some(drm_device) = self
            .device_for_output(output_key)
            .map(|device| Rc::clone(&device.device))
        else {
            return FinishOutcome::Unavailable(FrameTargetError::NoDevice(io::Error::other(
                format!("no DRM device for output {output_key:?}"),
            )));
        };
        let Some(atomic_commit_sink) = self
            .device_for_output(output_key)
            .map(|device| Rc::clone(&device.atomic_commit_sink))
        else {
            return FinishOutcome::Unavailable(FrameTargetError::NoDevice(io::Error::other(
                format!("no atomic commit sink for output {output_key:?}"),
            )));
        };
        let cursor_positions = self.cursor_plane_atomic_positions_for_outputs(&[output_idx]);
        let timing = self
            .scene_flip_timings
            .get(output_idx)
            .copied()
            .flatten()
            .unwrap_or(self.requested_flip_timing);
        let async_ledger = &mut self.async_ledger;
        let tearing_context = crate::drm::atomic::TearingCommitContext::composed(
            self.tearing_copy_received_at,
            self.tearing_copy_received_monotonic,
        );
        let Some(pool) = self
            .scanout_pools
            .get_mut(output_idx)
            .and_then(Option::as_mut)
        else {
            return FinishOutcome::Unavailable(FrameTargetError::NoSlot);
        };
        let output = &self.outputs[output_idx].output;
        let mut shared_cursor_commit_submitted = false;
        let mut cursor_rejection_error = None;
        let mut async_outcome = None;
        let outcome = match pool {
            OutputScanout::Shared(pool) if self.defer_scene_flips => {
                let Some(bo) = pool.bos.get_mut(bo_idx) else {
                    return FinishOutcome::Unavailable(FrameTargetError::NoSlot);
                };
                // The caller commits every deferred frame in one transaction
                // and then installs the accepted event identity on the ack.
                let result = render_shared_scanout_frame(bo, record).map(|(submit, _, _)| {
                    self.deferred_scene_flips
                        .push((output_idx, bo_idx, submit.is_complete()));
                    FrameHandoff::FlipPending {
                        submit,
                        event_id: None,
                    }
                });
                FinishOutcome::Finished {
                    result,
                    previous_gpu_ns: bo.last_gpu_render_ns.take(),
                    fail_stop: false,
                }
            }
            OutputScanout::Shared(pool) => {
                let allocation_plan = pool.allocation_plan;
                let Some(bo) = pool.bos.get_mut(bo_idx) else {
                    return FinishOutcome::Unavailable(FrameTargetError::NoSlot);
                };
                let submitted = submit_shared_scanout_frame(
                    &drm_device,
                    atomic_commit_sink.as_ref(),
                    output,
                    allocation_plan,
                    bo,
                    record,
                    &cursor_positions,
                    async_ledger,
                    timing,
                    tearing_context,
                );
                if let Err(error) = &submitted
                    && atomic_commit_sink.take_last_cursor_rejection()
                {
                    cursor_rejection_error = Some(error.to_string());
                }
                if let Ok((_, result, _)) = &submitted {
                    async_outcome = Some(*result);
                    shared_cursor_commit_submitted =
                        *result != crate::drm::async_flip::AsyncOutcome::Accepted;
                }
                let result =
                    submitted.map(
                        |(submit, _async_outcome, event_id)| FrameHandoff::FlipPending {
                            submit,
                            event_id: Some(event_id),
                        },
                    );
                FinishOutcome::Finished {
                    result,
                    previous_gpu_ns: bo.last_gpu_render_ns.take(),
                    fail_stop: false,
                }
            }
            OutputScanout::Copied(pool) => {
                debug_assert!(timing == crate::drm::async_flip::FlipTiming::Vsync);
                let Some(source) = pool.sources.get_mut(bo_idx) else {
                    return FinishOutcome::Unavailable(FrameTargetError::NoSlot);
                };
                let Some(destination) = pool.destinations.bos.get_mut(bo_idx) else {
                    return FinishOutcome::Unavailable(FrameTargetError::NoSlot);
                };
                let result = submit_copied_scanout_render(source, &mut destination.state, record);
                let fail_stop = result
                    .as_ref()
                    .is_err_and(CopiedRenderSubmitError::requires_fail_stop);
                FinishOutcome::Finished {
                    result: result
                        .map(FrameHandoff::RenderCompletion)
                        .map_err(CopiedRenderSubmitError::into_present),
                    previous_gpu_ns: source.last_gpu_render_ns.take(),
                    fail_stop,
                }
            }
        };
        if let Some(async_outcome) = async_outcome {
            self.async_outcomes.insert(
                crate::drm::async_flip::AsyncStream::Composed(output.crtc),
                async_outcome,
            );
            // The receipt belongs to this damage once any commit for it is
            // accepted. An EINVAL retry can successfully submit Vsync, and
            // later damage must not inherit the old frame's timestamp.
            if self.tearing_copy_received_at.is_some() {
                self.tearing_copy_received_at = None;
                self.tearing_copy_received_monotonic = None;
            }
        }
        if shared_cursor_commit_submitted {
            self.cursor_plane_scene_commit_submitted(&cursor_positions);
        }
        if let Some(error) = cursor_rejection_error {
            let rejected = self.reject_atomic_cursor_commit(&cursor_positions, &error);
            self.defer_cursor_move_outcome(rejected);
        }
        outcome
    }

    fn invalidate_slot(&mut self, output_idx: usize, bo_idx: usize) {
        self.invalidate_bo(output_idx, bo_idx);
    }

    fn cancel_slot(&mut self, output_idx: usize, bo_idx: usize) {
        self.cancel_scanout_bo_recording(output_idx, bo_idx);
    }

    fn recycle_failed_slot(&mut self, output_idx: usize, bo_idx: usize) -> io::Result<()> {
        self.recycle_failed_submit_bo(output_idx, bo_idx)
    }

    fn on_page_flip_complete(&mut self, output_idx: usize) -> Option<FlipRetirement> {
        PlatformBackend::on_page_flip_complete(self, output_idx)
    }

    fn commit_slot_present(&mut self, output_idx: usize, bo_idx: usize, generation: u64) {
        self.commit_bo_present(output_idx, bo_idx, generation);
    }

    fn register_render_completion(
        &mut self,
        output_idx: usize,
        bo_idx: usize,
        fd: Option<OwnedFd>,
    ) -> io::Result<u64> {
        let output_key = self.outputs[output_idx].key.clone();
        self.register_scanout_render_completion(output_key, bo_idx, fd)
    }

    fn drain_render_completions(&mut self) -> Vec<RenderCompletion> {
        self.drain_scanout_render_completions()
            .into_iter()
            .map(|completion| RenderCompletion {
                job_id: completion.job_id,
                output: self
                    .outputs
                    .iter()
                    .position(|output| output.key == completion.output_key)
                    .ok_or_else(|| format!("{:?}", completion.output_key)),
                bo_idx: completion.bo_idx,
                fd: completion.fd,
            })
            .collect()
    }

    fn clear_render_completions(&mut self) {
        self.clear_scanout_render_completions();
    }

    fn submit_copied(
        &mut self,
        output_idx: usize,
        bo_idx: usize,
        fd: Option<OwnedFd>,
    ) -> io::Result<Option<u64>> {
        self.submit_copied_scanout(output_idx, bo_idx, fd).map(Some)
    }
}

impl PlatformBackend {
    /// Select the timing used by the shared scene-flip route for this output.
    /// `maybe_composite` resets every output to Vsync before making any
    /// admission decision, so an earlier pass cannot leave Async behind.
    pub(crate) fn set_scene_flip_timing(
        &mut self,
        output_idx: usize,
        timing: crate::drm::async_flip::FlipTiming,
    ) {
        if self.scene_flip_timings.len() <= output_idx {
            self.scene_flip_timings.resize(output_idx + 1, None);
        }
        self.scene_flip_timings[output_idx] = Some(timing);
    }

    /// Return the exact shape used by the shared composed-flip submission
    /// path. Copied scanout never requests Async.
    pub(crate) fn scene_async_shape(
        &self,
        output_idx: usize,
    ) -> Option<crate::drm::async_flip::AsyncShape> {
        let output = &self.outputs.get(output_idx)?.output;
        let Some(Some(OutputScanout::Shared(pool))) = self.scanout_pools.get(output_idx) else {
            return None;
        };
        let bo = pool.bos.first()?;
        Some(composed_async_shape(
            output,
            pool.allocation_plan,
            bo.pitch,
            bo.width,
            bo.height,
        ))
    }
}

fn composed_async_shape(
    output: &crate::drm::modeset::Output,
    allocation_plan: crate::kms::vk::scanout::ScanoutAllocationPlan,
    pitch: u32,
    width: u32,
    height: u32,
) -> crate::drm::async_flip::AsyncShape {
    const DRM_FORMAT_XRGB8888: u32 = 0x3432_5258;
    let modifier = match allocation_plan {
        crate::kms::vk::scanout::ScanoutAllocationPlan::GbmModifier(modifier)
        | crate::kms::vk::scanout::ScanoutAllocationPlan::DrmModifier(modifier) => modifier,
        crate::kms::vk::scanout::ScanoutAllocationPlan::PaddedExplicitLinear { .. }
        | crate::kms::vk::scanout::ScanoutAllocationPlan::ExplicitLinear
        | crate::kms::vk::scanout::ScanoutAllocationPlan::LegacyLinear => 0,
    };
    crate::drm::async_flip::AsyncShape {
        fourcc: DRM_FORMAT_XRGB8888,
        modifier,
        pitch,
        width,
        height,
        crtcs: vec![output.crtc],
        direct: false,
    }
}

impl CursorPlaneSink for PlatformBackend {
    fn fits(&self, output_idx: usize, width: u32, height: u32) -> bool {
        self.cursor_plane_fits_for_output(output_idx, width, height)
    }

    fn visible(&self, output_idx: usize) -> bool {
        self.cursor_plane_visible_for_output(output_idx)
    }

    fn uploaded_image_matches(
        &self,
        output_idx: usize,
        version: u64,
        hot_x: u16,
        hot_y: u16,
    ) -> bool {
        self.cursor_plane_uploaded_image_matches_for_output(output_idx, version, hot_x, hot_y)
    }

    fn upload(
        &mut self,
        output_idx: usize,
        version: u64,
        hot_x: u16,
        hot_y: u16,
        width: u32,
        height: u32,
        bgra_bytes: &[u8],
    ) -> io::Result<CursorUploadOutcome> {
        self.cursor_plane_upload_image_for_output(
            output_idx, version, hot_x, hot_y, width, height, bgra_bytes,
        )
    }

    fn show(
        &mut self,
        output_idx: usize,
        hot_x: u16,
        hot_y: u16,
        x: i32,
        y: i32,
    ) -> Result<(), CursorShowFailure> {
        self.cursor_plane_show_on_crtc(output_idx, hot_x, hot_y, x, y)
            .map_err(|error| CursorShowFailure {
                remains_visible: error.remains_visible(),
                deferred: error.is_deferred(),
                error: Box::new(error),
            })
    }

    fn hide(&mut self, output_idx: usize) -> io::Result<()> {
        let result = self.cursor_plane_hide_on_crtc(output_idx);
        if result.is_err() {
            self.scene_cursor_hide_failed = true;
        }
        result
    }

    fn hide_all(&mut self) -> io::Result<()> {
        self.cursor_plane_hide_all()
    }

    fn note_composed_retirement(&mut self, output_idx: usize) -> bool {
        self.cursor_plane_note_composed_retirement(output_idx)
    }
}

/// A copied frame's failure, before or after the record.
enum CopiedRenderSubmitError {
    RendererAcquire(io::Error),
    Present(PresentError),
}

impl CopiedRenderSubmitError {
    fn requires_fail_stop(&self) -> bool {
        matches!(self, Self::RendererAcquire(_))
    }

    fn into_present(self) -> PresentError {
        match self {
            Self::RendererAcquire(error) => PresentError::Io(error),
            Self::Present(error) => error,
        }
    }
}

impl ComposeRenderTarget for ScanoutBo {
    fn image(&self) -> vk::Image {
        self.vk_image
    }

    fn image_view(&self) -> vk::ImageView {
        self.vk_image_view
    }

    fn command_buffer(&self) -> vk::CommandBuffer {
        self.vk_transfer.command_buffer
    }

    fn completion_semaphore(&self) -> vk::Semaphore {
        self.vk_semaphore
    }

    fn width(&self) -> u32 {
        self.width
    }

    fn height(&self) -> u32 {
        self.height
    }

    fn timestamp_pool(&self) -> vk::QueryPool {
        self.vk_transfer.timestamp_pool
    }

    fn timestamps_written(&self) -> bool {
        self.vk_transfer.timestamps_written
    }

    fn mark_timestamps_written(&mut self) {
        self.vk_transfer.timestamps_written = true;
    }

    fn set_last_gpu_render_ns(&mut self, value: Option<u64>) {
        self.last_gpu_render_ns = value;
    }

    fn prepare_post_compose(&mut self) -> Result<(), PresentError> {
        Ok(())
    }

    fn record_post_compose(
        &mut self,
        vk: &crate::kms::vk::device::VkContext,
        command_buffer: vk::CommandBuffer,
    ) {
        let to_scanout = [vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
            .src_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
            .dst_access_mask(vk::AccessFlags2::empty())
            .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .new_layout(vk::ImageLayout::GENERAL)
            .image(self.vk_image)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            )];
        crate::vk_count!(cmd_pipeline_barrier2);
        unsafe {
            vk.device.cmd_pipeline_barrier2(
                command_buffer,
                &vk::DependencyInfo::default().image_memory_barriers(&to_scanout),
            );
        }
    }
}

impl ComposeRenderTarget for CopiedRenderSource {
    fn image(&self) -> vk::Image {
        self.image()
    }

    fn image_view(&self) -> vk::ImageView {
        self.image_view()
    }

    fn command_buffer(&self) -> vk::CommandBuffer {
        self.transfer.command_buffer
    }

    fn completion_semaphore(&self) -> vk::Semaphore {
        self.completion_semaphore
    }

    fn width(&self) -> u32 {
        self.width()
    }

    fn height(&self) -> u32 {
        self.height()
    }

    fn timestamp_pool(&self) -> vk::QueryPool {
        self.transfer.timestamp_pool
    }

    fn timestamps_written(&self) -> bool {
        self.transfer.timestamps_written
    }

    fn mark_timestamps_written(&mut self) {
        self.transfer.timestamps_written = true;
    }

    fn set_last_gpu_render_ns(&mut self, value: Option<u64>) {
        self.last_gpu_render_ns = value;
    }

    fn prepare_post_compose(&mut self) -> Result<(), PresentError> {
        let preparation = self.transport_preparation().map_err(PresentError::Io)?;
        self.post_compose_preparation = Some(preparation);
        Ok(())
    }

    fn renderer_wait_semaphore(&self) -> Option<vk::Semaphore> {
        self.renderer_wait_semaphore()
    }

    fn note_submit_succeeded(&mut self) {
        self.note_renderer_submit_succeeded();
    }

    fn record_post_compose(
        &mut self,
        _vk: &crate::kms::vk::device::VkContext,
        command_buffer: vk::CommandBuffer,
    ) {
        let Some(preparation) = self.post_compose_preparation.take() else {
            unreachable!("copied target recorded post-compose without a preparation")
        };
        self.record_transport_copy(command_buffer, preparation);
    }
}

/// Render into a free KMS framebuffer and leave it `Submitted` with its
/// exported render fence held as the `IN_FENCE_FD` of the flip to come.
fn render_shared_scanout_frame(
    bo: &mut ScanoutBo,
    record: &mut RecordFrame<'_>,
) -> Result<(ComposeSubmit, ::drm::control::framebuffer::Handle, i32), PresentError> {
    use std::os::fd::IntoRawFd;

    if bo.state.phase != BoPhase::Free {
        return Err(PresentError::WrongPhase(bo.state.phase));
    }
    let fb_handle = bo.fb_handle.ok_or(PresentError::NoFb)?;
    bo.state.transition_to_recording();
    let submitted = record(bo)?;

    let fd = bo
        .export_signaled_fd()
        .map_err(PresentError::Vk)?
        .map_or(-1, IntoRawFd::into_raw_fd);
    bo.state.transition_to_submitted(fd);
    Ok((submitted, fb_handle, fd))
}

/// Render directly into the KMS framebuffer and immediately queue its flip.
fn submit_shared_scanout_frame(
    drm: &crate::drm::Device,
    atomic_commit_sink: &crate::drm::atomic::AtomicCommitSink,
    output: &crate::platform::drm::Output,
    allocation_plan: crate::kms::vk::scanout::ScanoutAllocationPlan,
    bo: &mut ScanoutBo,
    record: &mut RecordFrame<'_>,
    cursor_positions: &[crate::drm::atomic::CursorPlanePosition],
    async_ledger: &mut crate::drm::async_flip::AsyncFlipLedger,
    timing: crate::drm::async_flip::FlipTiming,
    tearing_context: crate::drm::atomic::TearingCommitContext,
) -> Result<(ComposeSubmit, crate::drm::async_flip::AsyncOutcome, u64), PresentError> {
    use std::os::fd::FromRawFd;

    let (submitted, fb_handle, fd) = render_shared_scanout_frame(bo, record)?;

    let shape = composed_async_shape(output, allocation_plan, bo.pitch, bo.width, bo.height);
    let stream = crate::drm::async_flip::AsyncStream::Composed(output.crtc);
    if timing == crate::drm::async_flip::FlipTiming::Async {
        async_ledger.observe_shape(stream, &shape);
    }
    let attempt_timing = if timing == crate::drm::async_flip::FlipTiming::Async
        && async_ledger.allows(stream, &shape)
    {
        crate::drm::async_flip::FlipTiming::Async
    } else {
        crate::drm::async_flip::FlipTiming::Vsync
    };
    let mut out_fence: i32 = -1;
    let mut submit_result = crate::drm::page_flip::submit_scene_flip_with_fences_tearing(
        drm,
        atomic_commit_sink,
        output,
        fb_handle,
        fd,
        &mut out_fence,
        cursor_positions,
        attempt_timing,
        tearing_context,
    );
    let async_outcome = if attempt_timing == crate::drm::async_flip::FlipTiming::Async {
        match submit_result {
            Ok(accepted_id) => {
                async_ledger.note_accepted(stream, &shape);
                submit_result = Ok(accepted_id);
                crate::drm::async_flip::AsyncOutcome::Accepted
            }
            Err(error) if error.raw_os_error() == Some(libc::EINVAL) => {
                let suspended = async_ledger.note_einval(stream, &shape);
                let retry_fd = bo
                    .state
                    .transition_to_recording_after_atomic_reject()
                    .unwrap_or(-1);
                bo.state.transition_to_submitted(retry_fd);
                out_fence = -1;
                submit_result = crate::drm::page_flip::submit_scene_flip_with_fences_tearing(
                    drm,
                    atomic_commit_sink,
                    output,
                    fb_handle,
                    retry_fd,
                    &mut out_fence,
                    cursor_positions,
                    crate::drm::async_flip::FlipTiming::Vsync,
                    crate::drm::atomic::TearingCommitContext {
                        async_fallback: true,
                        ..tearing_context
                    },
                );
                crate::drm::async_flip::AsyncOutcome::FellBack { suspended }
            }
            Err(error) => {
                submit_result = Err(error);
                crate::drm::async_flip::AsyncOutcome::NotRequested
            }
        }
    } else {
        crate::drm::async_flip::AsyncOutcome::NotRequested
    };

    match submit_result {
        Ok(event_id) => {
            if let Some(reclaimed) = bo.state.transition_to_pending(out_fence) {
                // SAFETY: this is the in-fence fd retained across any async
                // EINVAL retry and returned after atomic acceptance.
                drop(unsafe { std::os::fd::OwnedFd::from_raw_fd(reclaimed) });
            }
            Ok((submitted, async_outcome, event_id))
        }
        Err(error) => {
            if let Some(reclaimed) = bo.state.transition_to_recording_after_atomic_reject() {
                // SAFETY: same fd inserted by `transition_to_submitted`.
                drop(unsafe { std::os::fd::OwnedFd::from_raw_fd(reclaimed) });
            }
            if out_fence >= 0 {
                // Defensive: OUT_FENCE_PTR should only be written on success.
                drop(unsafe { std::os::fd::OwnedFd::from_raw_fd(out_fence) });
            }
            Err(PresentError::Io(error))
        }
    }
}

/// Render into A's exportable source. The paired destination phase reserves
/// the same BO index until readiness advances the frame to B's copy + KMS
/// submission on the main-loop boundary.
fn submit_copied_scanout_render(
    source: &mut CopiedRenderSource,
    destination_state: &mut BoState,
    record: &mut RecordFrame<'_>,
) -> Result<Option<OwnedFd>, CopiedRenderSubmitError> {
    if destination_state.phase != BoPhase::Free {
        return Err(CopiedRenderSubmitError::Present(PresentError::WrongPhase(
            destination_state.phase,
        )));
    }
    source
        .prepare_renderer_acquire()
        .map_err(CopiedRenderSubmitError::RendererAcquire)?;
    destination_state.transition_to_recording();
    record(source)
        .map(|_| ())
        .map_err(CopiedRenderSubmitError::Present)?;
    source
        .export_render_completion()
        .map_err(|error| CopiedRenderSubmitError::Present(PresentError::Vk(error)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copied_renderer_acquire_failure_is_fail_stop_before_submit() {
        let acquire = CopiedRenderSubmitError::RendererAcquire(io::Error::other(
            "retained B-to-A completion import failed",
        ));
        assert!(acquire.requires_fail_stop());
        assert!(matches!(acquire.into_present(), PresentError::Io(_)));

        let ordinary = CopiedRenderSubmitError::Present(PresentError::Vk(
            vk::Result::ERROR_OUT_OF_DEVICE_MEMORY,
        ));
        assert!(!ordinary.requires_fail_stop());
    }
}
