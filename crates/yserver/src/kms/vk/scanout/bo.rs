use super::*;

pub(super) fn release_drm_handles_strict<Fb, Gem>(
    framebuffer: &mut Option<Fb>,
    gem: &mut Option<Gem>,
    mut destroy_framebuffer: impl FnMut(Fb) -> io::Result<()>,
    mut close_gem: impl FnMut(Gem) -> io::Result<()>,
) -> io::Result<()>
where
    Fb: Copy,
    Gem: Copy,
{
    if let Some(handle) = *framebuffer {
        destroy_framebuffer(handle)?;
        *framebuffer = None;
    }
    if let Some(handle) = *gem {
        close_gem(handle)?;
        *gem = None;
    }
    Ok(())
}

impl PartialScanoutBoAllocation {
    fn release_drm_strict(&mut self) -> io::Result<()> {
        release_drm_handles_strict(
            &mut self.framebuffer,
            &mut self.gem,
            |framebuffer| {
                self.drm.destroy_framebuffer(framebuffer).map_err(|error| {
                    scanout_io_context(
                        format!("destroy partial disposable framebuffer {framebuffer:?}"),
                        error,
                    )
                })
            },
            |gem| {
                self.drm.close_buffer(gem).map_err(|error| {
                    scanout_io_context(
                        format!("close partial disposable GEM handle {gem:?}"),
                        error,
                    )
                })
            },
        )
    }

    fn release_drm_best_effort(&mut self) {
        if let Some(framebuffer) = self.framebuffer.take()
            && let Err(error) = self.drm.destroy_framebuffer(framebuffer)
        {
            log::warn!("drm destroy partial framebuffer failed: {error}");
        }
        if let Some(gem) = self.gem.take()
            && let Err(error) = self.drm.close_buffer(gem)
        {
            log::warn!("drm close partial GEM handle failed: {error}");
        }
    }

    fn destroy_backing(mut self) {
        unsafe {
            if let Some(mut transfer) = self.transfer.take() {
                destroy_transfer_resources(&self.vk, &mut transfer);
            }
            if let Some(image_view) = self.image_view.take() {
                self.vk.device.destroy_image_view(image_view, None);
            }
            if let Some(semaphore) = self.semaphore.take() {
                self.vk.device.destroy_semaphore(semaphore, None);
            }
        }
        destroy_scanout_image(&self.vk, self.image, self.memory);
    }

    fn rollback(
        mut self,
        original: io::Error,
        policy: AllocationCleanupPolicy,
    ) -> DisposableProbeError {
        match policy {
            AllocationCleanupPolicy::BestEffort => {
                self.release_drm_best_effort();
                self.destroy_backing();
                DisposableProbeError::from(original)
            }
            AllocationCleanupPolicy::StrictDisposable => match self.release_drm_strict() {
                Ok(()) => {
                    self.destroy_backing();
                    DisposableProbeError::from(original)
                }
                Err(cleanup) => {
                    let cleanup = io::Error::new(
                        cleanup.kind(),
                        format!(
                            "strict partial scanout rollback failed after {original}; retaining \
                            backing: {cleanup}"
                        ),
                    );
                    // `self` is the retention anchor: it owns the VkContext
                    // Arc, DRM Rc, optional GBM BO, and every raw Vulkan/KMS
                    // handle created so far. Forgetting the complete staged
                    // owner keeps backing alive until the isolated helper is
                    // killed; retaining only the raw handles would allow the
                    // VkDevice or GBM allocation to disappear underneath the
                    // parent-shared GEM/FB registration.
                    std::mem::forget(self);
                    DisposableProbeError::terminal_cleanup(cleanup)
                }
            },
        }
    }
}

