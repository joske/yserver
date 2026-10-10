use super::*;

impl OutputScanout {
    /// Semantic renderer-to-KMS route presented by this output.
    #[must_use]
    pub(crate) fn route(&self) -> ScanoutRoute {
        match self {
            Self::Shared(pool) => pool.route,
            Self::Copied(pool) => pool.route,
        }
    }

    /// Pool whose framebuffer is installed on the KMS CRTC.
    #[must_use]
    pub(crate) fn display_pool(&self) -> &ScanoutBoPool {
        match self {
            Self::Shared(pool) => pool,
            Self::Copied(pool) => &pool.destinations,
        }
    }

    pub(crate) fn display_pool_mut(&mut self) -> &mut ScanoutBoPool {
        match self {
            Self::Shared(pool) => pool,
            Self::Copied(pool) => &mut pool.destinations,
        }
    }

    #[must_use]
    #[allow(dead_code)] // immutable diagnostics; scene currently needs copied_mut.
    pub(crate) fn copied(&self) -> Option<&CopiedScanoutPool> {
        match self {
            Self::Shared(_) => None,
            Self::Copied(pool) => Some(pool),
        }
    }

    pub(crate) fn copied_mut(&mut self) -> Option<&mut CopiedScanoutPool> {
        match self {
            Self::Shared(_) => None,
            Self::Copied(pool) => Some(pool),
        }
    }

    pub(crate) fn note_kms_modeset_installed(&mut self, bo_idx: usize) -> io::Result<()> {
        match self {
            Self::Shared(_) => Ok(()),
            Self::Copied(pool) => pool.note_kms_modeset_installed(bo_idx),
        }
    }

    /// Leak only the KMS-visible destination backing after a failed final
    /// disable. Renderer-side copied sources are not referenced by KMS.
    pub(crate) fn disarm(&mut self) {
        match self {
            Self::Shared(pool) => {
                for bo in &mut pool.bos {
                    bo.disarm();
                }
            }
            Self::Copied(pool) => pool.disarm_display_backing(),
        }
    }

    pub(crate) fn drain_all_pending(&mut self, render_vk: &VkContext) -> io::Result<()> {
        match self {
            Self::Shared(pool) => {
                pool.drain_all_pending(render_vk);
                Ok(())
            }
            Self::Copied(pool) => pool.drain_all_pending(),
        }
    }
}

impl ScanoutBoPool {
    pub(super) fn release_disposable_drm_resources(&mut self) -> io::Result<()> {
        for (index, bo) in self.bos.iter_mut().enumerate() {
            bo.release_disposable_drm_resources().map_err(|error| {
                scanout_io_context(format!("disposable scanout BO {index}"), error)
            })?;
        }
        Ok(())
    }

    pub(crate) fn finish_disposable_probe(
        self,
        result: Result<(), DisposableProbeError>,
    ) -> Result<(), DisposableProbeError> {
        finish_disposable_probe_attempt(self, result)
    }

    /// Register a client-imported `DrawableImage` as an alien BO in
    /// the pool. The DrawableImage's underlying `VkDeviceMemory` is
    /// already allocated; we run the same `add_fb2` framebuffer
    /// registration the pool's owned BOs use, with the imported
    /// memory's GEM handle plus its DRM modifier.
    ///
    /// Phase 4.2.4 first-cut: returns `Err` because the
    /// VkDeviceMemory → GEM handle bridge is non-trivial and lives
    /// behind the live KMS Flip integration. The wire surface is in
    /// place so the dispatcher's choose_path Flip / DirectScanout
    /// branches plumb correctly; live registration arrives with the
    /// vng + Venus smoke for §5.5 hardware coverage.
    pub fn register_alien(
        &mut self,
        _drawable: &crate::kms::vk::target::DrawableImage,
    ) -> io::Result<AlienBoHandle> {
        Err(io::Error::other(
            "ScanoutBoPool::register_alien: live KMS Flip integration not yet wired \
             (Phase 4.2.4 design §5.5 hardware coverage smoke)",
        ))
    }

    /// Drop a previously registered alien BO. Releases the framebuffer
    /// registration and removes the entry from `bos`. No-op if the
    /// handle's index is out of range.
    #[allow(dead_code)]
    pub fn unregister_alien(&mut self, _handle: AlienBoHandle) -> io::Result<()> {
        // Counterpart to register_alien — unimplemented for the same
        // reason. The plan's Task 29 test covers the round-trip once
        // both halves land.
        Ok(())
    }

