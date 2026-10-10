use super::*;

/// Exact usage of GPU B's imported alias of A's DMA-BUF transport. Keep this
/// single value shared by the capability query and actual import.
pub(super) const COPIED_SINK_IMPORT_USAGE: vk::ImageUsageFlags = vk::ImageUsageFlags::TRANSFER_SRC;

impl CopiedScanoutPlan {
    pub(crate) fn describe(self) -> String {
        format!(
            "source-{} -> destination-{}",
            self.source.describe(),
            self.destination.describe(),
        )
    }
}

impl CopiedRenderSource {
    fn allocate_exact(
        render_vk: Arc<VkContext>,
        sink_vk: Arc<VkContext>,
        width: u32,
        height: u32,
        plan: CopiedSourcePlan,
    ) -> io::Result<Self> {
        let transport_on_renderer = allocate_copied_source_exact(
            &render_vk,
            width,
            height,
            vk::Format::B8G8R8A8_UNORM,
            plan,
        )
        .map_err(|result| scanout_vk_error("allocate exact copied source", result))?;
        let exported = crate::kms::vk::dri3::export_backing(&render_vk, &transport_on_renderer)
            .map_err(|result| scanout_vk_error("export copied transport DMA-BUF", result))?;
        let imported_on_sink = DrawableImage::from_dmabuf_with_usage(
            Arc::clone(&sink_vk),
            exported.fd,
            width,
            height,
            vk::Format::B8G8R8A8_UNORM,
            exported.modifier,
            &[transport_on_renderer.offset],
            &[transport_on_renderer.stride],
            COPIED_SINK_IMPORT_USAGE,
            NonExplicitLinearLayoutPolicy::RequireExact,
        )
        .map_err(|error| copied_drawable_error("import copied transport on sink", error))?;
        let render_target =
            DrawableImage::new_server_owned_window(Arc::clone(&render_vk), width, height).map_err(
                |error| copied_drawable_error("allocate copied optimal render target", error),
            )?;
        for memory in [
            transport_on_renderer.memory,
            imported_on_sink.backing_memory(),
            render_target.backing_memory(),
        ] {
            crate::kms::vk::mem_accounting::recategorise(
                memory,
                crate::kms::vk::mem_accounting::MemCategory::Scanout,
            );
        }

        let completion_semaphore = create_export_semaphore(&render_vk).map_err(|result| {
            scanout_vk_error("create copied source completion semaphore", result)
        })?;
        let transfer = match allocate_transfer_resources(&render_vk, width, height) {
            Ok(transfer) => transfer,
            Err(result) => {
                unsafe {
                    render_vk
                        .device
                        .destroy_semaphore(completion_semaphore, None);
                }
                return Err(scanout_vk_error(
                    "allocate copied source command resources",
                    result,
                ));
            }
        };

        Ok(Self {
            imported_on_sink: Some(imported_on_sink),
            transport_on_renderer: Some(transport_on_renderer),
            render_target: Some(render_target),
            completion_semaphore,
            completion_semaphore_reuse: ExportSemaphoreReuseState::Reusable,
            transfer,
            last_gpu_render_ns: None,
            render_vk,
            sink_vk,
            sink_wait_semaphore: None,
            renderer_wait_semaphore: None,
            renderer_return_completion: None,
            ownership: CopiedSourceOwnership::RendererFirstUse,
            render_target_contents: CopiedRenderTargetContents::Uninitialized,
            disarmed: false,
        })
    }

    #[must_use]
    pub(crate) fn image(&self) -> vk::Image {
        self.render_target
            .as_ref()
            .expect("live copied source has optimal render target")
            .vk_image
    }

    #[must_use]
    pub(crate) fn image_view(&self) -> vk::ImageView {
        self.render_target
            .as_ref()
            .expect("live copied source has optimal render target")
            .vk_image_view
    }

    #[must_use]
    fn transport_image(&self) -> vk::Image {
        self.transport_on_renderer
            .as_ref()
            .expect("live copied source has DMA-BUF transport")
            .image
    }

    #[must_use]
    pub(crate) fn width(&self) -> u32 {
        self.render_target
            .as_ref()
            .expect("live copied source has optimal render target")
            .extent
            .width
    }

    #[must_use]
    pub(crate) fn height(&self) -> u32 {
        self.render_target
            .as_ref()
            .expect("live copied source has optimal render target")
            .extent
            .height
    }

    pub(crate) fn export_render_completion(&mut self) -> Result<Option<OwnedFd>, vk::Result> {
        self.completion_semaphore_reuse.begin_post_submit_export();
        let info = vk::SemaphoreGetFdInfoKHR::default()
            .semaphore(self.completion_semaphore)
            .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
        let raw = unsafe { self.render_vk.semaphore_fd_ext()?.get_semaphore_fd(&info)? };
        let completion = crate::kms::vk::optional_sync_fd_from_vk(
            raw,
            "vkGetSemaphoreFdKHR(copied render SYNC_FD)",
        )?;
        self.completion_semaphore_reuse.finish_successful_export();
        Ok(completion)
    }

    /// Replace a completion semaphore whose post-submit export failed.
    ///
    /// The caller must first prove the renderer submission completed. A fresh
    /// semaphore is created before destroying the dirty one so allocation
    /// failure leaves the source quarantinable rather than half-initialized.
    pub(crate) fn rearm_completion_semaphore_after_quiescence(&mut self) -> Result<(), vk::Result> {
        if !self.completion_semaphore_reuse.needs_rearm() {
            return Ok(());
        }
        let replacement = create_export_semaphore(&self.render_vk)?;
        unsafe {
            self.render_vk
                .device
                .destroy_semaphore(self.completion_semaphore, None);
        }
        self.completion_semaphore = replacement;
        self.completion_semaphore_reuse.finish_successful_export();
        Ok(())
    }

    /// Import renderer B's retained completion before recording an overwrite
    /// of the reused DMA-BUF transport. The subsequent preflight determines
    /// whether the command buffer records a FOREIGN -> A acquire barrier.
    pub(crate) fn prepare_renderer_acquire(&mut self) -> io::Result<()> {
        match self.ownership {
            CopiedSourceOwnership::RendererFirstUse | CopiedSourceOwnership::RendererDiscard => {
                Ok(())
            }
            CopiedSourceOwnership::ForeignAwaitingRenderer => {
                if self.renderer_return_completion.is_some()
                    && self.renderer_wait_semaphore.is_some()
                {
                    return Err(io::Error::other(
                        "copied source retained a new B completion before the prior A wait semaphore retired",
                    ));
                }
                if let Some(completion) = self.renderer_return_completion.take() {
                    let wait = crate::kms::vk::sync::import_optional_sync_file(
                        &self.render_vk,
                        completion.into_optional(),
                    )
                    .map_err(|result| {
                        scanout_vk_error("import copied sink completion on renderer", result)
                    })?;
                    self.renderer_wait_semaphore = Some(wait);
                } else if self.renderer_wait_semaphore.is_none() {
                    return Err(io::Error::other(
                        "copied source has no retained B completion for renderer acquire",
                    ));
                }
                Ok(())
            }
            CopiedSourceOwnership::ForeignAwaitingSink => Err(io::Error::other(
                "copied source cannot return to renderer before sink handoff",
            )),
            CopiedSourceOwnership::ForeignReturnPending => Err(io::Error::other(
                "copied source B-to-A completion is not resolved",
            )),
        }
    }