impl ScanoutBo {
    /// Allocate one scanout bo: GBM-alloc or Vulkan-alloc dma-buf +
    /// DRM framebuffer registration. All steps must succeed; partial
    /// allocations are unwound on error so the returned `Err` leaves
    /// no resources leaked.
    pub fn allocate(
        vk: Arc<VkContext>,
        drm: Rc<crate::drm::Device>,
        gbm: Option<Rc<GbmDevice>>,
        width: u32,
        height: u32,
        scanout_modifiers: &[u64],
    ) -> io::Result<Self> {
        let output_owned_modifiers =
            scanout_modifier_candidates(&vk, scanout_modifiers, ScanoutOwnership::Output);
        let renderer_owned_modifiers =
            scanout_modifier_candidates(&vk, scanout_modifiers, ScanoutOwnership::Renderer);
        let plans = scanout_allocation_plans(
            &vk,
            &output_owned_modifiers,
            &renderer_owned_modifiers,
            width,
            gbm.is_some(),
        );
        let mut errors = Vec::new();

        for plan in plans {
            match Self::allocate_with_plan(
                Arc::clone(&vk),
                Rc::clone(&drm),
                gbm.as_ref().map(Rc::clone),
                width,
                height,
                plan,
            ) {
                Ok(bo) => {
                    log::info!(
                        "scanout bo: {} succeeded ({}x{}, pitch {})",
                        plan.describe(),
                        width,
                        height,
                        bo.pitch,
                    );
                    return Ok(bo);
                }
                Err(e) => {
                    // Log every rejected plan, not just the winner. Which
                    // plans a card silently falls THROUGH is the thing that
                    // distinguishes one NVIDIA generation from another (an
                    // Ampere box on driver 595 fails GBM-LINEAR and scans out
                    // block-linear tiled; a Pascal GTX 1050 takes LINEAR), and
                    // it was invisible in every user log until now because
                    // `errors` only ever surfaced when EVERY plan failed.
                    // INFO, not WARN: falling through is normal operation —
                    // the aggregate failure below is the actual error.
                    log::info!("scanout bo: {} failed: {e}", plan.describe());
                    errors.push(format!("{}: {e}", plan.describe()));
                }
            }
        }

        Err(io::Error::other(format!(
            "scanout allocation failed for every path: {}",
            errors.join("; ")
        )))
    }

    pub(super) fn allocate_with_plan(
        vk: Arc<VkContext>,
        drm: Rc<crate::drm::Device>,
        gbm: Option<Rc<GbmDevice>>,
        width: u32,
        height: u32,
        plan: ScanoutAllocationPlan,
    ) -> io::Result<Self> {
        Self::allocate_with_plan_policy(
            vk,
            drm,
            gbm,
            width,
            height,
            plan,
            AllocationCleanupPolicy::BestEffort,
        )
        .map_err(DisposableProbeError::into_io_error)
    }