    /// Reset every bo in the pool to `Free`, draining any in-flight
    /// fence fds. Used by the modeset / hot-config path (resize, mode
    /// change, hotplug — design §2 "Modeset / hot-config events").
    ///
    /// Order of operations:
    ///
    /// 1. `vkDeviceWaitIdle` on the device — heavy hammer that waits
    ///    for any in-flight `vkQueueSubmit2` work to complete. Cheap
    ///    in steady state (no work) and conservatively correct for
    ///    `Submitted`-phase bos which would otherwise have GPU work
    ///    racing the DRM tear-down.
    /// 2. For each bo, advance state machine to `Free` via
    ///    `transition_to_free_after_modeset_reset` and close any
    ///    returned fence fds.
    ///
    /// Pool dimensions stay the same; this is "reset state machine,
    /// keep the bos." Re-allocating bos with new dimensions is the
    /// caller's responsibility (drop the pool, allocate a fresh one
    /// with `ScanoutBoPool::allocate`).
    // Consumers: Drop, and `PlatformBackend::reset_scanout_bos_for_suspend`
    // (VT-switch suspend reclaims orphaned scanout BOs after master loss).
    pub fn drain_all_pending(&mut self, vk: &VkContext) {
        if let Err(e) = unsafe { vk.device.device_wait_idle() } {
            log::warn!("scanout pool drain: vkDeviceWaitIdle: {e}");
        }
        for bo in &mut self.bos {
            let released = bo.state.transition_to_free_after_modeset_reset();
            if let Some(fd) = released.in_fence {
                // SAFETY: fd inserted by transition_to_submitted; unique owner.
                drop(unsafe { OwnedFd::from_raw_fd(fd) });
            }
            if let Some(fd) = released.release_fence {
                drop(unsafe { OwnedFd::from_raw_fd(fd) });
            }
        }
    }

    /// True if any bo in this pool is in `BoPhase::Pending` —
    /// i.e. an atomic flip was accepted by KMS and the kernel
    /// hasn't yet emitted its pageflip-complete event for that
    /// flip. Used by the shutdown sequence to wait until KMS
    /// quiesces before issuing `disable_output`. Calling
    /// `disable_output` while a Pending bo exists is what
    /// produces the `atomic remove_fb failed with -22` kernel
    /// warning that leaves Wayland host compositors stranded.
    pub fn has_pending_pageflip(&self) -> bool {
        self.bos.iter().any(|b| b.state.phase == BoPhase::Pending)
    }

    /// Prove that every BO in this exact pool can complete a real Vulkan
    /// color-attachment write. Callers use only disposable contexts here, so
    /// a rejected foreign-memory submission cannot poison the live renderer.
    pub(crate) fn probe_renderer_access(self, timeout_ns: u64) -> Result<(), DisposableProbeError> {
        let result = (|| {
            for (index, bo) in self.bos.iter().enumerate() {
                let probe_started = Instant::now();
                bo.probe_renderer_access(timeout_ns).map_err(|error| {
                    error.with_context(format!("BO {index} disposable renderer-access probe"))
                })?;
                log::info!(
                    "copy-free rendering probe: bo={index} completed in {} ms; fence timeout {:?}",
                    probe_started.elapsed().as_millis(),
                    Duration::from_nanos(timeout_ns),
                );
            }
            Ok(())
        })();
        if result
            .as_ref()
            .is_err_and(DisposableProbeError::bypass_normal_teardown)
        {
            log::error!(
                "copy-free rendering probe timed out or left GPU completion uncertain; retaining \
                 the disposable pool without vkDeviceWaitIdle"
            );
        }
        finish_disposable_probe_attempt(self, result)
    }

