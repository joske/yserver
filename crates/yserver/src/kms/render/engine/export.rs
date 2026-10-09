use super::*;

impl RenderEngine {
    /// GLX-TFP (Task 1.2): permanently migrate a server-owned pixmap
    /// onto dma-buf-exportable storage (glamor's model). Idempotent —
    /// returns early if the drawable is already exportable (DRI3 import
    /// or a prior promotion).
    ///
    /// Steps: (a) allocate an exportable image; (b)+(c) copy the old
    /// content into it and block until the copy completes; (d) build
    /// fresh sample + attachment views over the new image; (e) swap the
    /// `Storage` handles; (f) invalidate the drawable's cached views
    /// (the cache keys on `DrawableId` and never re-checks the
    /// `VkImage`, so a swap without this keeps sampling the old image);
    /// (g) retire the old handles once their guarding fence signals.
    ///
    /// # Errors
    ///
    /// `NoVk` (stub engine), `UnknownDrawable`, or any propagated
    /// `vk::Result` from allocation / view creation / the blocking copy.
    pub(crate) fn promote_drawable_exportable(
        &mut self,
        platform: &mut PlatformBackend,
        store: &mut DrawableStore,
        id: DrawableId,
    ) -> Result<(), RenderError> {
        if self.inner.is_none() {
            return Err(RenderError::NoVk);
        }
        // Idempotency check first — avoid the flush cost if already done.
        {
            let d = store.get(id).ok_or(RenderError::UnknownDrawable(id))?;
            if d.storage.is_exportable() {
                return Ok(());
            }
        }

        // Submit-boundary (codex review): any open frame / parked submit
        // group may hold CBs that captured cached views of the OLD image
        // but have NOT been submitted to the queue yet. The copy below
        // relies on same-queue submission ordering to read finalized
        // old-image content, and `retire_image_after` later destroys the
        // old handles + invalidated views once `last_render_ticket`
        // signals. Both are only sound if every prior user of the old
        // image is already submitted. Close the open frame and flush the
        // submit group here to establish that boundary (mirrors
        // `get_image`'s SyncWait close before its readback). After this,
        // `last_render_ticket` names the newest in-flight ticket that
        // touched the drawable.
        self.close_open_frame(
            store,
            platform,
            crate::kms::render::frame_builder::CloseReason::SyncWait,
        )?;
        self.flush_submit_group(
            store,
            platform,
            crate::kms::render::submit_group::FlushReason::SyncBoundary,
        )?;

        // Metadata read (post-close: current_layout reflects any
        // transition the frame close recorded).
        let (extent, format, depth, old_layout, old_image) = {
            let d = store.get(id).ok_or(RenderError::UnknownDrawable(id))?;
            let s = &d.storage;
            (s.extent, s.format, s.depth, s.current_layout, s.image)
        };

        let vk = platform.vk().ok_or(RenderError::NoVk)?.clone();

        // (a) allocate exportable target.
        let exp =
            crate::kms::vk::target::allocate_exportable(&vk, extent.width, extent.height, format)?;

        // (b)+(c) copy old content → new image, block until complete.
        self.copy_image_blocking(platform, old_image, old_layout, exp.image, extent)?;
        let new_layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;

        // (d) build new views (same depth-aware swizzle the store uses).
        let sample_view = PlatformBackend::build_sample_view(&vk, exp.image, format, depth)?;
        let image_view = match PlatformBackend::build_attachment_view(&vk, exp.image, format) {
            Ok(v) => v,
            Err(e) => {
                unsafe { vk.device.destroy_image_view(sample_view, None) };
                return Err(e.into());
            }
        };

        // (e) swap storage, taking ownership of the exportable image's
        //     raw handles. The drawable existence check happens BEFORE
        //     `into_raw_parts` so an absent drawable can't leak the four
        //     raw handles (`into_raw_parts` disarms `ExportableImage`'s
        //     Drop, so a bail-out after it would orphan image/memory and
        //     the two views). Unreachable in the single-threaded engine,
        //     but cheap to make leak-proof.
        let retired = {
            let d = store.get_mut(id).ok_or(RenderError::UnknownDrawable(id))?;
            let (exp_image, exp_memory, exp_stride, exp_size, exp_modifier) = exp.into_raw_parts();
            // A promoted redirect backing stays distinguishable from other exports.
            crate::kms::vk::mem_accounting::recategorise(
                exp_memory,
                crate::kms::vk::mem_accounting::export_category_for(
                    crate::kms::vk::mem_accounting::category_of(d.storage.memory),
                ),
            );
            d.storage.adopt_exportable(
                exp_image,
                exp_memory,
                sample_view,
                image_view,
                new_layout,
                exp_stride,
                exp_size,
                exp_modifier,
            )
        };

        // (f) invalidate the view cache for this DrawableId.
        self.invalidate_drawable_views(id);

        // (g) retire old handles once the old image's last render fence
        //     signals (clone the ticket; None → retire eagerly).
        let guard = store.get(id).and_then(|d| d.last_render_ticket.clone());
        self.retire_image_after(retired, guard);
        Ok(())
    }