    #[must_use]
    pub(crate) fn renderer_wait_semaphore(&self) -> Option<vk::Semaphore> {
        self.renderer_wait_semaphore
    }

    pub(crate) fn transport_preparation(&self) -> io::Result<CopiedTransportPreparation> {
        self.ownership.transport_preparation()
    }

    /// Confirm that renderer A's local optimal target contains a completed or
    /// queue-ordered compose suitable for readback. The target never crosses
    /// devices, so this deliberately does not consume B's retained completion
    /// or mutate transport ownership.
    pub(crate) fn validate_renderer_readback(&self) -> io::Result<()> {
        self.render_target_contents.validate_readback()
    }

    pub(crate) fn note_renderer_submit_succeeded(&mut self) {
        debug_assert!(matches!(
            self.ownership,
            CopiedSourceOwnership::RendererFirstUse
                | CopiedSourceOwnership::RendererDiscard
                | CopiedSourceOwnership::ForeignAwaitingRenderer
        ));
        self.render_target_contents.note_submit_succeeded();
        self.ownership = CopiedSourceOwnership::ForeignAwaitingSink;
        self.renderer_return_completion = None;
    }

    fn note_sink_submit_succeeded(&mut self) {
        debug_assert_eq!(self.ownership, CopiedSourceOwnership::ForeignAwaitingSink);
        self.ownership = CopiedSourceOwnership::ForeignReturnPending;
    }

    fn retain_sink_release_completion(&mut self, completion: Option<OwnedFd>) {
        debug_assert_eq!(self.ownership, CopiedSourceOwnership::ForeignReturnPending);
        self.renderer_return_completion = Some(RetainedSyncFile::from_optional(completion));
        self.ownership = CopiedSourceOwnership::ForeignAwaitingRenderer;
    }

    fn recover_before_sink_submit_after_quiescence(&mut self) -> io::Result<()> {
        match self.ownership {
            CopiedSourceOwnership::ForeignAwaitingSink => {
                // B never acquired/released the source, so there is no
                // legitimate foreign return to pair with. The scene waits A's
                // compose fence before reuse; the next full repaint discards
                // from UNDEFINED and implicitly reacquires instead.
                self.renderer_return_completion = None;
                self.ownership = CopiedSourceOwnership::RendererDiscard;
                Ok(())
            }
            CopiedSourceOwnership::ForeignAwaitingRenderer => Ok(()),
            CopiedSourceOwnership::ForeignReturnPending => {
                // B's queue is idle, so its recorded B->FOREIGN release is
                // complete even though exporting/duplicating the sync_file
                // failed. Retain Vulkan's already-signalled sentinel for the
                // matching A acquire.
                self.renderer_return_completion = Some(RetainedSyncFile::AlreadySignalled);
                self.ownership = CopiedSourceOwnership::ForeignAwaitingRenderer;
                Ok(())
            }
            CopiedSourceOwnership::RendererFirstUse => Err(io::Error::other(
                "copied sink failure observed before renderer ownership release",
            )),
            CopiedSourceOwnership::RendererDiscard => Ok(()),
        }
    }

    /// Make a failed copied cycle reusable after the scene successfully waited
    /// renderer A's compose fence. This covers both an A handoff failure
    /// (ownership is still awaiting B) and a later B/copy/KMS failure (B's
    /// recovery already retained a return completion).
    pub(crate) fn recover_failed_cycle_after_renderer_quiescence(&mut self) -> io::Result<()> {
        self.render_target_contents.invalidate();
        self.rearm_completion_semaphore_after_quiescence()
            .map_err(|result| {
                scanout_vk_error(
                    "rearm copied renderer completion semaphore after failed handoff",
                    result,
                )
            })?;
        self.release_renderer_wait_semaphore();
        match self.ownership {
            CopiedSourceOwnership::ForeignAwaitingSink => {
                self.renderer_return_completion = None;
                self.ownership = CopiedSourceOwnership::RendererDiscard;
                Ok(())
            }
            CopiedSourceOwnership::ForeignAwaitingRenderer
            | CopiedSourceOwnership::RendererDiscard => Ok(()),
            CopiedSourceOwnership::RendererFirstUse
            | CopiedSourceOwnership::ForeignReturnPending => Err(io::Error::other(
                "copied failed-cycle recovery found unsafe ownership",
            )),
        }
    }

    fn reset_after_lifecycle_quiescence(&mut self) -> io::Result<()> {
        self.render_target_contents.invalidate();
        self.rearm_completion_semaphore_after_quiescence()
            .map_err(|result| {
                scanout_vk_error(
                    "rearm copied renderer completion semaphore after lifecycle quiescence",
                    result,
                )
            })?;
        self.release_sink_wait_semaphore();
        self.release_renderer_wait_semaphore();
        self.renderer_return_completion = None;
        self.ownership = self.ownership.after_lifecycle_quiescence();
        Ok(())
    }

    fn release_sink_wait_semaphore(&mut self) {
        if let Some(semaphore) = self.sink_wait_semaphore.take() {
            unsafe { self.sink_vk.device.destroy_semaphore(semaphore, None) };
        }
    }

    fn release_renderer_wait_semaphore(&mut self) {
        if let Some(semaphore) = self.renderer_wait_semaphore.take() {
            unsafe { self.render_vk.device.destroy_semaphore(semaphore, None) };
        }
    }

    fn imported_sink_image(&self) -> vk::Image {
        self.imported_on_sink
            .as_ref()
            .expect("live copied source has sink import")
            .vk_image
    }