    /// Allocate `count` bos for one output. Phase 4.1.2 uses 3 bos
    /// per pool (design §2). Opens the per-pool `gbm_device` on the
    /// KMS DRM fd so BOs can go through the GBM-first path. GBM
    /// device open failure is non-fatal — bos fall back to the
    /// Vulkan-first legacy allocator. On BO allocation failure the
    /// partial pool is dropped (each successful bo cleans up via
    /// `ScanoutBo::Drop`).
    pub(crate) fn allocate(
        vk: Arc<VkContext>,
        drm: Rc<crate::drm::Device>,
        route: ScanoutRoute,
        width: u32,
        height: u32,
        count: usize,
        scanout_modifiers: &[u64],
    ) -> io::Result<Self> {
        let metadata = probe_dmabuf_scanout_metadata(&vk, &drm, route, scanout_modifiers);
        let gbm_device = open_scanout_gbm_device(&drm);
        let output_owned_gbm = if gbm_device.is_some() {
            ScanoutMetadataSupport::Supported
        } else {
            ScanoutMetadataSupport::Unknown
        };
        let verdict = classify_dmabuf_scanout_route(route, &metadata, output_owned_gbm);
        log::info!(
            "dma-buf scanout observation for {route:?}: gbm={output_owned_gbm:?} \
             verdict={verdict:?} (diagnostic only)"
        );

        let plans =
            exact_scanout_allocation_plans(&vk, width, scanout_modifiers, gbm_device.is_some());
        let mut errors = Vec::new();
        for plan in plans {
            match Self::allocate_exact_observed(
                Arc::clone(&vk),
                Rc::clone(&drm),
                gbm_device.as_ref().map(Rc::clone),
                route,
                width,
                height,
                count,
                metadata.clone(),
                verdict.clone(),
                plan,
            ) {
                Ok(pool) => return Ok(pool),
                Err(error) if scanout_error_is_device_lost(&error) => return Err(error),
                Err(error) => {
                    log::info!("scanout pool: exact {} failed: {error}", plan.describe());
                    errors.push(format!("{}: {error}", plan.describe()));
                }
            }
        }

        Err(io::Error::other(format!(
            "scanout allocation failed for every exact full-pool plan: {}",
            errors.join("; ")
        )))
    }

    /// Enumerate exact full-pool candidates in the allocator's established
    /// total order. Output-owned candidates require Vulkan IMPORTABLE DMA-BUF
    /// modifiers; renderer-owned candidates require EXPORTABLE modifiers.
    #[must_use]
    pub(crate) fn exact_allocation_plans(
        vk: &VkContext,
        drm: &Rc<crate::drm::Device>,
        width: u32,
        scanout_modifiers: &[u64],
    ) -> Vec<ScanoutAllocationPlan> {
        let gbm_available = open_scanout_gbm_device(drm).is_some();
        exact_scanout_allocation_plans(vk, width, scanout_modifiers, gbm_available)
    }

    /// Allocate every BO with one exact representation. No BO may fall
    /// through to a different plan, so `ownership` and `allocation_plan`
    /// remain truthful for the complete pool.
    pub(crate) fn allocate_exact(
        vk: Arc<VkContext>,
        drm: Rc<crate::drm::Device>,
        route: ScanoutRoute,
        width: u32,
        height: u32,
        count: usize,
        scanout_modifiers: &[u64],
        plan: ScanoutAllocationPlan,
    ) -> io::Result<Self> {
        let metadata = probe_dmabuf_scanout_metadata(&vk, &drm, route, scanout_modifiers);
        let gbm_device = open_scanout_gbm_device(&drm);
        let output_owned_gbm = if gbm_device.is_some() {
            ScanoutMetadataSupport::Supported
        } else {
            ScanoutMetadataSupport::Unknown
        };
        let verdict = classify_dmabuf_scanout_route(route, &metadata, output_owned_gbm);
        log::info!(
            "dma-buf exact scanout observation for {route:?}: plan={} \
             gbm={output_owned_gbm:?} verdict={verdict:?} (diagnostic only)",
            plan.describe(),
        );
        Self::allocate_exact_observed(
            vk, drm, gbm_device, route, width, height, count, metadata, verdict, plan,
        )
    }