    pub(super) fn allocate_with_plan_for_disposable_probe(
        vk: Arc<VkContext>,
        drm: Rc<crate::drm::Device>,
        gbm: Option<Rc<GbmDevice>>,
        width: u32,
        height: u32,
        plan: ScanoutAllocationPlan,
    ) -> Result<Self, DisposableProbeError> {
        Self::allocate_with_plan_policy(
            vk,
            drm,
            gbm,
            width,
            height,
            plan,
            AllocationCleanupPolicy::StrictDisposable,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn allocate_with_plan_policy(
        vk: Arc<VkContext>,
        drm: Rc<crate::drm::Device>,
        gbm: Option<Rc<GbmDevice>>,
        width: u32,
        height: u32,
        plan: ScanoutAllocationPlan,
        cleanup_policy: AllocationCleanupPolicy,
    ) -> Result<Self, DisposableProbeError> {
        // 1. Allocate the source dma-buf + import into Vulkan
        //    (GBM plans) OR allocate the `VkImage` and export as
        //    dma-buf (Vulkan-alloc plans).
        let img = match plan {
            ScanoutAllocationPlan::GbmModifier(modifier) => {
                let gbm_device = gbm.as_ref().ok_or_else(|| {
                    DisposableProbeError::from(io::Error::other(
                        "gbm plan requested but pool has no gbm_device",
                    ))
                })?;
                allocate_gbm_scanout_image(&vk, gbm_device, width, height, modifier).map_err(
                    |error| match error {
                        GbmScanoutError::Vk(result) => DisposableProbeError::from(
                            scanout_vk_error("gbm scanout Vulkan import", result),
                        ),
                        error => DisposableProbeError::from(io::Error::other(format!(
                            "gbm scanout image: {error}"
                        ))),
                    },
                )?
            }
            _ => allocate_vk_scanout_image(&vk, width, height, plan).map_err(|result| {
                DisposableProbeError::from(scanout_vk_error(
                    "Vulkan scanout image allocation",
                    result,
                ))
            })?,
        };
        let VkScanoutImage {
            image,
            memory,
            dmabuf,
            pitch,
            offset,
            modifier,
            gbm_bo,
        } = img;

        // 2. PRIME_FD_TO_HANDLE on the DRM device. Same DRM fd the
        //    GBM device (if any) was created on, so this returns the
        //    existing GEM handle rather than creating a new one when
        //    the source is a gbm_bo — kernel refcounts the underlying
        //    dma-buf either way.
        let gem_handle = match drm.prime_fd_to_buffer(dmabuf.as_fd()) {
            Ok(h) => h,
            Err(e) => {
                destroy_scanout_image(&vk, image, memory);
                return Err(DisposableProbeError::from(io::Error::other(format!(
                    "drm prime_fd_to_buffer: {e}"
                ))));
            }
        };
        // The GEM handle holds its own reference; close the dma-buf
        // fd we no longer need.
        drop(dmabuf);

        let mut partial = PartialScanoutBoAllocation {
            vk,
            drm,
            image,
            memory,
            image_view: None,
            semaphore: None,
            transfer: None,
            framebuffer: None,
            gem: Some(gem_handle),
            gbm_bo,
        };

        // 3. add_fb2. Modifier-backed paths must pass the MODIFIERS
        // flag even for DRM_FORMAT_MOD_LINEAR; the legacy fallback
        // deliberately keeps the old untagged shape.
        let fb_handle = match partial.drm.add_planar_framebuffer(
            &VkScanoutFb {
                gem_handle,
                width,
                height,
                pitch,
                offset,
                modifier,
            },
            addfb_flags_for_modifier(modifier),
        ) {
            Ok(h) => h,
            Err(e) => {
                return Err(
                    partial.rollback(io::Error::other(format!("drm add_fb: {e}")), cleanup_policy)
                );
            }
        };
        partial.framebuffer = Some(fb_handle);

        // 4. Long-lived export semaphore.
        let vk_semaphore = match create_export_semaphore(&partial.vk) {
            Ok(s) => s,
            Err(result) => {
                return Err(partial.rollback(
                    scanout_vk_error("Vulkan scanout semaphore", result),
                    cleanup_policy,
                ));
            }
        };
        partial.semaphore = Some(vk_semaphore);

        // 5. Per-bo transfer resources (always present now —
        //    every bo has a live VkImage to upload into).
        let vk_transfer = match allocate_transfer_resources(&partial.vk, width, height) {
            Ok(t) => t,
            Err(result) => {
                return Err(partial.rollback(
                    scanout_vk_error("Vulkan scanout transfer resources", result),
                    cleanup_policy,
                ));
            }
        };
        partial.transfer = Some(vk_transfer);

        // 6. Color image view used by the 4.1.3.4 composite pass
        //    `vkCmdBeginRendering` as the color attachment.
        let view_info = vk::ImageViewCreateInfo::default()
            .image(partial.image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(vk::Format::B8G8R8A8_UNORM)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            );
        let vk_image_view = match unsafe { partial.vk.device.create_image_view(&view_info, None) } {
            Ok(v) => v,
            Err(result) => {
                return Err(partial.rollback(
                    scanout_vk_error("Vulkan scanout image view", result),
                    cleanup_policy,
                ));
            }
        };
        partial.image_view = Some(vk_image_view);

        let PartialScanoutBoAllocation {
            vk,
            drm,
            image,
            memory,
            image_view,
            semaphore,
            transfer,
            framebuffer,
            gem,
            gbm_bo,
        } = partial;

        Ok(Self {
            state: BoState::default(),
            width,
            height,
            is_alien: false,
            pitch,
            last_gpu_render_ns: None,
            vk_image: image,
            vk_memory: memory,
            vk_image_view: image_view.expect("completed allocation has an image view"),
            vk_semaphore: semaphore.expect("completed allocation has a semaphore"),
            export_semaphore_reuse: ExportSemaphoreReuseState::Reusable,
            fb_handle: framebuffer,
            gem_handle: gem,
            vk_transfer: transfer.expect("completed allocation has transfer resources"),
            drm,
            vk,
            disarmed: false,
            gbm_bo,
        })
    }

    /// Submit a real color-attachment clear through this BO on a disposable
    /// Vulkan context.
    ///
    /// Import/export and framebuffer creation alone cannot prove that a
    /// foreign allocation is renderable. The probe follows the first-frame
    /// layout path, gives this submitted batch one bounded fence wait, and
    /// leaves the image in `GENERAL` for scanout validation.
    pub(super) fn probe_renderer_access(
        &self,
        timeout_ns: u64,
    ) -> Result<(), DisposableProbeError> {
        let device = &self.vk.device;
        let command_buffer = self.vk_transfer.command_buffer;

        unsafe {
            device
                .reset_command_buffer(command_buffer, vk::CommandBufferResetFlags::empty())
                .map_err(|result| {
                    scanout_vk_error("reset disposable scanout probe command buffer", result)
                })?;
            let begin = vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            crate::vk_count!(begin_command_buffer);
            device
                .begin_command_buffer(command_buffer, &begin)
                .map_err(|result| {
                    scanout_vk_error("begin disposable scanout probe command buffer", result)
                })?;

            let to_color = [vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::TOP_OF_PIPE)
                .src_access_mask(vk::AccessFlags2::empty())
                .dst_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
                .dst_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .image(self.vk_image)
                .subresource_range(
                    vk::ImageSubresourceRange::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .level_count(1)
                        .layer_count(1),
                )];
            device.cmd_pipeline_barrier2(
                command_buffer,
                &vk::DependencyInfo::default().image_memory_barriers(&to_color),
            );

            let color_attachment = [vk::RenderingAttachmentInfo::default()
                .image_view(self.vk_image_view)
                .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .load_op(vk::AttachmentLoadOp::CLEAR)
                .store_op(vk::AttachmentStoreOp::STORE)
                .clear_value(vk::ClearValue {
                    color: vk::ClearColorValue {
                        float32: [0.0, 0.0, 0.0, 1.0],
                    },
                })];
            let rendering = vk::RenderingInfo::default()
                .render_area(vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: self.width,
                        height: self.height,
                    },
                })
                .layer_count(1)
                .color_attachments(&color_attachment);
            crate::vk_count!(cmd_begin_rendering);
            device.cmd_begin_rendering(command_buffer, &rendering);
            crate::vk_count!(cmd_end_rendering);
            device.cmd_end_rendering(command_buffer);

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
            device.cmd_pipeline_barrier2(
                command_buffer,
                &vk::DependencyInfo::default().image_memory_barriers(&to_scanout),
            );

            crate::vk_count!(end_command_buffer);
            device
                .end_command_buffer(command_buffer)
                .map_err(|result| {
                    scanout_vk_error("end disposable scanout probe command buffer", result)
                })?;

            let fence = device
                .create_fence(&vk::FenceCreateInfo::default(), None)
                .map_err(|result| {
                    scanout_vk_error("create disposable scanout probe fence", result)
                })?;
            let mut fence = ProbeFence::new(device, fence);
            let command_buffers =
                [vk::CommandBufferSubmitInfo::default().command_buffer(command_buffer)];
            let submits = [vk::SubmitInfo2::default().command_buffer_infos(&command_buffers)];
            crate::vk_count!(queue_submit2);
            crate::vk_count!(submit_other);
            if let Err(result) = crate::kms::vk::submit_stats::timed(
                crate::kms::vk::submit_stats::SubmitCause::Scanout,
                1,
                false,
                || device.queue_submit2(self.vk.graphics_queue, &submits, fence.handle()),
            ) {
                fence.abandon_pending();
                return Err(DisposableProbeError::quarantined(scanout_vk_error(
                    "submit disposable scanout rendering probe",
                    result,
                )));
            }

            wait_copy_free_probe_fence(&mut fence, timeout_ns)
        }
    }

    /// Export a SYNC_FD payload from this bo's signal semaphore. Call
    /// this after `vkQueueSubmit2` with `signalSemaphore = vk_semaphore`
    /// — it returns the freshly-payloaded fd to hand KMS as
    /// `IN_FENCE_FD`. `None` maps to the KMS `-1` no-fence sentinel.
    #[allow(dead_code)] // wired in by Task 2.5 (atomic-commit fence path).
    pub fn export_signaled_fd(&mut self) -> Result<Option<OwnedFd>, vk::Result> {
        self.export_semaphore_reuse.begin_post_submit_export();
        let ext = self.vk.semaphore_fd_ext()?.clone();
        let info = vk::SemaphoreGetFdInfoKHR::default()
            .semaphore(self.vk_semaphore)
            .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
        let raw_fd = unsafe { ext.get_semaphore_fd(&info)? };
        let completion =
            crate::kms::vk::optional_sync_fd_from_vk(raw_fd, "vkGetSemaphoreFdKHR(SYNC_FD)")?;
        self.export_semaphore_reuse.finish_successful_export();
        Ok(completion)
    }

    /// Replace a binary export semaphore whose submitted payload could not be
    /// exported. The caller must first prove the queue submission completed.
    pub(crate) fn rearm_export_semaphore_after_quiescence(&mut self) -> Result<(), vk::Result> {
        if !self.export_semaphore_reuse.needs_rearm() {
            return Ok(());
        }
        let replacement = create_export_semaphore(&self.vk)?;
        unsafe {
            self.vk.device.destroy_semaphore(self.vk_semaphore, None);
        }
        self.vk_semaphore = replacement;
        self.export_semaphore_reuse.finish_successful_export();
        Ok(())
    }

    /// Strictly remove helper-created KMS registrations while their backing is
    /// still alive. Handles are cleared only after the corresponding ioctl
    /// succeeds, so a caller can retain the complete object graph when cleanup
    /// fails instead of letting ordinary Drop free still-referenced backing.
    pub(super) fn release_disposable_drm_resources(&mut self) -> io::Result<()> {
        release_drm_handles_strict(
            &mut self.fb_handle,
            &mut self.gem_handle,
            |framebuffer| {
                self.drm.destroy_framebuffer(framebuffer).map_err(|error| {
                    scanout_io_context(
                        format!("destroy disposable framebuffer {framebuffer:?}"),
                        error,
                    )
                })
            },
            |gem| {
                self.drm.close_buffer(gem).map_err(|error| {
                    scanout_io_context(format!("close disposable GEM handle {gem:?}"), error)
                })
            },
        )
    }

    /// Mark this BO as "let process-exit clean up." Subsequent
    /// `Drop` is a no-op. Idempotent.
    /// **Only valid at final process exit** — see field doc.
    pub fn disarm(&mut self) {
        // `Drop::drop` returning early does not suppress automatic field
        // drops. A GBM BO is an owning RAII handle, so leaving it in the
        // field would free output-owned storage while KMS may still retain
        // the framebuffer. Leak it deliberately with the other raw handles.
        leak_owned_backing(&mut self.gbm_bo);
        self.disarmed = true;
    }
}