    /// Record renderer A's post-compose copy into the selected transport.
    ///
    /// Queue-family ownership barriers and local layout transitions are kept
    /// in separate commands because overlapping transitions for one image in
    /// a single dependency info are not sequential. Only the DMA-BUF transport
    /// crosses FOREIGN; the optimal target remains renderer-local in GENERAL.
    pub(crate) fn record_transport_copy(
        &self,
        command_buffer: vk::CommandBuffer,
        preparation: CopiedTransportPreparation,
    ) {
        let device = &self.render_vk.device;
        unsafe {
            if preparation.foreign_acquire {
                let acquire = [vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                    .src_access_mask(vk::AccessFlags2::MEMORY_WRITE)
                    .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                    .dst_access_mask(vk::AccessFlags2::MEMORY_READ | vk::AccessFlags2::MEMORY_WRITE)
                    .src_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
                    .dst_queue_family_index(self.render_vk.graphics_queue_family)
                    .old_layout(vk::ImageLayout::GENERAL)
                    .new_layout(vk::ImageLayout::GENERAL)
                    .image(self.transport_image())
                    .subresource_range(color_subresource_range())];
                device.cmd_pipeline_barrier2(
                    command_buffer,
                    &vk::DependencyInfo::default().image_memory_barriers(&acquire),
                );
            }

            let local_to_copy = [
                vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
                    .src_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
                    .dst_stage_mask(vk::PipelineStageFlags2::COPY)
                    .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
                    .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                    .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                    .image(self.image())
                    .subresource_range(color_subresource_range()),
                vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(if preparation.foreign_acquire {
                        vk::PipelineStageFlags2::ALL_COMMANDS
                    } else {
                        vk::PipelineStageFlags2::TOP_OF_PIPE
                    })
                    .src_access_mask(if preparation.foreign_acquire {
                        vk::AccessFlags2::MEMORY_READ | vk::AccessFlags2::MEMORY_WRITE
                    } else {
                        vk::AccessFlags2::empty()
                    })
                    .dst_stage_mask(vk::PipelineStageFlags2::COPY)
                    .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                    .old_layout(preparation.local_old_layout)
                    .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                    .image(self.transport_image())
                    .subresource_range(color_subresource_range()),
            ];
            device.cmd_pipeline_barrier2(
                command_buffer,
                &vk::DependencyInfo::default().image_memory_barriers(&local_to_copy),
            );

            let regions = [vk::ImageCopy::default()
                .src_subresource(color_subresource_layers())
                .dst_subresource(color_subresource_layers())
                .extent(vk::Extent3D {
                    width: self.width(),
                    height: self.height(),
                    depth: 1,
                })];
            device.cmd_copy_image(
                command_buffer,
                self.image(),
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                self.transport_image(),
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &regions,
            );

            let local_to_general = [
                vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::COPY)
                    .src_access_mask(vk::AccessFlags2::TRANSFER_READ)
                    .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                    .dst_access_mask(vk::AccessFlags2::MEMORY_READ)
                    .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                    .new_layout(vk::ImageLayout::GENERAL)
                    .image(self.image())
                    .subresource_range(color_subresource_range()),
                vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::COPY)
                    .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                    .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                    .dst_access_mask(vk::AccessFlags2::MEMORY_READ)
                    .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                    .new_layout(vk::ImageLayout::GENERAL)
                    .image(self.transport_image())
                    .subresource_range(color_subresource_range()),
            ];
            device.cmd_pipeline_barrier2(
                command_buffer,
                &vk::DependencyInfo::default().image_memory_barriers(&local_to_general),
            );

            let release = [vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                .src_access_mask(vk::AccessFlags2::MEMORY_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                .dst_access_mask(vk::AccessFlags2::empty())
                .src_queue_family_index(self.render_vk.graphics_queue_family)
                .dst_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
                .old_layout(vk::ImageLayout::GENERAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .image(self.transport_image())
                .subresource_range(color_subresource_range())];
            device.cmd_pipeline_barrier2(
                command_buffer,
                &vk::DependencyInfo::default().image_memory_barriers(&release),
            );
        }
    }

    /// Copy the renderer-local optimal target into this source's tightly
    /// packed host-visible probe buffer. The caller records this only after
    /// [`Self::record_transport_copy`], while the target is back in `GENERAL`.
    /// The DMA-BUF transport remains FOREIGN-owned and is deliberately not
    /// touched by this diagnostic readback.
    pub(super) fn record_probe_readback(
        &self,
        command_buffer: vk::CommandBuffer,
        readback: CopiedProbeReadback<'_>,
    ) {
        let device = &self.render_vk.device;
        unsafe {
            let to_copy = [vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                .src_access_mask(vk::AccessFlags2::MEMORY_READ)
                .dst_stage_mask(vk::PipelineStageFlags2::COPY)
                .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
                .old_layout(vk::ImageLayout::GENERAL)
                .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .image(self.image())
                .subresource_range(color_subresource_range())];
            device.cmd_pipeline_barrier2(
                command_buffer,
                &vk::DependencyInfo::default().image_memory_barriers(&to_copy),
            );

            let regions = [tight_bgra_buffer_image_copy(self.width(), self.height())];
            crate::vk_count!(cmd_copy_image_to_buffer);
            device.cmd_copy_image_to_buffer(
                command_buffer,
                self.image(),
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                readback.destination_buffer(&self.transfer),
                &regions,
            );

            let image_to_general = [vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::COPY)
                .src_access_mask(vk::AccessFlags2::TRANSFER_READ)
                .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                .dst_access_mask(vk::AccessFlags2::MEMORY_READ)
                .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .image(self.image())
                .subresource_range(color_subresource_range())];
            match readback {
                CopiedProbeReadback::CpuExact => {
                    let buffer_to_host = [probe_buffer_to_host_barrier(&self.transfer)];
                    device.cmd_pipeline_barrier2(
                        command_buffer,
                        &vk::DependencyInfo::default()
                            .image_memory_barriers(&image_to_general)
                            .buffer_memory_barriers(&buffer_to_host),
                    );
                }
                CopiedProbeReadback::GpuDigest(digest) => {
                    device.cmd_pipeline_barrier2(
                        command_buffer,
                        &vk::DependencyInfo::default().image_memory_barriers(&image_to_general),
                    );
                    digest.record_after_transfer(command_buffer);
                }
            }
        }
    }

    fn probe_readback_bytes(&self) -> io::Result<&[u8]> {
        tight_mapped_bgra_bytes(&self.transfer, self.width(), self.height())
    }

    /// Preserve every Vulkan child and both owning contexts when GPU
    /// quiescence could not be proven. This is the copied-path analogue of
    /// [`ScanoutBo::disarm`]: leaking is safer than freeing referenced memory.
    fn disarm(&mut self) {
        if self.disarmed {
            return;
        }
        leak_owned_backing(&mut self.imported_on_sink);
        leak_owned_backing(&mut self.transport_on_renderer);
        leak_owned_backing(&mut self.render_target);
        std::mem::forget(Arc::clone(&self.render_vk));
        std::mem::forget(Arc::clone(&self.sink_vk));
        self.disarmed = true;
    }
}

impl Drop for CopiedRenderSource {
    fn drop(&mut self) {
        if self.disarmed {
            log::warn!(
                "copied source disarmed after failed GPU quiescence; leaking Vulkan resources"
            );
            return;
        }
        self.release_sink_wait_semaphore();
        self.release_renderer_wait_semaphore();
        unsafe {
            destroy_transfer_resources(&self.render_vk, &mut self.transfer);
            self.render_vk
                .device
                .destroy_semaphore(self.completion_semaphore, None);
        }
    }
}

impl CopiedScanoutPool {
    pub(crate) fn finish_disposable_probe(
        self,
        result: Result<(), DisposableProbeError>,
    ) -> Result<(), DisposableProbeError> {
        finish_disposable_probe_attempt(self, result)
    }

    /// Enumerate exact copied candidates in transport tiers. All mutually
    /// supported native transport modifiers are exhausted before explicit
    /// LINEAR; within each tier the established destination allocator order
    /// remains authoritative.
    #[must_use]
    pub(crate) fn exact_allocation_plans(
        render_vk: &VkContext,
        sink_vk: &VkContext,
        drm: &Rc<crate::drm::Device>,
        width: u32,
        scanout_modifiers: &[u64],
    ) -> Vec<CopiedScanoutPlan> {
        if render_vk.device_selector() == sink_vk.device_selector()
            || !render_vk.queue_family_foreign
            || !sink_vk.queue_family_foreign
        {
            return Vec::new();
        }
        let sources = exact_copied_source_plans(render_vk, sink_vk);
        let destinations =
            ScanoutBoPool::exact_allocation_plans(sink_vk, drm, width, scanout_modifiers);
        assemble_copied_scanout_plans(&destinations, &sources)
    }

