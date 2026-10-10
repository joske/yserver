use super::*;

pub(super) fn present_error_is_device_lost(error: &PresentError) -> bool {
    matches!(error, PresentError::Vk(vk::Result::ERROR_DEVICE_LOST))
}

impl CopiedRenderSubmitError {
    pub(super) fn requires_fail_stop(&self) -> bool {
        matches!(self, Self::RendererAcquire(_))
    }

    pub(super) fn into_present(self) -> PresentError {
        match self {
            Self::RendererAcquire(error) => PresentError::Io(error),
            Self::Present(error) => error,
        }
    }
}

pub(super) fn vk_result_is_device_lost(result: vk::Result) -> bool {
    result == vk::Result::ERROR_DEVICE_LOST
}

pub(super) fn compose_submit_was_complete(submitted: ComposeSubmit, draw_count: usize) -> bool {
    submitted.descriptor_count == draw_count
}

/// Step 3's staging, gated on the submit having recorded every draw.
///
/// A truncated submit (descriptor pool exhausted, `record_command_buffer` drew
/// only the allocated prefix) painted less than `painted` claims. Recording it
/// as painted would clear `missing` for pixels never touched and bake the hole
/// into that BO permanently; `invalidate` costs one full repaint instead.
pub(super) fn stage_submitted_frame(
    damage: &mut ScanoutDamage,
    complete: bool,
    bo_idx: usize,
    repaint: &Region,
    painted: &Region,
) {
    if complete {
        damage.commit_submitted(bo_idx, repaint, painted);
    } else {
        damage.invalidate();
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

    fn post_compose_preparation(&self) -> Result<PostComposePreparation, PresentError> {
        Ok(PostComposePreparation::Shared)
    }

    fn record_post_compose(
        &self,
        vk: &crate::kms::vk::device::VkContext,
        command_buffer: vk::CommandBuffer,
        preparation: PostComposePreparation,
    ) {
        debug_assert!(matches!(preparation, PostComposePreparation::Shared));
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

    fn post_compose_preparation(&self) -> Result<PostComposePreparation, PresentError> {
        self.transport_preparation()
            .map(PostComposePreparation::Copied)
            .map_err(PresentError::Io)
    }

    fn renderer_wait_semaphore(&self) -> Option<vk::Semaphore> {
        self.renderer_wait_semaphore()
    }

    fn note_submit_succeeded(&mut self) {
        self.note_renderer_submit_succeeded();
    }

    fn record_post_compose(
        &self,
        _vk: &crate::kms::vk::device::VkContext,
        command_buffer: vk::CommandBuffer,
        preparation: PostComposePreparation,
    ) {
        let PostComposePreparation::Copied(preparation) = preparation else {
            unreachable!("copied target received shared post-compose preparation")
        };
        self.record_transport_copy(command_buffer, preparation);
    }
}

/// Render directly into the KMS framebuffer and immediately queue its flip.
#[allow(clippy::too_many_arguments)]
pub(super) fn submit_shared_scanout_frame(
    vk: &crate::kms::vk::device::VkContext,
    drm: &crate::drm::Device,
    output: &crate::platform::drm::Output,
    bo: &mut ScanoutBo,
    pipeline: &CompositorPipeline,
    descriptor_pool: vk::DescriptorPool,
    scene: &CompositeScene,
    repaint: Repaint,
    scissors: &[vk::Rect2D],
    signal_fence: vk::Fence,
    gpu_submitted: &mut bool,
    overlay_ops: &[(u32, vk::Rect2D)],
    xor_pipeline: vk::Pipeline,
    xor_layout: vk::PipelineLayout,
    scale: Option<&ScalePass>,
    cursor_save: Option<CursorSaveTarget>,
) -> Result<ComposeSubmit, PresentError> {
    use std::os::fd::{FromRawFd, IntoRawFd};

    if bo.state.phase != BoPhase::Free {
        return Err(PresentError::WrongPhase(bo.state.phase));
    }
    let fb_handle = bo.fb_handle.ok_or(PresentError::NoFb)?;
    bo.state.transition_to_recording();
    let submitted = record_and_submit_render(
        vk,
        bo,
        pipeline,
        descriptor_pool,
        scene,
        repaint,
        scissors,
        signal_fence,
        gpu_submitted,
        overlay_ops,
        xor_pipeline,
        xor_layout,
        scale,
        cursor_save,
    )?;

    let fd = bo
        .export_signaled_fd()
        .map_err(PresentError::Vk)?
        .map_or(-1, IntoRawFd::into_raw_fd);
    bo.state.transition_to_submitted(fd);

    let mut out_fence: i32 = -1;
    match crate::drm::page_flip::submit_flip_with_fences(drm, output, fb_handle, fd, &mut out_fence)
    {
        Ok(()) => {
            if let Some(reclaimed) = bo.state.transition_to_pending(out_fence) {
                // SAFETY: `reclaimed` was inserted by
                // `transition_to_submitted` above.
                drop(unsafe { std::os::fd::OwnedFd::from_raw_fd(reclaimed) });
            }
            Ok(submitted)
        }
        Err(error) => {
            if let Some(reclaimed) = bo.state.transition_to_recording_after_atomic_reject() {
                // SAFETY: same fd we just inserted.
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
#[allow(clippy::too_many_arguments)]
pub(super) fn submit_copied_scanout_render(
    vk: &crate::kms::vk::device::VkContext,
    source: &mut CopiedRenderSource,
    destination_state: &mut BoState,
    pipeline: &CompositorPipeline,
    descriptor_pool: vk::DescriptorPool,
    scene: &CompositeScene,
    repaint: Repaint,
    scissors: &[vk::Rect2D],
    signal_fence: vk::Fence,
    gpu_submitted: &mut bool,
    overlay_ops: &[(u32, vk::Rect2D)],
    xor_pipeline: vk::Pipeline,
    xor_layout: vk::PipelineLayout,
    scale: Option<&ScalePass>,
    cursor_save: Option<CursorSaveTarget>,
) -> Result<Option<std::os::fd::OwnedFd>, CopiedRenderSubmitError> {
    if destination_state.phase != BoPhase::Free {
        return Err(CopiedRenderSubmitError::Present(PresentError::WrongPhase(
            destination_state.phase,
        )));
    }
    source
        .prepare_renderer_acquire()
        .map_err(CopiedRenderSubmitError::RendererAcquire)?;
    destination_state.transition_to_recording();
    record_and_submit_render(
        vk,
        source,
        pipeline,
        descriptor_pool,
        scene,
        repaint,
        scissors,
        signal_fence,
        gpu_submitted,
        overlay_ops,
        xor_pipeline,
        xor_layout,
        scale,
        cursor_save,
    )
    .map(|_| ())
    .map_err(CopiedRenderSubmitError::Present)?;
    source
        .export_render_completion()
        .map_err(|error| CopiedRenderSubmitError::Present(PresentError::Vk(error)))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn record_and_submit_render(
    vk: &crate::kms::vk::device::VkContext,
    target: &mut impl ComposeRenderTarget,
    pipeline: &CompositorPipeline,
    descriptor_pool: vk::DescriptorPool,
    scene: &CompositeScene,
    repaint: Repaint,
    scissors: &[vk::Rect2D],
    signal_fence: vk::Fence,
    gpu_submitted: &mut bool,
    overlay_ops: &[(u32, vk::Rect2D)],
    xor_pipeline: vk::Pipeline,
    xor_layout: vk::PipelineLayout,
    scale: Option<&ScalePass>,
    cursor_save: Option<CursorSaveTarget>,
) -> Result<ComposeSubmit, PresentError> {
    // Compose GPU-render telemetry. Read the PREVIOUS compose's
    // timestamps BEFORE the CB overwrites them; the read is
    // synchronous (no WAIT flag), and the bo is being re-acquired so
    // its prior compose fence has already signalled → results are
    // available. The first compose of a new pool must NOT read: its
    // queries have never been reset, and reading them is invalid
    // (validation: "query not reset") even though drivers tend to
    // answer `NOT_READY`. `tick_one_output` takes and forwards this to
    // `telemetry.record_gpu_render_ns` after submission returns.
    let ts_pool = target.timestamp_pool();
    let ts_enabled = vk.timestamp_period > 0.0 && ts_pool != vk::QueryPool::null();
    let last_gpu_render_ns = if ts_enabled && target.timestamps_written() {
        let mut ts = [0u64; 2];
        match unsafe {
            vk.device
                .get_query_pool_results(ts_pool, 0, &mut ts, vk::QueryResultFlags::TYPE_64)
        } {
            Ok(()) => {
                let ticks = ts[1].saturating_sub(ts[0]);
                #[allow(
                    clippy::cast_precision_loss,
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss
                )]
                let ns = (ticks as f64 * f64::from(vk.timestamp_period)) as u64;
                Some(ns)
            }
            Err(_) => None,
        }
    } else {
        None
    };
    target.set_last_gpu_render_ns(last_gpu_render_ns);

    // Allocate descriptor sets — same shape as v1.
    let mut descriptors: Vec<vk::DescriptorSet> = Vec::with_capacity(scene.draws.len());
    for draw in &scene.draws {
        let layouts = [pipeline.descriptor_set_layout];
        let alloc_info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(descriptor_pool)
            .set_layouts(&layouts);
        let set = match unsafe { vk.device.allocate_descriptor_sets(&alloc_info) } {
            Ok(sets) => sets[0],
            Err(e) => {
                log::warn!(
                    "render compose: descriptor allocation failed ({e:?}) at draw {} of {}",
                    descriptors.len(),
                    scene.draws.len(),
                );
                break;
            }
        };
        let image_info = [vk::DescriptorImageInfo::default()
            .image_view(draw.image_view)
            .sampler(pipeline.sampler)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
        let writes = [vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .image_info(&image_info)];
        unsafe { vk.device.update_descriptor_sets(&writes, &[]) };
        descriptors.push(set);
    }

    // Record.
    record_command_buffer(
        vk,
        target,
        pipeline,
        scene,
        &descriptors,
        repaint,
        scissors,
        overlay_ops,
        xor_pipeline,
        xor_layout,
        scale,
        cursor_save,
    )?;

    let cb = target.command_buffer();
    let cb_info = [vk::CommandBufferSubmitInfo::default().command_buffer(cb)];
    let signal_semaphore = target.completion_semaphore();
    let sig_info = [vk::SemaphoreSubmitInfo::default()
        .semaphore(signal_semaphore)
        .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)];
    let waits = target.renderer_wait_semaphore().map(|semaphore| {
        [vk::SemaphoreSubmitInfo::default()
            .semaphore(semaphore)
            .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)]
    });
    let mut submit = vk::SubmitInfo2::default().command_buffer_infos(&cb_info);
    if signal_semaphore != vk::Semaphore::null() {
        submit = submit.signal_semaphore_infos(&sig_info);
    }
    if let Some(waits) = waits.as_ref() {
        submit = submit.wait_semaphore_infos(waits);
    }
    let submit = [submit];
    unsafe {
        crate::vk_count!(queue_submit2);
        crate::vk_count!(submit_compositor);
        crate::kms::vk::submit_stats::timed(
            crate::kms::vk::submit_stats::SubmitCause::Compose,
            cb_info.len(),
            false,
            || {
                vk.device
                    .queue_submit2(vk.graphics_queue, &submit, signal_fence)
            },
        )?;
    }
    target.note_submit_succeeded();
    *gpu_submitted = true;
    // The CB just submitted reset and wrote both queries
    // (`record_command_buffer`), so the next compose may read them.
    if ts_enabled {
        target.mark_timestamps_written();
    }
    Ok(ComposeSubmit {
        descriptor_count: descriptors.len(),
    })
}

#[allow(clippy::too_many_arguments)]
fn record_command_buffer<T: ComposeRenderTarget + ?Sized>(
    vk: &crate::kms::vk::device::VkContext,
    bo: &T,
    pipeline: &CompositorPipeline,
    scene: &CompositeScene,
    descriptors: &[vk::DescriptorSet],
    repaint: Repaint,
    scissors: &[vk::Rect2D],
    overlay_ops: &[(u32, vk::Rect2D)],
    xor_pipeline: vk::Pipeline,
    xor_layout: vk::PipelineLayout,
    scale: Option<&ScalePass>,
    cursor_save: Option<CursorSaveTarget>,
) -> Result<(), PresentError> {
    let device = &vk.device;
    let cb = bo.command_buffer();
    // A transformed output composites into its intermediate; the scale pass
    // then writes the whole BO.
    let (compose_image, compose_view, compose_w, compose_h) = scale.map_or(
        (bo.image(), bo.image_view(), bo.width(), bo.height()),
        |sp| (sp.image, sp.view, sp.extent.width, sp.extent.height),
    );
    // Mirror the timestamp gate `record_and_submit_render` uses so we can bracket the
    // CB with TOP/BOTTOM timestamp writes; caller already read the
    // previous pool contents before we reset the pool below.
    let ts_pool = bo.timestamp_pool();
    let ts_enabled = vk.timestamp_period > 0.0 && ts_pool != vk::QueryPool::null();
    // Validate all copied transport state before beginning the command buffer.
    // The post-compose recorder is then infallible and cannot strand a live CB
    // in recording state on an ownership-ledger error.
    let post_compose_preparation = bo.post_compose_preparation()?;
    let (load_op, render_area, old_layout) = match repaint {
        Repaint::Full(extent) => (
            vk::AttachmentLoadOp::CLEAR,
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent,
            },
            vk::ImageLayout::UNDEFINED,
        ),
        Repaint::Clipped(rect) => (
            vk::AttachmentLoadOp::LOAD,
            // Step 4: `render_area` is the clipped rect, not the whole
            // attachment. Asking the driver to LOAD an attachment we then refuse
            // to touch is pure cost. The layout barrier stays full-subresource,
            // which is consistent — dynamic rendering is free to use a smaller
            // render area than the image.
            rect,
            // LOAD requires the previous layout to be valid; the
            // BO has been through a prior present which left it
            // at GENERAL (KMS scanout layout). Transition from
            // GENERAL → COLOR_ATTACHMENT_OPTIMAL with a full
            // memory barrier so prior writes are visible.
            vk::ImageLayout::GENERAL,
        ),
        Repaint::AuditClearClipped(rect) => {
            (vk::AttachmentLoadOp::CLEAR, rect, vk::ImageLayout::GENERAL)
        }
    };
    // Scissors to render under. `plan_repaint` supplies the damage region's own
    // rects when the bounding box wastes enough to be worth the extra draw
    // calls; otherwise a single rect, which is the behaviour this had before.
    // They are disjoint (a canonical `Region`), so every fragment is written
    // exactly once — which is what keeps the non-idempotent overlay XOR correct.
    let default_scissor = match repaint {
        Repaint::Full(extent) => vk::Rect2D {
            offset: vk::Offset2D::default(),
            extent,
        },
        Repaint::Clipped(rect) | Repaint::AuditClearClipped(rect) => rect,
    };
    let scissors: &[vk::Rect2D] = if let Some(sp) = scale {
        // Only footprint ∩ root is composited (spec D3); none at all when
        // the root does not reach the CRTC.
        sp.composite_rect.as_slice()
    } else if scissors.is_empty() {
        std::slice::from_ref(&default_scissor)
    } else {
        scissors
    };

    unsafe {
        device.reset_command_buffer(cb, vk::CommandBufferResetFlags::empty())?;
        let begin = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        crate::vk_count!(begin_command_buffer);
        device.begin_command_buffer(cb, &begin)?;

        // GPU-render timer: reset the pool (GPU-ordered, after the CPU
        // read above) and stamp TOP-of-pipe before any compose work.
        // See the corresponding BOTTOM stamp before end_command_buffer.
        if ts_enabled {
            device.cmd_reset_query_pool(cb, ts_pool, 0, 2);
            device.cmd_write_timestamp(cb, vk::PipelineStageFlags::TOP_OF_PIPE, ts_pool, 0);
        }

        let to_color_src_access = if matches!(load_op, vk::AttachmentLoadOp::LOAD) {
            // LOAD: previous KMS scanout left the BO in GENERAL.
            // The kernel "consumed" the BO contents via the page
            // flip; we now need the GPU to read+write them. Pair
            // ALL_COMMANDS + empty source access (no prior GPU
            // work to drain — the scanout completes before the
            // pageflip event fires) with COLOR_ATTACHMENT_OUTPUT
            // + WRITE on the dst.
            vk::AccessFlags2::empty()
        } else {
            vk::AccessFlags2::empty()
        };
        // The intermediate was last sampled by the previous frame's scale
        // pass or copied by a root readback: wait for those reads.
        let to_color_src_stage = if scale.is_some() {
            vk::PipelineStageFlags2::ALL_COMMANDS
        } else {
            vk::PipelineStageFlags2::TOP_OF_PIPE
        };
        let to_color = vk::ImageMemoryBarrier2::default()
            .src_stage_mask(to_color_src_stage)
            .src_access_mask(to_color_src_access)
            .dst_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
            // B.2 fix (vkdebug READ_AFTER_WRITE at vkCmdBeginRendering):
            // include COLOR_ATTACHMENT_READ so the loadOp=LOAD that
            // begin_rendering performs is synchronized against the
            // layout-transition's write. Validation surfaces this
            // hazard with the message "must allow
            // COLOR_ATTACHMENT_READ accesses at COLOR_ATTACHMENT_OUTPUT".
            .dst_access_mask(
                vk::AccessFlags2::COLOR_ATTACHMENT_WRITE | vk::AccessFlags2::COLOR_ATTACHMENT_READ,
            )
            .old_layout(old_layout)
            .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .image(compose_image)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            );
        let to_color_arr = [to_color];
        let to_color_dep = vk::DependencyInfo::default().image_memory_barriers(&to_color_arr);
        crate::vk_count!(cmd_pipeline_barrier2);
        device.cmd_pipeline_barrier2(cb, &to_color_dep);

        // Outside the root the intermediate is transparent black, which the
        // scale pass's bilinear edge blends toward (spec D4).
        let clear_color = if scale.is_some() {
            [0.0; 4]
        } else {
            scene.bg_color
        };
        let color_attachment = [vk::RenderingAttachmentInfo::default()
            .image_view(compose_view)
            .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .load_op(load_op)
            .store_op(vk::AttachmentStoreOp::STORE)
            .clear_value(vk::ClearValue {
                color: vk::ClearColorValue {
                    float32: clear_color,
                },
            })];
        let rendering_info = vk::RenderingInfo::default()
            .render_area(render_area)
            .layer_count(1)
            .color_attachments(&color_attachment);
        crate::vk_count!(cmd_begin_rendering);
        device.cmd_begin_rendering(cb, &rendering_info);

        let viewport = [vk::Viewport {
            x: 0.0,
            y: 0.0,
            #[allow(clippy::cast_precision_loss)]
            width: compose_w as f32,
            #[allow(clippy::cast_precision_loss)]
            height: compose_h as f32,
            min_depth: 0.0,
            max_depth: 1.0,
        }];
        crate::vk_count!(cmd_set_viewport);
        device.cmd_set_viewport(cb, 0, &viewport);

        if let Some(rect) = scale.and_then(|sp| sp.composite_rect) {
            // The root's part gets the clear colour the untransformed path
            // clears the whole BO to.
            let clear = [vk::ClearAttachment::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .color_attachment(0)
                .clear_value(vk::ClearValue {
                    color: vk::ClearColorValue {
                        float32: scene.bg_color,
                    },
                })];
            let rects = [vk::ClearRect::default().rect(rect).layer_count(1)];
            device.cmd_clear_attachments(cb, &clear, &rects);
        }

        #[allow(clippy::cast_precision_loss)]
        let viewport_size = [compose_w as f32, compose_h as f32];
        // The software cursor, the last draw, waits until the pixels under it
        // are saved.
        let body = if cursor_save.is_some() {
            descriptors.len().min(scene.draws.len().saturating_sub(1))
        } else {
            descriptors.len()
        };
        // Scissor-major: for each rect, replay the draws that touch it. A draw
        // spanning two rects is issued twice, which is correct because the rects
        // are disjoint, and is why `MAX_SCISSOR_RECTS` bounds the list.
        let record_draws = |range: std::ops::Range<usize>| {
            let mut last_pipeline: Option<vk::Pipeline> = None;
            for scissor in scissors {
                crate::vk_count!(cmd_set_scissor);
                device.cmd_set_scissor(cb, 0, std::slice::from_ref(scissor));
                for (i, draw) in scene
                    .draws
                    .iter()
                    .enumerate()
                    .take(range.end)
                    .skip(range.start)
                {
                    if scissors.len() > 1
                        && draw_dst_rect_inward(draw)
                            .is_some_and(|dst| !rects_intersect(dst, *scissor))
                    {
                        continue;
                    }
                    let pl = pipeline.pipeline_for(draw.alpha_passthrough);
                    if last_pipeline != Some(pl) {
                        crate::vk_count!(cmd_bind_pipeline);
                        device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, pl);
                        last_pipeline = Some(pl);
                    }
                    let sets = [descriptors[i]];
                    crate::vk_count!(cmd_bind_descriptor_sets);
                    device.cmd_bind_descriptor_sets(
                        cb,
                        vk::PipelineBindPoint::GRAPHICS,
                        pipeline.pipeline_layout,
                        0,
                        &sets,
                        &[],
                    );
                    let push = CompositePushConsts {
                        dst_origin: draw.dst_origin,
                        dst_size: draw.dst_size,
                        viewport: viewport_size,
                        src_origin: draw.src_origin,
                        src_size: draw.src_size,
                    };
                    crate::vk_count!(cmd_push_constants);
                    device.cmd_push_constants(
                        cb,
                        pipeline.pipeline_layout,
                        vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                        0,
                        push.as_bytes(),
                    );
                    crate::vk_count!(cmd_draw);
                    device.cmd_draw(cb, 4, 1, 0, 0);
                }
            }
        };
        record_draws(0..body);

        // Retained root-`IncludeInferiors` overlay XOR pass — applied
        // into the freshly-composited scanout BO while it is still in
        // COLOR_ATTACHMENT_OPTIMAL with rendering active (no extra
        // barrier / begin_rendering needed). The recorder rebinds its
        // own pipeline + per-op scissor, so it is safe after the scene
        // draw loop above.
        //
        // NON-IDEMPOTENT: correct only under `Repaint::Full` (CLEAR + full
        // redraw → XORed once onto fresh pixels). If buffer-age
        // `Repaint::Clipped`+LOAD is re-enabled, the overlay rects MUST be
        // folded into the repaint region or the XOR double-applies on
        // uncovered pooled BOs. See `pick_repaint_region` doc.
        if !overlay_ops.is_empty() {
            crate::kms::vk::ops::scanout_logic_fill::record_scanout_logic_fill(
                vk,
                cb,
                xor_pipeline,
                xor_layout,
                viewport_size,
                overlay_ops,
            );
        }

        crate::vk_count!(cmd_end_rendering);
        device.cmd_end_rendering(cb);

        if let Some(save) = cursor_save {
            record_cursor_save(vk, cb, compose_image, save);
            let color_attachment = [color_attachment[0].load_op(vk::AttachmentLoadOp::LOAD)];
            let rendering_info = rendering_info.color_attachments(&color_attachment);
            crate::vk_count!(cmd_begin_rendering);
            device.cmd_begin_rendering(cb, &rendering_info);
            crate::vk_count!(cmd_set_viewport);
            device.cmd_set_viewport(cb, 0, &viewport);
            record_draws(body..descriptors.len());
            crate::vk_count!(cmd_end_rendering);
            device.cmd_end_rendering(cb);
        }

        if let Some(sp) = scale {
            record_scale_pass(
                vk,
                cb,
                bo.image(),
                bo.image_view(),
                bo.width(),
                bo.height(),
                sp,
            );
        }

        bo.record_post_compose(vk, cb, post_compose_preparation);

        // GPU-render timer: stamp BOTTOM-of-pipe after all compose work.
        if ts_enabled {
            device.cmd_write_timestamp(cb, vk::PipelineStageFlags::BOTTOM_OF_PIPE, ts_pool, 1);
        }

        crate::vk_count!(end_command_buffer);
        device.end_command_buffer(cb)?;
    }
    let _ = render_area;
    Ok(())
}