impl Drop for ScanoutBo {
    fn drop(&mut self) {
        if self.disarmed {
            // Disarmed by shutdown-failed-disable path; let DRM-fd
            // close (process exit) reap GEM/FB and VkDevice teardown
            // releases the userspace handles. We are DELIBERATELY
            // leaking: this Drop deliberately skips vkDestroyImage,
            // vkFreeMemory, destroy_framebuffer, close_buffer(gem),
            // etc. — because touching them while KMS may still hold
            // the FB produces the `atomic remove_fb failed with -22`
            // warning that strands Wayland host sessions.
            log::warn!(
                "ScanoutBo disarmed (atomic disable_output failed); \
                 leaking FB/GEM/Vk to be reaped by DRM-fd close"
            );
            return;
        }
        // Defensive fence-fd cleanup. If the bo was Submitted /
        // Pending / OnScreen / Retiring at drop time (mid-flight
        // shutdown, or modeset that didn't go through the explicit
        // drain path), close any held fence fds so they don't leak.
        // Kernel-side sync_file refs survive our fd close until the
        // DRM device closes — atomic flip will still complete or
        // fail safely on its own.
        let released = self.state.transition_to_free_after_modeset_reset();
        if let Some(fd) = released.in_fence {
            // SAFETY: fd was inserted by transition_to_submitted; we
            // are the unique owner.
            drop(unsafe { OwnedFd::from_raw_fd(fd) });
        }
        if let Some(fd) = released.release_fence {
            drop(unsafe { OwnedFd::from_raw_fd(fd) });
        }

        // DRM-side teardown next: framebuffer references the GEM
        // handle; both must be released before we free the underlying
        // memory the dma-buf was exported from.
        if let Some(fb) = self.fb_handle.take()
            && let Err(e) = self.drm.destroy_framebuffer(fb)
        {
            log::warn!("drm destroy_framebuffer failed: {e}");
        }
        if let Some(h) = self.gem_handle.take()
            && let Err(e) = self.drm.close_buffer(h)
        {
            log::warn!("drm close_buffer (gem) failed: {e}");
        }

        unsafe {
            // Transfer resources (staging mapping must release before
            // memory is freed; command pool releases its CB).
            let t = std::mem::replace(
                &mut self.vk_transfer,
                TransferResources {
                    command_pool: vk::CommandPool::null(),
                    command_buffer: vk::CommandBuffer::null(),
                    staging_buffer: vk::Buffer::null(),
                    staging_memory: vk::DeviceMemory::null(),
                    staging_mapped: std::ptr::NonNull::dangling(),
                    staging_size: 0,
                    timestamp_pool: vk::QueryPool::null(),
                    timestamps_written: false,
                },
            );
            if t.command_pool != vk::CommandPool::null() {
                self.vk.device.unmap_memory(t.staging_memory);
                self.vk.device.destroy_buffer(t.staging_buffer, None);
                crate::kms::vk::mem_accounting::free_memory(&self.vk.device, t.staging_memory);
                self.vk.device.destroy_command_pool(t.command_pool, None);
                if t.timestamp_pool != vk::QueryPool::null() {
                    self.vk.device.destroy_query_pool(t.timestamp_pool, None);
                }
            }

            // Image view before image, image before memory, then
            // semaphore.
            if self.vk_image_view != vk::ImageView::null() {
                self.vk.device.destroy_image_view(self.vk_image_view, None);
            }
            self.vk.device.destroy_image(self.vk_image, None);
            crate::kms::vk::mem_accounting::free_memory(&self.vk.device, self.vk_memory);
            if self.vk_semaphore != vk::Semaphore::null() {
                self.vk.device.destroy_semaphore(self.vk_semaphore, None);
            }
        }
    }
}