    /// Allocate the complete copied pool using one exact plan for all slots.
    /// `route` is A->B; `destination_route` describes the sink Vulkan device
    /// writing the same B KMS endpoint and is stored on the destination pool.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn allocate_exact(
        render_vk: Arc<VkContext>,
        sink_vk: Arc<VkContext>,
        drm: Rc<crate::drm::Device>,
        route: ScanoutRoute,
        destination_route: ScanoutRoute,
        width: u32,
        height: u32,
        count: usize,
        scanout_modifiers: &[u64],
        plan: CopiedScanoutPlan,
    ) -> io::Result<Self> {
        Self::allocate_exact_with_policy(
            render_vk,
            sink_vk,
            drm,
            route,
            destination_route,
            width,
            height,
            count,
            scanout_modifiers,
            plan,
            AllocationCleanupPolicy::BestEffort,
        )
        .map_err(DisposableProbeError::into_io_error)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn allocate_exact_for_disposable_probe(
        render_vk: Arc<VkContext>,
        sink_vk: Arc<VkContext>,
        drm: Rc<crate::drm::Device>,
        route: ScanoutRoute,
        destination_route: ScanoutRoute,
        width: u32,
        height: u32,
        count: usize,
        scanout_modifiers: &[u64],
        plan: CopiedScanoutPlan,
    ) -> Result<Self, DisposableProbeError> {
        Self::allocate_exact_with_policy(
            render_vk,
            sink_vk,
            drm,
            route,
            destination_route,
            width,
            height,
            count,
            scanout_modifiers,
            plan,
            AllocationCleanupPolicy::StrictDisposable,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn allocate_exact_with_policy(
        render_vk: Arc<VkContext>,
        sink_vk: Arc<VkContext>,
        drm: Rc<crate::drm::Device>,
        route: ScanoutRoute,
        destination_route: ScanoutRoute,
        width: u32,
        height: u32,
        count: usize,
        scanout_modifiers: &[u64],
        plan: CopiedScanoutPlan,
        cleanup_policy: AllocationCleanupPolicy,
    ) -> Result<Self, DisposableProbeError> {
        validate_copied_route_pair(route, destination_route).map_err(DisposableProbeError::from)?;
        if render_vk.device_selector() == sink_vk.device_selector() {
            return Err(DisposableProbeError::from(io::Error::new(
                io::ErrorKind::InvalidInput,
                "copied scanout requires distinct renderer and sink Vulkan devices",
            )));
        }
        if !render_vk.queue_family_foreign || !sink_vk.queue_family_foreign {
            return Err(DisposableProbeError::from(io::Error::new(
                io::ErrorKind::Unsupported,
                "copied scanout requires VK_EXT_queue_family_foreign on renderer and sink",
            )));
        }

        let destinations = match cleanup_policy {
            AllocationCleanupPolicy::BestEffort => ScanoutBoPool::allocate_exact(
                Arc::clone(&sink_vk),
                drm,
                destination_route,
                width,
                height,
                count,
                scanout_modifiers,
                plan.destination,
            )
            .map_err(DisposableProbeError::from),
            AllocationCleanupPolicy::StrictDisposable => {
                ScanoutBoPool::allocate_exact_for_disposable_probe(
                    Arc::clone(&sink_vk),
                    drm,
                    destination_route,
                    width,
                    height,
                    count,
                    scanout_modifiers,
                    plan.destination,
                )
            }
        }
        .map_err(|error| {
            error.with_context(format!("exact copied {} destination pool", plan.describe()))
        })?;

        let initial_destination_ownership = match plan.destination.ownership() {
            ScanoutOwnership::Output => CopiedDestinationOwnership::ForeignImportedFirstUse,
            ScanoutOwnership::Renderer => CopiedDestinationOwnership::LocalFirstUse,
        };
        let mut sources = Vec::with_capacity(count);
        for index in 0..count {
            let source = CopiedRenderSource::allocate_exact(
                Arc::clone(&render_vk),
                Arc::clone(&sink_vk),
                width,
                height,
                plan.source,
            )
            .map_err(|error| {
                DisposableProbeError::from(scanout_io_context(
                    format!("exact copied {} source BO {index}", plan.describe()),
                    error,
                ))
            });
            match source {
                Ok(source) => sources.push(source),
                Err(error)
                    if matches!(cleanup_policy, AllocationCleanupPolicy::StrictDisposable) =>
                {
                    let partial_pool = Self {
                        sources,
                        destinations,
                        route,
                        plan,
                        sink_vk,
                        destination_ownership: vec![initial_destination_ownership; count],
                    };
                    return Err(partial_pool
                        .finish_disposable_probe(Err(error))
                        .expect_err("failed copied allocation cannot become a successful probe"));
                }
                Err(error) => return Err(error),
            }
        }

        Ok(Self {
            sources,
            destinations,
            route,
            plan,
            sink_vk,
            destination_ownership: vec![initial_destination_ownership; count],
        })
    }

    /// Submit B's copy after A's completion fd became readable. Readiness is
    /// scheduling only: B still imports and waits the synchronization payload.
    pub(crate) fn submit_copy(
        &mut self,
        bo_idx: usize,
        render_completion: Option<OwnedFd>,
    ) -> io::Result<Option<OwnedFd>> {
        self.submit_copy_with_fence(bo_idx, render_completion, vk::Fence::null(), None)
    }

    fn submit_copy_with_fence(
        &mut self,
        bo_idx: usize,
        render_completion: Option<OwnedFd>,
        fence: vk::Fence,
        probe_readback: Option<CopiedProbeReadback<'_>>,
    ) -> io::Result<Option<OwnedFd>> {
        let source = self
            .sources
            .get_mut(bo_idx)
            .ok_or_else(|| io::Error::other("copied scanout source index out of range"))?;
        let destination = self
            .destinations
            .bos
            .get_mut(bo_idx)
            .ok_or_else(|| io::Error::other("copied scanout destination index out of range"))?;
        let destination_ownership = self
            .destination_ownership
            .get_mut(bo_idx)
            .ok_or_else(|| io::Error::other("copied scanout ownership index out of range"))?;
        let destination_foreign_acquire = destination_ownership.foreign_acquire_layouts();
        let destination_local_old = destination_ownership.local_copy_old_layout()?;

        source.release_sink_wait_semaphore();
        // `None` is Vulkan's fd=-1 already-signalled SYNC_FD payload. It must
        // still be imported and waited: the semaphore wait is the external
        // memory dependency paired with renderer A's ownership release.
        let wait_semaphore =
            crate::kms::vk::sync::import_optional_sync_file(&self.sink_vk, render_completion)
                .map_err(|result| scanout_vk_error("import renderer completion on sink", result))?;
        source.sink_wait_semaphore = Some(wait_semaphore);

        let command_buffer = destination.vk_transfer.command_buffer;
        unsafe {
            self.sink_vk
                .device
                .reset_command_buffer(command_buffer, vk::CommandBufferResetFlags::empty())
                .map_err(|result| scanout_vk_error("reset copied sink command buffer", result))?;
            self.sink_vk
                .device
                .begin_command_buffer(
                    command_buffer,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )
                .map_err(|result| scanout_vk_error("begin copied sink command buffer", result))?;

            // Ownership acquires and local layout transitions are deliberately
            // separate commands. Two overlapping barriers in one dependency
            // info do not sequence GENERAL->GENERAL before GENERAL->TRANSFER.
            let mut ownership_acquires = Vec::with_capacity(2);
            ownership_acquires.push(
                vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                    .src_access_mask(vk::AccessFlags2::MEMORY_WRITE)
                    .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                    .dst_access_mask(vk::AccessFlags2::MEMORY_READ)
                    .src_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
                    .dst_queue_family_index(self.sink_vk.graphics_queue_family)
                    .old_layout(vk::ImageLayout::GENERAL)
                    .new_layout(vk::ImageLayout::GENERAL)
                    .image(source.imported_sink_image())
                    .subresource_range(color_subresource_range()),
            );
            if let Some((old_layout, new_layout)) = destination_foreign_acquire {
                ownership_acquires.push(
                    vk::ImageMemoryBarrier2::default()
                        .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                        .src_access_mask(vk::AccessFlags2::MEMORY_READ)
                        .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                        .dst_access_mask(vk::AccessFlags2::MEMORY_WRITE)
                        .src_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
                        .dst_queue_family_index(self.sink_vk.graphics_queue_family)
                        .old_layout(old_layout)
                        .new_layout(new_layout)
                        .image(destination.vk_image)
                        .subresource_range(color_subresource_range()),
                );
            }
            self.sink_vk.device.cmd_pipeline_barrier2(
                command_buffer,
                &vk::DependencyInfo::default().image_memory_barriers(&ownership_acquires),
            );

            let local_to_copy = [
                vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                    .src_access_mask(vk::AccessFlags2::MEMORY_WRITE)
                    .dst_stage_mask(vk::PipelineStageFlags2::COPY)
                    .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
                    .old_layout(vk::ImageLayout::GENERAL)
                    .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                    .image(source.imported_sink_image())
                    .subresource_range(color_subresource_range()),
                vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                    .src_access_mask(vk::AccessFlags2::MEMORY_READ)
                    .dst_stage_mask(vk::PipelineStageFlags2::COPY)
                    .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                    .old_layout(destination_local_old)
                    .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                    .image(destination.vk_image)
                    .subresource_range(color_subresource_range()),
            ];
            self.sink_vk.device.cmd_pipeline_barrier2(
                command_buffer,
                &vk::DependencyInfo::default().image_memory_barriers(&local_to_copy),
            );
            let regions = [vk::ImageCopy::default()
                .src_subresource(color_subresource_layers())
                .dst_subresource(color_subresource_layers())
                .extent(vk::Extent3D {
                    width: source.width(),
                    height: source.height(),
                    depth: 1,
                })];
            self.sink_vk.device.cmd_copy_image(
                command_buffer,
                source.imported_sink_image(),
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                destination.vk_image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &regions,
            );
            let source_to_general = [vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::COPY)
                .src_access_mask(vk::AccessFlags2::TRANSFER_READ)
                .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                .dst_access_mask(vk::AccessFlags2::empty())
                .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .image(source.imported_sink_image())
                .subresource_range(color_subresource_range())];
            let destination_after_copy = [vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::COPY)
                .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .dst_stage_mask(if probe_readback.is_some() {
                    vk::PipelineStageFlags2::COPY
                } else {
                    vk::PipelineStageFlags2::ALL_COMMANDS
                })
                .dst_access_mask(if probe_readback.is_some() {
                    vk::AccessFlags2::TRANSFER_READ
                } else {
                    vk::AccessFlags2::MEMORY_READ
                })
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(if probe_readback.is_some() {
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL
                } else {
                    vk::ImageLayout::GENERAL
                })
                .image(destination.vk_image)
                .subresource_range(color_subresource_range())];
            self.sink_vk.device.cmd_pipeline_barrier2(
                command_buffer,
                &vk::DependencyInfo::default().image_memory_barriers(&source_to_general),
            );
            self.sink_vk.device.cmd_pipeline_barrier2(
                command_buffer,
                &vk::DependencyInfo::default().image_memory_barriers(&destination_after_copy),
            );

            if let Some(readback) = probe_readback {
                let regions = [tight_bgra_buffer_image_copy(
                    source.width(),
                    source.height(),
                )];
                crate::vk_count!(cmd_copy_image_to_buffer);
                self.sink_vk.device.cmd_copy_image_to_buffer(
                    command_buffer,
                    destination.vk_image,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    readback.destination_buffer(&destination.vk_transfer),
                    &regions,
                );

                let destination_to_general = [vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::COPY)
                    .src_access_mask(vk::AccessFlags2::TRANSFER_READ)
                    .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                    .dst_access_mask(vk::AccessFlags2::MEMORY_READ)
                    .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                    .new_layout(vk::ImageLayout::GENERAL)
                    .image(destination.vk_image)
                    .subresource_range(color_subresource_range())];
                match readback {
                    CopiedProbeReadback::CpuExact => {
                        let buffer_to_host =
                            [probe_buffer_to_host_barrier(&destination.vk_transfer)];
                        self.sink_vk.device.cmd_pipeline_barrier2(
                            command_buffer,
                            &vk::DependencyInfo::default()
                                .image_memory_barriers(&destination_to_general)
                                .buffer_memory_barriers(&buffer_to_host),
                        );
                    }
                    CopiedProbeReadback::GpuDigest(digest) => {
                        self.sink_vk.device.cmd_pipeline_barrier2(
                            command_buffer,
                            &vk::DependencyInfo::default()
                                .image_memory_barriers(&destination_to_general),
                        );
                        digest.record_after_transfer(command_buffer);
                    }
                }
            }

            let ownership_releases = [
                vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                    .src_access_mask(vk::AccessFlags2::MEMORY_READ)
                    .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                    .dst_access_mask(vk::AccessFlags2::empty())
                    .src_queue_family_index(self.sink_vk.graphics_queue_family)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
                    .old_layout(vk::ImageLayout::GENERAL)
                    .new_layout(vk::ImageLayout::GENERAL)
                    .image(source.imported_sink_image())
                    .subresource_range(color_subresource_range()),
                vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                    .src_access_mask(vk::AccessFlags2::MEMORY_WRITE)
                    .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                    .dst_access_mask(vk::AccessFlags2::empty())
                    .src_queue_family_index(self.sink_vk.graphics_queue_family)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
                    .old_layout(vk::ImageLayout::GENERAL)
                    .new_layout(vk::ImageLayout::GENERAL)
                    .image(destination.vk_image)
                    .subresource_range(color_subresource_range()),
            ];
            self.sink_vk.device.cmd_pipeline_barrier2(
                command_buffer,
                &vk::DependencyInfo::default().image_memory_barriers(&ownership_releases),
            );
            self.sink_vk
                .device
                .end_command_buffer(command_buffer)
                .map_err(|result| scanout_vk_error("end copied sink command buffer", result))?;

            let waits = [vk::SemaphoreSubmitInfo::default()
                .semaphore(wait_semaphore)
                .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)];
            let commands = [vk::CommandBufferSubmitInfo::default().command_buffer(command_buffer)];
            let signals = [vk::SemaphoreSubmitInfo::default()
                .semaphore(destination.vk_semaphore)
                .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)];
            let submits = [vk::SubmitInfo2::default()
                .wait_semaphore_infos(&waits)
                .command_buffer_infos(&commands)
                .signal_semaphore_infos(&signals)];
            crate::kms::vk::submit_stats::timed(
                crate::kms::vk::submit_stats::SubmitCause::Scanout,
                1,
                false,
                || {
                    self.sink_vk
                        .device
                        .queue_submit2(self.sink_vk.graphics_queue, &submits, fence)
                },
            )
            .map_err(|result| scanout_vk_error("submit copied sink transfer", result))?;
        }
        source.note_sink_submit_succeeded();
        *destination_ownership = CopiedDestinationOwnership::ForeignPendingKmsFromSink;
        let completion = destination
            .export_signaled_fd()
            .map_err(|result| scanout_vk_error("export copied sink completion", result))?;
        let renderer_completion = completion
            .as_ref()
            .map(OwnedFd::try_clone)
            .transpose()
            .map_err(|error| {
                scanout_io_context("retain copied sink completion for renderer acquire", error)
            })?;
        source.retain_sink_release_completion(renderer_completion);
        Ok(completion)
    }

    /// Probe all exact slots with a spatially unique A render, external
    /// semaphore handoff, B copy, and CPU-visible content validation. The
    /// normal path compares compact GPU-computed block digests; unsupported
    /// compute queues retain the exact full-image CPU fallback.
    /// Each slot runs twice: cycle two consumes B's retained
    /// completion on A, covering both directions of the source ownership
    /// protocol rather than admitting a route after only its first A -> B
    /// handoff. TEST_ONLY does not perform a KMS ownership release, so the
    /// destination is explicitly abandoned after cycle one and cycle two
    /// full-discards from UNDEFINED; GENERAL KMS -> B reuse requires a live
    /// two-flip hardware check. Each submitted renderer and sink batch gets a
    /// fresh bounded fence wait. Pipeline creation, command recording, and CPU
    /// validation are not compatibility failures merely because they take
    /// longer than that GPU-liveness timeout. An actual fence timeout or other
    /// uncertain post-submit failure consumes and quarantines this disposable
    /// pool so no GPU-referenced object reaches normal teardown.
    pub(crate) fn probe_copy_all(self, timeout_ns: u64) -> Result<(), DisposableProbeError> {
        let probe_started = Instant::now();
        let mut attempt = CopiedDisposableProbeAttempt {
            pool: self,
            pattern: None,
            render_digest: None,
            sink_digest: None,
        };
        let render_vk = match attempt.pool.sources.first() {
            Some(source) => Arc::clone(&source.render_vk),
            None => {
                return finish_disposable_probe_attempt(
                    attempt,
                    Err(DisposableProbeError::from(io::Error::other(
                        "copied probe pool has no source slots",
                    ))),
                );
            }
        };
        let pipeline_started = Instant::now();
        let pattern = match CopiedProbePatternPipeline::new(Arc::clone(&render_vk)) {
            Ok(pattern) => pattern,
            Err(result) => {
                // Pipeline creation performs no queue submission. The pool's
                // disposable contexts are therefore safe to destroy directly
                // even though generic context Drop remains conservative.
                return finish_disposable_probe_attempt(
                    attempt,
                    Err(DisposableProbeError::from(scanout_vk_error(
                        "create copied content-probe pipeline",
                        result,
                    ))),
                );
            }
        };
        attempt.pattern = Some(pattern);

        let sink_vk = Arc::clone(&attempt.pool.sink_vk);
        if ProbeDigestPipeline::is_supported(
            &render_vk,
            attempt.pool.destinations.width,
            attempt.pool.destinations.height,
        ) && ProbeDigestPipeline::is_supported(
            &sink_vk,
            attempt.pool.destinations.width,
            attempt.pool.destinations.height,
        ) {
            match ProbeDigestPipeline::new(
                Arc::clone(&render_vk),
                attempt.pool.destinations.width,
                attempt.pool.destinations.height,
            ) {
                Ok(digest) => attempt.render_digest = Some(digest),
                Err(vk::Result::ERROR_DEVICE_LOST) => {
                    return finish_disposable_probe_attempt(
                        attempt,
                        Err(DisposableProbeError::from(scanout_vk_error(
                            "create copied renderer GPU digest",
                            vk::Result::ERROR_DEVICE_LOST,
                        ))),
                    );
                }
                Err(result) => log::warn!(
                    "copied content probe could not create renderer GPU digest ({result:?}); \
                     falling back to exact CPU validation"
                ),
            }
            if attempt.render_digest.is_some() {
                match ProbeDigestPipeline::new(
                    sink_vk,
                    attempt.pool.destinations.width,
                    attempt.pool.destinations.height,
                ) {
                    Ok(digest) => attempt.sink_digest = Some(digest),
                    Err(vk::Result::ERROR_DEVICE_LOST) => {
                        return finish_disposable_probe_attempt(
                            attempt,
                            Err(DisposableProbeError::from(scanout_vk_error(
                                "create copied sink GPU digest",
                                vk::Result::ERROR_DEVICE_LOST,
                            ))),
                        );
                    }
                    Err(result) => log::warn!(
                        "copied content probe could not create sink GPU digest ({result:?}); \
                         falling back to exact CPU validation"
                    ),
                }
            }
        } else {
            log::warn!(
                "copied content probe selected queue or extent cannot run compact GPU digest; \
                 falling back to exact CPU validation"
            );
        }

        let pipeline_elapsed = pipeline_started.elapsed();
        if let Some((render_digest, sink_digest)) = attempt
            .render_digest
            .as_ref()
            .zip(attempt.sink_digest.as_ref())
        {
            debug_assert_eq!(render_digest.grid_width(), sink_digest.grid_width());
            debug_assert_eq!(render_digest.grid_height(), sink_digest.grid_height());
            debug_assert_eq!(
                render_digest.summary_word_count(),
                sink_digest.summary_word_count()
            );
            log::info!(
                "copied content probe pipelines ready in {} ms; validator=gpu-block-digest \
                 grid={}x{} summary={} bytes/device; each renderer/sink fence wait has {:?}",
                pipeline_elapsed.as_millis(),
                render_digest.grid_width(),
                render_digest.grid_height(),
                render_digest.summary_word_count() * std::mem::size_of::<u32>(),
                Duration::from_nanos(timeout_ns),
            );
        } else {
            log::info!(
                "copied content probe pipeline ready in {} ms; validator=cpu-exact; each \
                 renderer/sink fence wait has {:?}",
                pipeline_elapsed.as_millis(),
                Duration::from_nanos(timeout_ns),
            );
        }

        let result = attempt.pool.probe_copy_all_inner(
            &render_vk,
            attempt.pattern.as_ref().expect("probe pattern was created"),
            attempt
                .render_digest
                .as_ref()
                .zip(attempt.sink_digest.as_ref()),
            timeout_ns,
        );
        if result.is_ok() {
            log::info!(
                "copied content probe completed {} slots x 2 cycles in {} ms; per-fence timeout {:?}",
                attempt.pool.sources.len(),
                probe_started.elapsed().as_millis(),
                Duration::from_nanos(timeout_ns),
            );
        }
        if result
            .as_ref()
            .is_err_and(DisposableProbeError::bypass_normal_teardown)
        {
            log::error!(
                "copied content probe timed out or left GPU completion uncertain; retaining the \
                 disposable A/B pool and pipeline without vkDeviceWaitIdle"
            );
        }
        finish_disposable_probe_attempt(attempt, result)
    }

    fn probe_copy_all_inner(
        &mut self,
        render_vk: &Arc<VkContext>,
        pattern: &CopiedProbePatternPipeline,
        digests: Option<(&ProbeDigestPipeline, &ProbeDigestPipeline)>,
        timeout_ns: u64,
    ) -> Result<(), DisposableProbeError> {
        for bo_idx in 0..self.sources.len() {
            let mut previous_renderer_hash = None;
            let mut previous_renderer_digest: Option<Vec<u32>> = None;
            for cycle in 0..2 {
                let cycle_started = Instant::now();
                let sink_vk = Arc::clone(&self.sink_vk);
                let (renderer_readback, sink_readback) = match digests {
                    Some((renderer, sink)) => (
                        CopiedProbeReadback::GpuDigest(renderer),
                        CopiedProbeReadback::GpuDigest(sink),
                    ),
                    None => (CopiedProbeReadback::CpuExact, CopiedProbeReadback::CpuExact),
                };
                let frame_token = u32::try_from(bo_idx)
                    .ok()
                    .and_then(|index| index.checked_mul(2))
                    .and_then(|base| base.checked_add(cycle))
                    .ok_or_else(|| io::Error::other("copied probe frame token overflow"))?;
                let render_fence = create_probe_fence(render_vk)?;
                let mut render_fence = ProbeFence::new(&render_vk.device, render_fence);
                let sink_fence = match create_probe_fence(&sink_vk) {
                    Ok(fence) => fence,
                    Err(error) => {
                        render_fence.destroy_known_idle();
                        return Err(DisposableProbeError::from(error).with_context(format!(
                            "BO {bo_idx} cycle {cycle} copied sink fence creation"
                        )));
                    }
                };
                let mut sink_fence = ProbeFence::new(&sink_vk.device, sink_fence);
                let render_completion = match submit_copied_source_probe(
                    &mut self.sources[bo_idx],
                    pattern,
                    renderer_readback,
                    frame_token,
                    render_fence.handle(),
                ) {
                    Ok(completion) => completion,
                    Err(error) => {
                        let pending = if error.requires_quarantine() {
                            PendingProbeSubmissions::Render
                        } else {
                            PendingProbeSubmissions::None
                        };
                        return Err(finish_pending_probe_failure(
                            pending,
                            error,
                            &mut render_fence,
                            &mut sink_fence,
                        )
                        .with_context(format!(
                            "BO {bo_idx} cycle {cycle} copied renderer submission"
                        )));
                    }
                };
                let renderer_submitted = Instant::now();

                // The sink helper may fail either side of its queue-submit
                // call. A is already outstanding, so conservatively retain
                // both fence handles and the aggregate attempt on any error.
                let copy_completion = match self.submit_copy_with_fence(
                    bo_idx,
                    render_completion,
                    sink_fence.handle(),
                    Some(sink_readback),
                ) {
                    Ok(completion) => completion,
                    Err(error) => {
                        return Err(finish_pending_probe_failure(
                            PendingProbeSubmissions::RenderAndSink,
                            DisposableProbeError::from(error),
                            &mut render_fence,
                            &mut sink_fence,
                        )
                        .with_context(format!(
                            "BO {bo_idx} cycle {cycle} copied sink submission"
                        )));
                    }
                };
                drop(copy_completion);
                let sink_submitted = Instant::now();

                let fence_waits =
                    wait_copied_probe_fence_pair(&mut render_fence, &mut sink_fence, timeout_ns)
                        .map_err(|error| {
                            error.with_context(format!(
                                "BO {bo_idx} cycle {cycle} copied fence completion"
                            ))
                        })?;
                let sink_completed = Instant::now();
                let validation = (|| -> Result<(), DisposableProbeError> {
                    self.release_completed_source(bo_idx);
                    match digests {
                        Some((renderer, sink)) => {
                            let renderer_summary = renderer.read_summary().map_err(|result| {
                                copied_probe_digest_readback_error(
                                    "read copied renderer GPU digest",
                                    result,
                                )
                            })?;
                            let sink_summary = sink.read_summary().map_err(|result| {
                                copied_probe_digest_readback_error(
                                    "read copied sink GPU digest",
                                    result,
                                )
                            })?;
                            validate_copied_probe_digest_fiducials(
                                &renderer_summary,
                                bo_idx,
                                cycle,
                                frame_token,
                            )?;
                            verify_copied_probe_digests(
                                &renderer_summary,
                                &sink_summary,
                                renderer.grid_width(),
                                renderer.grid_height(),
                                bo_idx,
                                cycle,
                                frame_token,
                            )?;
                            validate_copied_probe_digest_freshness(
                                previous_renderer_digest.as_deref(),
                                &renderer_summary,
                                bo_idx,
                                cycle,
                                frame_token,
                            )?;
                            previous_renderer_digest = Some(renderer_summary);
                        }
                        None => {
                            let renderer_pixels = self.sources[bo_idx].probe_readback_bytes()?;
                            let sink_pixels = tight_mapped_bgra_bytes(
                                &self.destinations.bos[bo_idx].vk_transfer,
                                self.sources[bo_idx].width(),
                                self.sources[bo_idx].height(),
                            )?;
                            validate_copied_probe_fiducials(
                                renderer_pixels,
                                self.sources[bo_idx].width(),
                                self.sources[bo_idx].height(),
                                bo_idx,
                                cycle,
                                frame_token,
                            )?;
                            let renderer_hash = verify_copied_probe_pixels(
                                renderer_pixels,
                                sink_pixels,
                                self.sources[bo_idx].width(),
                                self.sources[bo_idx].height(),
                                bo_idx,
                                cycle,
                                frame_token,
                            )?;
                            validate_copied_probe_freshness(
                                previous_renderer_hash,
                                renderer_hash,
                                bo_idx,
                                cycle,
                                frame_token,
                            )?;
                            previous_renderer_hash = Some(renderer_hash);
                        }
                    }
                    if cycle == 0 {
                        // No real KMS commit acquired/released the destination.
                        // After B is proven idle, recover it as an atomic reject
                        // and let the next full copy discard from UNDEFINED rather
                        // than fabricating an external GENERAL return.
                        self.recover_copy_failure_after_quiescence(bo_idx)?;
                    }
                    Ok(())
                })();
                let validation_completed = Instant::now();
                let validation_verdict = match validation.as_ref() {
                    Ok(()) => "match",
                    Err(error) if scanout_error_is_device_lost(error.as_io_error()) => {
                        "device-lost"
                    }
                    Err(error) if error.abort_candidate_search() => "indeterminate",
                    Err(_) => "reject",
                };
                log::info!(
                    "copied content probe cycle: bo={bo_idx} cycle={cycle} verdict={} \
                     validator={} \
                     renderer-submit={}ms sink-submit={}ms renderer-wait={}ms sink-wait={}ms \
                     validation={}ms total={}ms per-fence-timeout={:?}",
                    validation_verdict,
                    if digests.is_some() {
                        "gpu-block-digest"
                    } else {
                        "cpu-exact"
                    },
                    renderer_submitted.duration_since(cycle_started).as_millis(),
                    sink_submitted
                        .duration_since(renderer_submitted)
                        .as_millis(),
                    fence_waits.renderer.as_millis(),
                    fence_waits.sink.as_millis(),
                    validation_completed
                        .duration_since(sink_completed)
                        .as_millis(),
                    validation_completed
                        .duration_since(cycle_started)
                        .as_millis(),
                    Duration::from_nanos(timeout_ns),
                );
                completed_probe_validation(validation)?;
            }
        }
        Ok(())
    }

    pub(crate) fn release_completed_source(&mut self, bo_idx: usize) {
        if let Some(source) = self.sources.get_mut(bo_idx) {
            source.release_sink_wait_semaphore();
            // B completion (and therefore its wait on A) has retired. The
            // temporary B->A wait from the previous A submission is no longer
            // GPU-referenced and must be destroyed before importing the next
            // cycle's retained completion.
            source.release_renderer_wait_semaphore();
        }
    }

    pub(super) fn mark_disposable_probe_quiescent(&self) {
        self.sink_vk.mark_disposable_probe_quiescent();
        for source in &self.sources {
            source.render_vk.mark_disposable_probe_quiescent();
        }
    }

    /// Record that a later flip retired this destination from KMS. Only this
    /// replacement boundary makes the slot eligible for a subsequent B
    /// ownership acquire and write.
    pub(crate) fn note_kms_retired(&mut self, bo_idx: usize) -> io::Result<()> {
        let ownership = self
            .destination_ownership
            .get_mut(bo_idx)
            .ok_or_else(|| io::Error::other("copied scanout ownership index out of range"))?;
        *ownership = ownership.after_kms_retirement(bo_idx)?;
        Ok(())
    }

    /// A synchronous modeset may install a fresh destination without a prior
    /// B submission. Once it succeeds, KMS is nevertheless the external
    /// owner, so the next B write must acquire from FOREIGN.
    pub(crate) fn note_kms_modeset_installed(&mut self, bo_idx: usize) -> io::Result<()> {
        let ownership = self
            .destination_ownership
            .get_mut(bo_idx)
            .ok_or_else(|| io::Error::other("copied scanout ownership index out of range"))?;
        // Preserve whether GENERAL was established by a prior real B release.
        // A fresh/directly-modeset image remains layout-uninitialized even
        // after KMS has displayed it.
        *ownership = ownership.after_kms_modeset();
        Ok(())
    }

    /// Quiesce a failed B submission before returning the pair to the free
    /// list. Other slots, including the one currently scanned out, retain
    /// their state.
    pub(crate) fn recover_copy_failure(&mut self, bo_idx: usize) -> io::Result<()> {
        copied_quiescence_result("quiesce sink after copied scanout failure", unsafe {
            self.sink_vk.device.device_wait_idle()
        })?;
        self.recover_copy_failure_after_quiescence(bo_idx)
    }

    /// Recover a disposable copied-probe cycle after its explicit A and B
    /// fences have both signalled. Unlike live failure recovery, this proven
    /// boundary needs no device-wide idle wait.
    fn recover_copy_failure_after_quiescence(&mut self, bo_idx: usize) -> io::Result<()> {
        let source = self
            .sources
            .get_mut(bo_idx)
            .ok_or_else(|| io::Error::other("copied scanout source index out of range"))?;
        source.release_sink_wait_semaphore();
        source.recover_before_sink_submit_after_quiescence()?;
        let destination_ownership = self
            .destination_ownership
            .get_mut(bo_idx)
            .ok_or_else(|| io::Error::other("copied scanout ownership index out of range"))?;
        if *destination_ownership == CopiedDestinationOwnership::ForeignPendingKmsFromSink {
            // B did release to FOREIGN, but KMS never accepted/acquired it.
            // The next guaranteed-full copy discards from UNDEFINED rather
            // than inventing a matching external release.
            *destination_ownership = CopiedDestinationOwnership::ReleasedButAtomicRejected;
        }
        if let Some(destination) = self.destinations.bos.get_mut(bo_idx) {
            destination
                .rearm_export_semaphore_after_quiescence()
                .map_err(|result| {
                    scanout_vk_error(
                        "rearm copied sink completion semaphore after failed export",
                        result,
                    )
                })?;
        }
        Ok(())
    }

    pub(crate) fn drain_all_pending(&mut self) -> io::Result<()> {
        copied_quiescence_result("quiesce copied scanout sink", unsafe {
            self.sink_vk.device.device_wait_idle()
        })?;
        if let Some(render_vk) = self
            .sources
            .first()
            .map(|source| Arc::clone(&source.render_vk))
        {
            copied_quiescence_result("quiesce copied scanout renderer", unsafe {
                render_vk.device.device_wait_idle()
            })?;
        }
        for destination in &mut self.destinations.bos {
            destination
                .rearm_export_semaphore_after_quiescence()
                .map_err(|result| {
                    scanout_vk_error(
                        "rearm copied destination semaphore after lifecycle quiescence",
                        result,
                    )
                })?;
            close_modeset_released(destination.state.transition_to_free_after_modeset_reset());
        }
        for ownership in &mut self.destination_ownership {
            *ownership = ownership.after_lifecycle_quiescence();
        }
        for source in &mut self.sources {
            source.reset_after_lifecycle_quiescence()?;
        }
        Ok(())
    }

    fn disarm_uncertain_resources(&mut self) {
        for source in &mut self.sources {
            source.disarm();
        }
        for destination in &mut self.destinations.bos {
            destination.disarm();
        }
        // Destination BOs each drop one Arc, but the context itself must stay
        // alive because their disarmed raw handles still belong to it.
        std::mem::forget(Arc::clone(&self.sink_vk));
    }

    pub(super) fn disarm_display_backing(&mut self) {
        for destination in &mut self.destinations.bos {
            destination.disarm();
        }
        // A copied sink context has no platform-global owner. Keep it alive so
        // disarmed destination VkImages remain backed while KMS may retain the
        // framebuffer after a failed final disable.
        std::mem::forget(Arc::clone(&self.sink_vk));
    }
}

impl Drop for CopiedScanoutPool {
    fn drop(&mut self) {
        let render_requires_idle = self
            .sources
            .iter()
            .any(|source| source.render_vk.requires_drop_device_idle());
        if !self.sink_vk.requires_drop_device_idle() && !render_requires_idle {
            return;
        }
        if let Err(error) = self.drain_all_pending() {
            log::error!(
                "copied scanout drop could not prove GPU quiescence ({error}); \
                 disarming and leaking uncertain resources"
            );
            self.disarm_uncertain_resources();
        }
    }
}

pub(super) fn color_subresource_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1)
}

pub(super) fn color_subresource_layers() -> vk::ImageSubresourceLayers {
    vk::ImageSubresourceLayers::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .layer_count(1)
}