    /// Helper-only exact allocation. Any partially-created KMS registration is
    /// rolled back strictly; a cleanup failure is terminal and retains the
    /// backing rather than returning through live best-effort Drop.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn allocate_exact_for_disposable_probe(
        vk: Arc<VkContext>,
        drm: Rc<crate::drm::Device>,
        route: ScanoutRoute,
        width: u32,
        height: u32,
        count: usize,
        scanout_modifiers: &[u64],
        plan: ScanoutAllocationPlan,
    ) -> Result<Self, DisposableProbeError> {
        let metadata = probe_dmabuf_scanout_metadata(&vk, &drm, route, scanout_modifiers);
        let gbm_device = open_scanout_gbm_device(&drm);
        let output_owned_gbm = if gbm_device.is_some() {
            ScanoutMetadataSupport::Supported
        } else {
            ScanoutMetadataSupport::Unknown
        };
        let verdict = classify_dmabuf_scanout_route(route, &metadata, output_owned_gbm);
        log::info!(
            "disposable dma-buf exact scanout observation for {route:?}: plan={} \
             gbm={output_owned_gbm:?} verdict={verdict:?} (diagnostic only)",
            plan.describe(),
        );
        Self::allocate_exact_observed_with_policy(
            vk,
            drm,
            gbm_device,
            route,
            width,
            height,
            count,
            metadata,
            verdict,
            plan,
            AllocationCleanupPolicy::StrictDisposable,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn allocate_exact_observed(
        vk: Arc<VkContext>,
        drm: Rc<crate::drm::Device>,
        gbm_device: Option<Rc<GbmDevice>>,
        route: ScanoutRoute,
        width: u32,
        height: u32,
        count: usize,
        metadata: DmabufScanoutMetadata,
        verdict: DmabufScanoutVerdict,
        plan: ScanoutAllocationPlan,
    ) -> io::Result<Self> {
        Self::allocate_exact_observed_with_policy(
            vk,
            drm,
            gbm_device,
            route,
            width,
            height,
            count,
            metadata,
            verdict,
            plan,
            AllocationCleanupPolicy::BestEffort,
        )
        .map_err(DisposableProbeError::into_io_error)
    }

    #[allow(clippy::too_many_arguments)]
    fn allocate_exact_observed_with_policy(
        vk: Arc<VkContext>,
        drm: Rc<crate::drm::Device>,
        gbm_device: Option<Rc<GbmDevice>>,
        route: ScanoutRoute,
        width: u32,
        height: u32,
        count: usize,
        metadata: DmabufScanoutMetadata,
        verdict: DmabufScanoutVerdict,
        plan: ScanoutAllocationPlan,
        cleanup_policy: AllocationCleanupPolicy,
    ) -> Result<Self, DisposableProbeError> {
        if plan.ownership() == ScanoutOwnership::Output && gbm_device.is_none() {
            return Err(DisposableProbeError::from(io::Error::other(format!(
                "exact {} requires a GBM device on the KMS fd",
                plan.describe()
            ))));
        }

        let mut bos = Vec::with_capacity(count);
        for index in 0..count {
            let allocation = match cleanup_policy {
                AllocationCleanupPolicy::BestEffort => ScanoutBo::allocate_with_plan(
                    Arc::clone(&vk),
                    Rc::clone(&drm),
                    gbm_device.as_ref().map(Rc::clone),
                    width,
                    height,
                    plan,
                )
                .map_err(DisposableProbeError::from),
                AllocationCleanupPolicy::StrictDisposable => {
                    ScanoutBo::allocate_with_plan_for_disposable_probe(
                        Arc::clone(&vk),
                        Rc::clone(&drm),
                        gbm_device.as_ref().map(Rc::clone),
                        width,
                        height,
                        plan,
                    )
                }
            }
            .map_err(|error| {
                error.with_context(format!("exact {} BO {index} allocation", plan.describe()))
            });
            match allocation {
                Ok(bo) => bos.push(bo),
                Err(error)
                    if matches!(cleanup_policy, AllocationCleanupPolicy::StrictDisposable)
                        && !bos.is_empty() =>
                {
                    let partial_pool = Self {
                        bos,
                        width,
                        height,
                        route,
                        ownership: plan.ownership(),
                        allocation_plan: plan,
                        metadata,
                        verdict,
                        gbm_device,
                    };
                    return Err(partial_pool
                        .finish_disposable_probe(Err(error))
                        .expect_err("failed allocation cannot become a successful probe"));
                }
                Err(error) => return Err(error),
            }
        }
        Ok(Self {
            bos,
            width,
            height,
            route,
            ownership: plan.ownership(),
            allocation_plan: plan,
            metadata,
            verdict,
            gbm_device,
        })
    }
}

fn open_scanout_gbm_device(drm: &Rc<crate::drm::Device>) -> Option<Rc<GbmDevice>> {
    match GbmDevice::new(Rc::clone(drm)) {
        Ok(device) => Some(Rc::new(device)),
        Err(error) => {
            log::warn!(
                "gbm_create_device failed on KMS fd ({error}); scanout allocation will \
                 fall back to Vulkan-alloc, where NVIDIA/Intel take LINEAR \
                 (see scanout_prefers_linear) because Vulkan-allocated tiled \
                 scanout garbles there"
            );
            None
        }
    }
}

fn exact_scanout_allocation_plans(
    vk: &VkContext,
    width: u32,
    scanout_modifiers: &[u64],
    gbm_available: bool,
) -> Vec<ScanoutAllocationPlan> {
    let output_owned_modifiers =
        scanout_modifier_candidates(vk, scanout_modifiers, ScanoutOwnership::Output);
    let renderer_owned_modifiers =
        scanout_modifier_candidates(vk, scanout_modifiers, ScanoutOwnership::Renderer);
    scanout_allocation_plans(
        vk,
        &output_owned_modifiers,
        &renderer_owned_modifiers,
        width,
        gbm_available,
    )
}
