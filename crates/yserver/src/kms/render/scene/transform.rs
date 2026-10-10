use super::*;

impl SceneCompositor {
    /// The intermediate of transformed output `output_idx`, once a compose has
    /// filled it: `(image, footprint extent)`, in `GENERAL`.
    pub(crate) fn transform_intermediate(
        &self,
        output_idx: usize,
    ) -> Option<(vk::Image, vk::Extent2D)> {
        let im = self
            .inner
            .as_ref()?
            .outputs
            .get(output_idx)?
            .intermediate
            .as_ref()?;
        im.has_content.then_some((im.image, im.extent))
    }
}

pub(super) fn ensure_intermediates(
    inner: &mut SceneCompositorInner,
    platform: &PlatformBackend,
) -> Result<(), SceneError> {
    for (i, o) in inner.outputs.iter_mut().enumerate() {
        if platform.output_transform(i).is_none() {
            continue;
        }
        if o.intermediate.as_ref().map(|im| im.extent) == Some(o.output_extent) {
            continue;
        }
        release_intermediate(o, &inner.vk);
        if inner.scale_pipeline.is_none() {
            inner.scale_pipeline = Some(
                ScalePassPipeline::new(Arc::clone(&inner.vk), vk::Format::B8G8R8A8_UNORM)
                    .map_err(SceneError::PipelineInit)?,
            );
        }
        let pipeline = inner.scale_pipeline.as_ref().expect("built above");
        o.intermediate = Some(
            TransformIntermediate::new(Arc::clone(&inner.vk), o.output_extent, pipeline)
                .map_err(SceneError::Vk)?,
        );
        log::info!(
            "render scene: output {i} transform intermediate {}x{}",
            o.output_extent.width,
            o.output_extent.height
        );
    }
    Ok(())
}

/// Drop `o`'s intermediate once every compose that may use it has finished.
pub(super) fn release_intermediate(
    o: &mut OutputSceneState,
    vk: &crate::kms::vk::device::VkContext,
) {
    let Some(intermediate) = o.intermediate.take() else {
        return;
    };
    let tickets = o
        .pending_acks
        .iter()
        .filter_map(|ack| ack.ticket.as_ref())
        .chain(o.failed_submit_bos.iter().map(|f| &f.ticket))
        .chain(o.pending_pool_releases.iter().map(|(_, t)| t));
    for ticket in tickets {
        if let Err(error) = ticket.wait(vk) {
            log::error!(
                "render scene: output {} transform intermediate fence wait failed: {error:?}; \
                 leaking it",
                o.output_idx
            );
            std::mem::forget(intermediate);
            return;
        }
    }
    drop(intermediate);
}

impl IntermediatePrimeTarget {
    fn new(
        vk: Arc<crate::kms::vk::device::VkContext>,
        intermediate: &TransformIntermediate,
    ) -> Result<Self, vk::Result> {
        let pool_info = vk::CommandPoolCreateInfo::default()
            .queue_family_index(vk.graphics_queue_family)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        let command_pool = unsafe { vk.device.create_command_pool(&pool_info, None)? };
        let cb_info = vk::CommandBufferAllocateInfo::default()
            .command_pool(command_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        let command_buffer = match unsafe { vk.device.allocate_command_buffers(&cb_info) } {
            Ok(buffers) => buffers[0],
            Err(error) => {
                unsafe { vk.device.destroy_command_pool(command_pool, None) };
                return Err(error);
            }
        };
        Ok(Self {
            image: intermediate.image,
            view: intermediate.view,
            extent: intermediate.extent,
            vk,
            command_pool,
            command_buffer,
        })
    }
}

impl Drop for IntermediatePrimeTarget {
    /// The owner waits for the compose fence first.
    fn drop(&mut self) {
        unsafe { self.vk.device.destroy_command_pool(self.command_pool, None) };
    }
}

impl ComposeRenderTarget for IntermediatePrimeTarget {
    fn image(&self) -> vk::Image {
        self.image
    }

    fn image_view(&self) -> vk::ImageView {
        self.view
    }

    fn command_buffer(&self) -> vk::CommandBuffer {
        self.command_buffer
    }

    fn completion_semaphore(&self) -> vk::Semaphore {
        vk::Semaphore::null()
    }

    fn width(&self) -> u32 {
        self.extent.width
    }

    fn height(&self) -> u32 {
        self.extent.height
    }

    fn timestamp_pool(&self) -> vk::QueryPool {
        vk::QueryPool::null()
    }

    fn timestamps_written(&self) -> bool {
        false
    }

    fn mark_timestamps_written(&mut self) {}