pub(super) fn close_modeset_released(released: ModesetReleased) {
    if let Some(fd) = released.in_fence {
        // SAFETY: the state machine returned unique ownership of this fd.
        drop(unsafe { OwnedFd::from_raw_fd(fd) });
    }
    if let Some(fd) = released.release_fence {
        // SAFETY: the state machine returned unique ownership of this fd.
        drop(unsafe { OwnedFd::from_raw_fd(fd) });
    }
}

pub(super) fn leak_owned_backing<T>(slot: &mut Option<T>) {
    if let Some(backing) = slot.take() {
        std::mem::forget(backing);
    }
}

// Compile-only check that `export_signaled_fd`'s call into
// `external_semaphore_fd::Device::get_semaphore_fd` keeps the same
// argument shape. If ash bumps and the signature changes, this
// function fails to compile and breaks the build before any
// integration test runs.
#[cfg(test)]
#[allow(dead_code)]
fn _compile_check_export_signature(
    ext: &ash::khr::external_semaphore_fd::Device,
    semaphore: vk::Semaphore,
) {
    let info = vk::SemaphoreGetFdInfoKHR::default()
        .semaphore(semaphore)
        .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
    let _: Result<i32, vk::Result> = unsafe { ext.get_semaphore_fd(&info) };
}