    /// GLX-TFP (Task 1.2): blocking old→new image copy used by the
    /// promotion path. Records, on a dedicated one-shot CB + fence
    /// (`run_one_shot_op` waits for the fence before returning):
    ///   - `src`: `src_layout` → `TRANSFER_SRC_OPTIMAL`
    ///   - `dst`: `UNDEFINED` → `TRANSFER_DST_OPTIMAL`
    ///   - `vkCmdCopyImage` (full `extent`, COLOR, 1 mip / 1 layer)
    ///   - `dst`: `TRANSFER_DST_OPTIMAL` → `SHADER_READ_ONLY_OPTIMAL`
    ///
    /// Promotion is rare (once per pixmap, on first GLX bind), so a
    /// dedicated fence wait is acceptable.
    ///
    /// # Errors
    ///
    /// `NoVk`, or any propagated `vk::Result` from CB recording / submit
    /// / fence wait.
    fn copy_image_blocking(
        &mut self,
        platform: &PlatformBackend,
        src: vk::Image,
        src_layout: vk::ImageLayout,
        dst: vk::Image,
        extent: vk::Extent2D,
    ) -> Result<(), RenderError> {
        let inner = self.inner.as_ref().ok_or(RenderError::NoVk)?;
        let vk = Arc::clone(&inner.vk);
        let pool = platform
            .ops_command_pool_handle()
            .ok_or(RenderError::NoVk)?;

        crate::kms::vk::ops::run_one_shot_op(&vk, pool, |vk, cb| {
            let full_range = vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .level_count(1)
                .layer_count(1);

            let pre = [
                // src → TRANSFER_SRC_OPTIMAL
                vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                    .src_access_mask(vk::AccessFlags2::MEMORY_WRITE)
                    .dst_stage_mask(vk::PipelineStageFlags2::COPY)
                    .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
                    .old_layout(src_layout)
                    .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                    .image(src)
                    .subresource_range(full_range),
                // dst (UNDEFINED) → TRANSFER_DST_OPTIMAL
                vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                    .src_access_mask(vk::AccessFlags2::empty())
                    .dst_stage_mask(vk::PipelineStageFlags2::COPY)
                    .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                    .old_layout(vk::ImageLayout::UNDEFINED)
                    .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                    .image(dst)
                    .subresource_range(full_range),
            ];
            let dep = vk::DependencyInfo::default().image_memory_barriers(&pre);
            unsafe { vk.device.cmd_pipeline_barrier2(cb, &dep) };

            let layers = vk::ImageSubresourceLayers::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .layer_count(1);
            let region = [vk::ImageCopy::default()
                .src_subresource(layers)
                .dst_subresource(layers)
                .extent(vk::Extent3D {
                    width: extent.width,
                    height: extent.height,
                    depth: 1,
                })];
            unsafe {
                vk.device.cmd_copy_image(
                    cb,
                    src,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    dst,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &region,
                );
            }

            let post = [vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::COPY)
                .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
                .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .image(dst)
                .subresource_range(full_range)];
            let dep = vk::DependencyInfo::default().image_memory_barriers(&post);
            unsafe { vk.device.cmd_pipeline_barrier2(cb, &dep) };
            Ok(())
        })?;
        Ok(())
    }
}