    fn set_last_gpu_render_ns(&mut self, _value: Option<u64>) {}

    fn post_compose_preparation(&self) -> Result<PostComposePreparation, PresentError> {
        Ok(PostComposePreparation::Shared)
    }

    /// The scale pass step already left the intermediate in `GENERAL`.
    fn record_post_compose(
        &self,
        _vk: &crate::kms::vk::device::VkContext,
        _command_buffer: vk::CommandBuffer,
        _preparation: PostComposePreparation,
    ) {
    }
}

impl SceneCompositor {
    /// Whether a transformed output's intermediate is missing or uncomposed.
    pub(crate) fn has_unprimed_transform_intermediate(&self, platform: &PlatformBackend) -> bool {
        let Some(inner) = self.inner.as_ref() else {
            return false;
        };
        inner.outputs.iter().enumerate().any(|(i, o)| {
            platform.output_transform(i).is_some()
                && o.intermediate.as_ref().is_none_or(|im| !im.has_content)
        })
    }

    /// Compose every transformed output whose intermediate has never been
    /// composed, and wait, so a root read never sees an undefined one (D6).
    /// The caller has flushed pending paint, as before a scene tick.
    pub(crate) fn prime_transform_intermediates(
        &mut self,
        core: &KmsCore,
        store: &mut DrawableStore,
        windows: &crate::kms::render::backend::WindowsMap,
        platform: &PlatformBackend,
        cow_host_xid: Option<u32>,
    ) -> Result<(), SceneError> {
        #[cfg(test)]
        let descriptor_limit = self.test_prime_descriptor_sets;
        #[cfg(not(test))]
        let descriptor_limit: Option<usize> = None;
        let Some(inner) = self.inner.as_mut() else {
            return Ok(());
        };
        ensure_intermediates(inner, platform)?;
        for output_idx in 0..inner.outputs.len() {
            let Some(transform) = platform.output_transform(output_idx) else {
                continue;
            };
            let state = &inner.outputs[output_idx];
            let Some(intermediate) = state.intermediate.as_ref() else {
                continue;
            };
            if intermediate.has_content {
                continue;
            }
            let built = build_scene(
                core,
                store,
                windows,
                output_idx,
                platform,
                inner.cursor.clone(),
                None,
                cow_host_xid,
                false,
                Visibility::On,
            );
            let mut scale = ScalePass::new(
                intermediate,
                inner
                    .scale_pipeline
                    .as_ref()
                    .expect("an intermediate implies the pipeline"),
                transform,
                platform.output_root_rect(output_idx),
                (u32::from(platform.fb_w), u32::from(platform.fb_h)),
            );
            scale.into_target = false;
            let extent = intermediate.extent;
            let mut target = IntermediatePrimeTarget::new(Arc::clone(&inner.vk), intermediate)
                .map_err(SceneError::Vk)?;
            let cursor_rect = built
                .software_cursor_tail
                .and(built.scene.draws.last())
                .and_then(draw_dst_rect_inward)
                .and_then(|rect| clip_rect_to_output_extent(rect, extent));
            let vk = Arc::clone(&inner.vk);
            let state = &mut inner.outputs[output_idx];
            let cursor_save = state.cursor_saves.prepare(&vk, scale.image, cursor_rect);
            let draws = built.scene.draws.len();
            let pool = create_audit_descriptor_pool(&vk, draws)?;
            let ticket = platform.acquire_fence_ticket().map_err(SceneError::Vk)?;
            let mut submitted = false;
            let result = record_and_submit_render(
                &vk,
                &mut target,
                &inner.pipeline,
                pool,
                &built.scene,
                Repaint::Full(extent),
                &[],
                ticket.fence(),
                &mut submitted,
                &[],
                vk::Pipeline::null(),
                vk::PipelineLayout::null(),
                Some(&scale),
                cursor_save,
            );
            let waited = if submitted {
                ticket.wait(&vk).map_err(SceneError::Vk)
            } else {
                Ok(())
            };
            state
                .cursor_saves
                .finish(scale.image, cursor_save, submitted.then_some(&ticket));
            if waited.is_err() {
                // The GPU may still use the pool and command buffer.
                std::mem::forget(target);
                return waited;
            }
            unsafe { vk.device.destroy_descriptor_pool(pool, None) };
            let recorded = result?.descriptor_count;
            // Drivers may over-allocate a pool, so the test caps the count.
            let recorded = descriptor_limit.map_or(recorded, |n| recorded.min(n));
            for id in &built.sampled_ids {
                store.touch_render_fence(*id, ticket.clone());
            }
            // A truncated compose painted less than the scene, as the tick's
            // own check says: the read then zero-fills that output as for any
            // unreadable piece, and the next tick repaints it in full.
            if recorded == draws {
                if let Some(intermediate) = state.intermediate.as_mut() {
                    intermediate.has_content = true;
                }
            } else {
                log::warn!(
                    "render root read: output {output_idx} priming composed {recorded} of \
                     {draws} draws (descriptor pool exhausted); not read"
                );
            }
        }
        Ok(())
    }
}

impl ScalePass {
    pub(super) fn new(
        intermediate: &TransformIntermediate,
        pipeline: &ScalePassPipeline,
        transform: &yserver_core::randr::CrtcTransform,
        footprint: (i32, i32, u32, u32),
        root: (u32, u32),
    ) -> Self {
        Self {
            image: intermediate.image,
            view: intermediate.view,
            extent: intermediate.extent,
            descriptor_set: intermediate.descriptor_set,
            pipeline: pipeline.pipeline,
            layout: pipeline.pipeline_layout,
            push: crate::kms::render::transform_intermediate::scale_push(
                transform,
                intermediate.extent,
            ),
            composite_rect: crate::kms::render::transform_intermediate::composite_rect(
                footprint, root,
            ),
            into_target: true,
        }
    }
}

/// Scale the just-composited intermediate into the BO (spec D4): leaves the
/// intermediate in `GENERAL` for the next frame and root readback, and the BO
/// in `COLOR_ATTACHMENT_OPTIMAL` for `record_post_compose`.
///
/// # Safety
///
/// `cb` is recording, outside a rendering scope, after the compose into
/// `sp.image`.
#[allow(clippy::too_many_arguments)]
pub(super) unsafe fn record_scale_pass(
    vk: &crate::kms::vk::device::VkContext,
    cb: vk::CommandBuffer,
    target: vk::Image,
    target_view: vk::ImageView,
    width: u32,
    height: u32,
    sp: &ScalePass,
) {
    let device = &vk.device;
    let color = vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1);
    let intermediate_done = vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
        .src_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
        .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
        .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
        .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .new_layout(vk::ImageLayout::GENERAL)
        .image(sp.image)
        .subresource_range(color);
    if !sp.into_target {
        crate::vk_count!(cmd_pipeline_barrier2);
        unsafe {
            device.cmd_pipeline_barrier2(
                cb,
                &vk::DependencyInfo::default().image_memory_barriers(&[intermediate_done]),
            );
        }
        return;
    }
    let barriers = [
        intermediate_done,
        vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::TOP_OF_PIPE)
            .src_access_mask(vk::AccessFlags2::empty())
            .dst_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
            .dst_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .image(target)
            .subresource_range(color),
    ];
    crate::vk_count!(cmd_pipeline_barrier2);
    unsafe {
        device.cmd_pipeline_barrier2(
            cb,
            &vk::DependencyInfo::default().image_memory_barriers(&barriers),
        );
    }
    let whole = vk::Rect2D {
        offset: vk::Offset2D::default(),
        extent: vk::Extent2D { width, height },
    };
    // Every pixel is written, so nothing needs loading.
    let attachment = [vk::RenderingAttachmentInfo::default()
        .image_view(target_view)
        .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .load_op(vk::AttachmentLoadOp::DONT_CARE)
        .store_op(vk::AttachmentStoreOp::STORE)];
    let rendering = vk::RenderingInfo::default()
        .render_area(whole)
        .layer_count(1)
        .color_attachments(&attachment);
    #[allow(clippy::cast_precision_loss)]
    let viewport = [vk::Viewport {
        x: 0.0,
        y: 0.0,
        width: width as f32,
        height: height as f32,
        min_depth: 0.0,
        max_depth: 1.0,
    }];
    unsafe {
        crate::vk_count!(cmd_begin_rendering);
        device.cmd_begin_rendering(cb, &rendering);
        device.cmd_set_viewport(cb, 0, &viewport);
        device.cmd_set_scissor(cb, 0, &[whole]);
        device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, sp.pipeline);
        device.cmd_bind_descriptor_sets(
            cb,
            vk::PipelineBindPoint::GRAPHICS,
            sp.layout,
            0,
            &[sp.descriptor_set],
            &[],
        );
        device.cmd_push_constants(
            cb,
            sp.layout,
            vk::ShaderStageFlags::FRAGMENT,
            0,
            sp.push.as_bytes(),
        );
        crate::vk_count!(cmd_draw);
        device.cmd_draw(cb, 3, 1, 0, 0);
        crate::vk_count!(cmd_end_rendering);
        device.cmd_end_rendering(cb);
    }
}
