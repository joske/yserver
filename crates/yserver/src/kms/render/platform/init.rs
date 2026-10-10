use super::*;

fn drm_device_is_nvidia(device: &drm::Device) -> bool {
    use ::drm::Device as _;
    device
        .get_driver()
        .ok()
        .map(|driver| {
            driver
                .name()
                .to_string_lossy()
                .to_ascii_lowercase()
                .contains("nvidia")
        })
        .unwrap_or(false)
}

fn retain_live_vk_contexts_after_terminal_probe(
    live_vk: &Arc<VkContext>,
    copied_contexts: &HashMap<RenderDeviceId, Arc<VkContext>>,
) {
    // PlatformBackend construction returns an error after a terminal probe.
    // Retain one Arc for every live logical device so unwinding the partially
    // constructed backend cannot re-enter VkContext::drop's device-wide idle
    // on the same physical GPU that just timed out.
    std::mem::forget(Arc::clone(live_vk));
    for context in copied_contexts.values() {
        std::mem::forget(Arc::clone(context));
    }
}

pub(super) fn retain_initialized_scanout_pools<T>(pools: &mut [Option<T>]) {
    for pool in pools.iter_mut().filter_map(Option::take) {
        std::mem::forget(pool);
    }
}

pub(super) fn retain_startup_gpu_owners<A, B, C>(ops: A, fences: B, pixmaps: C) {
    std::mem::forget(pixmaps);
    std::mem::forget(fences);
    std::mem::forget(ops);
}

impl RollbackScanoutOutput for PlatformInitOutput {
    fn output_key(&self) -> &OutputKey {
        &self.key
    }

    fn drm_output(&self) -> &crate::platform::drm::Output {
        &self.output
    }

    fn disarm_swapchain(&mut self) {
        self.swapchain.disarm();
    }
}

impl RollbackScanoutOutput for ActiveOutput {
    fn output_key(&self) -> &OutputKey {
        &self.key
    }

    fn drm_output(&self) -> &crate::platform::drm::Output {
        &self.output
    }

    fn disarm_swapchain(&mut self) {
        self.swapchain.disarm();
    }
}

fn rollback_initial_scanout_with<O, F>(devices: &[KmsDevice], outputs: &mut [O], disable: &mut F)
where
    O: RollbackScanoutOutput,
    F: FnMut(&drm::Device, &crate::platform::drm::Output) -> io::Result<()>,
{
    for layout in outputs.iter_mut().rev() {
        let device_key = layout.output_key().device_key;
        let connector_name = layout.drm_output().connector_name.clone();
        let Some(device) = devices.iter().find(|device| device.key == device_key) else {
            log::error!(
                "initial scanout rollback: no DRM device {} for {}; \
                 leaving its buffers for DRM-fd close",
                device_key,
                connector_name,
            );
            layout.disarm_swapchain();
            continue;
        };
        if let Err(err) = disable(&device.device, layout.drm_output()) {
            log::warn!(
                "initial scanout rollback: failed to disable {} on {}: {err}; \
                 leaving its buffers for DRM-fd close",
                connector_name,
                device.key,
            );
            layout.disarm_swapchain();
        }
    }
}

impl<'a, O, F> InitialScanoutRollbackGuard<'a, O, F>
where
    O: RollbackScanoutOutput,
    F: FnMut(&drm::Device, &crate::platform::drm::Output) -> io::Result<()>,
{
    pub(super) fn new_with(devices: &'a [KmsDevice], outputs: &'a mut [O], disable: F) -> Self {
        Self {
            devices,
            armed: !outputs.is_empty(),
            outputs,
            disable,
        }
    }

    fn devices(&self) -> &[KmsDevice] {
        self.devices
    }

    fn outputs(&self) -> &[O] {
        self.outputs
    }

    fn is_armed(&self) -> bool {
        self.armed
    }

    pub(super) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl<O, F> Drop for InitialScanoutRollbackGuard<'_, O, F>
where
    O: RollbackScanoutOutput,
    F: FnMut(&drm::Device, &crate::platform::drm::Output) -> io::Result<()>,
{
    fn drop(&mut self) {
        if self.armed {
            rollback_initial_scanout_with(self.devices, self.outputs, &mut self.disable);
            self.armed = false;
        }
    }
}

fn build_render_device_inventory(
    vk: &Arc<VkContext>,
    render_node: Option<crate::kms::render_node::OpenedRenderNode>,
) -> io::Result<(Vec<RenderDevice>, RenderDeviceId)> {
    let mut render_devices: Vec<_> = vk
        .drm_physical_devices
        .iter()
        .map(|entry| RenderDevice {
            id: RenderDeviceId::DrmRender(
                entry
                    .identity
                    .render
                    .expect("VkContext inventory contains only render-identified devices"),
            ),
            physical_device: entry.physical_device,
            selector: entry.selector,
            advertised_primary_node: entry.identity.primary,
            advertised_render_node: entry.identity.render,
            render_node: None,
            render_node_device: None,
            syncobj_timeline: false,
        })
        .collect();

    let selected_index = render_devices
        .iter()
        .position(|device| device.physical_device == vk.physical_device)
        .unwrap_or_else(|| {
            let identity = vk.selected_drm_identity;
            render_devices.push(RenderDevice {
                id: RenderDeviceId::UnverifiedFallback,
                physical_device: vk.physical_device,
                selector: vk.device_selector(),
                advertised_primary_node: identity.and_then(|identity| identity.primary),
                advertised_render_node: identity.and_then(|identity| identity.render),
                render_node: None,
                render_node_device: None,
                syncobj_timeline: false,
            });
            render_devices.len() - 1
        });

    let selected = &mut render_devices[selected_index];
    if let Some(render_node) = render_node {
        validate_render_node_attachment(selected.id, render_node.key())?;
        let render_node_device = drm::Device::open_render_node(
            render_node
                .path()
                .to_str()
                .unwrap_or("<non-UTF-8 render-node path>"),
        )
        .and_then(|device| {
            render_node.verify_fd(device.as_fd())?;
            Ok(Arc::new(device))
        })
        .map_err(|error| {
            log::warn!(
                "render device: failed to reopen selected DRM render node {}: {error}; syncobj support unavailable",
                render_node.path().display(),
            );
        })
        .ok();
        let syncobj_timeline = render_node_device
            .as_ref()
            .and_then(|device| {
                use ::drm::Device as _;
                device
                    .get_driver_capability(::drm::DriverCapability::TimelineSyncObj)
                    .ok()
            })
            .is_some_and(|value| value != 0);
        selected.render_node = Some(render_node);
        selected.render_node_device = render_node_device;
        selected.syncobj_timeline = syncobj_timeline;
    }

    let selected_id = render_devices[selected_index].id;
    Ok((render_devices, selected_id))
}

pub(super) fn resolve_copied_sink_renderer(
    render_devices: &[RenderDevice],
    selected: RenderDeviceId,
    kms_key: crate::platform::drm::DrmDeviceKey,
) -> io::Result<(RenderDeviceId, VulkanDeviceSelector)> {
    let matches = render_devices
        .iter()
        .filter(|renderer| renderer.id != selected)
        .filter(|renderer| renderer.advertised_primary_node == Some(kms_key))
        .map(|renderer| (renderer.id, renderer.selector))
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [matched] => Ok(*matched),
        [] => Err(io::Error::other(format!(
            "copied scanout KMS device {kms_key} has no distinct Vulkan renderer with matching advertised primary identity"
        ))),
        _ => Err(io::Error::other(format!(
            "copied scanout KMS device {kms_key} matches multiple Vulkan renderers: {:?}",
            matches.iter().map(|(id, _)| id).collect::<Vec<_>>()
        ))),
    }
}

pub(super) fn validate_render_node_attachment(
    selected: RenderDeviceId,
    opened: crate::platform::drm::DrmDeviceKey,
) -> io::Result<()> {
    match selected {
        RenderDeviceId::DrmRender(advertised) if advertised != opened => {
            Err(io::Error::other(format!(
                "selected Vulkan renderer advertises DRM render node {advertised}, but the opened render endpoint is {opened}"
            )))
        }
        RenderDeviceId::DrmRender(_) | RenderDeviceId::UnverifiedFallback => Ok(()),
    }
}

impl Drop for PlatformBackend {
    fn drop(&mut self) {
        if !self.initial_scanout_rollback_armed {
            return;
        }
        rollback_initial_scanout_with(
            &self.devices,
            &mut self.outputs,
            &mut drm::modeset::disable_output,
        );
        self.initial_scanout_rollback_armed = false;
    }
}

impl PlatformBackend {
    /// Mark the initial modesets as owned by a fully constructed backend.
    /// Normal shutdown will disable them through `KmsBackend::disable_output`;
    /// construction failures leave this armed so `Drop` performs rollback.
    pub(crate) fn disarm_initial_scanout_rollback(&mut self) {
        self.initial_scanout_rollback_armed = false;
    }

    /// Backend constructor. Opens DRM, initialises Vk,
    /// allocates per-output scanout pools, builds the fence pool.
    /// Fatal initialization failures tear down already-allocated resources
    /// and return `Err`.
    ///
    /// # Errors
    ///
    /// Propagates platform-init failures from `core_platform_init`,
    /// Vk init failures from `VkContext::new`, command-pool allocation
    /// failures from `OpsCommandPool::new`. An individual `ScanoutBoPool`
    /// failure is non-fatal while another output remains displayable; startup
    /// fails if every connected output lacks a live pool.
    pub(crate) fn open_with_commit(
        device_paths: &[PathBuf],
        commit: fn(
            &drm::Device,
            &crate::platform::drm::Output,
            ::drm::control::framebuffer::Handle,
        ) -> io::Result<()>,
    ) -> io::Result<Self> {
        // `core_platform_init` runs the hardware-Vulkan preflight after it
        // discovers an active output but before allocating or committing
        // scanout. Headless platforms skip it and may use software Vulkan.
        let platform_init = core_platform_init(device_paths, commit)?;
        Self::from_platform_init(platform_init)
    }

    /// Shared bring-up body: Vk + pools + epoll + cursor plane init
    /// from a pre-built [`PlatformInit`]. Called by
    /// [`open_with_commit`] (Direct mode — the only mode).
    fn from_platform_init(platform_init: PlatformInit) -> io::Result<Self> {
        let PlatformInit {
            devices,
            render_node,
            mut layouts,
            fb_w,
            fb_h,
            input_ctx,
        } = platform_init;

        let mut devices: Vec<KmsDevice> = devices
            .into_iter()
            .map(|device| {
                let cursor =
                    KmsCursorState::new_with_nvidia_policy(drm_device_is_nvidia(&device.device));
                KmsDevice {
                    key: device.key,
                    device: device.device,
                    cursor,
                }
            })
            .collect();

        // One independently-owned cursor buffer/state per DRM device. A
        // device with no active startup CRTC stays explicitly deferred until
        // its first successful RANDR enable inserts an ActiveOutput.
        for kms_device in &mut devices {
            let crtcs: Vec<_> = layouts
                .iter()
                .filter(|layout| layout.key.device_key == kms_device.key)
                .map(|layout| layout.output.crtc)
                .collect();
            if crtcs.is_empty() {
                log::info!(
                    "render cursor: device {} has no active startup CRTC; initialization deferred",
                    kms_device.key
                );
                continue;
            }
            initialize_cursor_plane_for_device(kms_device, &crtcs, "active startup");
        }

        let mut initial_scanout_rollback = InitialScanoutRollbackGuard::new_with(
            &devices,
            &mut layouts,
            drm::modeset::disable_output,
        );

        let requested_render_node = render_node.as_ref().map(|node| node.key());
        let vk_result = devices.first().map_or_else(VkContext::new, |display| {
            VkContext::new_for_render_device(requested_render_node, display.key)
        });
        let vk = match vk_result {
            Ok(v) => v,
            Err(e) => {
                return Err(io::Error::other(format!(
                    "render PlatformBackend: VkContext init failed (render backend requires Vulkan; \
                     no pixman fallback): {e}"
                )));
            }
        };
        log::info!(
            "render PlatformBackend: VkContext ready (driver_id={:?}, device_type={:?})",
            vk.driver_id,
            vk.device_type,
        );
        let (render_devices, selected_render_device) =
            build_render_device_inventory(&vk, render_node)?;
        let selected_renderer = render_devices
            .iter()
            .find(|device| device.id == selected_render_device)
            .expect("selected renderer is present in renderer inventory");
        if let Some(display) = devices.first() {
            let kms_relationship = match selected_renderer.relationship_to(display) {
                RenderKmsRelationship::Same => "same-device",
                RenderKmsRelationship::Different => "different-device",
                RenderKmsRelationship::Unknown => "unknown",
            };
            log::info!(
                "render PlatformBackend: selected renderer {:?} (render={:?}, primary={:?}) has {kms_relationship} relationship to KMS device {}",
                selected_renderer.physical_device,
                selected_renderer.advertised_render_node,
                selected_renderer.advertised_primary_node,
                display.key,
            );
        }

        // Refuse to drive real KMS scanout off a software rasterizer.
        // If the only Vulkan device is llvmpipe/lavapipe (CPU type) —
        // typically because the GPU's hardware Vulkan driver is missing
        // (e.g. nvidia removed but nouveau not loaded, so Mesa falls back
        // to llvmpipe) — then exporting a host-memory buffer and handing
        // it to a real GPU's atomic scanout commit HARD-HANGS the machine
        // (observed on nouveau/Pascal: no SSH, nothing in the journal).
        // Fail fast with an actionable error instead of wedging the box.
        // Venus (virtio-gpu) reports VIRTUAL_GPU, not CPU, so it is not
        // affected; the env override exists for any deliberate
        // software-scanout setup (e.g. lavapipe under vng).
        if !initial_scanout_rollback.outputs().is_empty()
            && vk.is_software_rasterizer()
            && std::env::var_os("YSERVER_ALLOW_SOFTWARE_VULKAN").is_none()
        {
            return Err(io::Error::other(format!(
                "render PlatformBackend: the only Vulkan device is a software rasterizer \
                 (device_type=CPU, driver_id={:?} — llvmpipe/lavapipe). Driving real KMS \
                 scanout off software Vulkan hard-hangs the machine on hardware that can't \
                 scan out a host-memory buffer. Refusing to start. Install a hardware Vulkan \
                 driver for the scanout GPU (radv / anv / nvk), or check your GPU/driver setup \
                 (e.g. nvidia removed but nouveau not loaded → Mesa falls back to llvmpipe). \
                 To override (e.g. virtio-gpu under vng), set YSERVER_ALLOW_SOFTWARE_VULKAN=1.",
                vk.driver_id,
            )));
        }
        if initial_scanout_rollback.outputs().is_empty() && vk.is_software_rasterizer() {
            log::info!(
                "render PlatformBackend: using software Vulkan for headless rendering; no KMS outputs are active"
            );
        }

        let ops_command_pool = OpsCommandPool::new(Arc::clone(&vk))
            .map_err(|e| io::Error::other(format!("ops command pool: {e:?}")))?;

        let fence_pool = FencePool::new(Arc::clone(&vk));

        // Stage 3f.10: pixmap pool reuses v1's allocator verbatim.
        // MATE / xfce4 / GTK widgets churn ~90 pixmap allocs/sec;
        // without this every CreatePixmap pays a full
        // create_image + allocate_memory + bind + create_view cycle.
        // Registers with the GLOBAL_LATEST_POOL hook so the main-
        // loop telemetry path can sample hit/miss counters even
        // though v2 doesn't own the telemetry-emit cadence directly.
        let pixmap_pool = {
            let p = Arc::new(crate::kms::vk::pixmap_pool::PixmapPool::new(Arc::clone(
                &vk,
            )));
            crate::kms::vk::pixmap_pool::register_for_telemetry(&p);
            Some(p)
        };

        // One ScanoutBoPool per output, 3-BO depth (matches v1).
        let mut scanout_pools = Vec::with_capacity(initial_scanout_rollback.outputs().len());
        let mut bo_generations = Vec::with_capacity(initial_scanout_rollback.outputs().len());
        let mut scanout_routes = Vec::with_capacity(initial_scanout_rollback.outputs().len());
        let mut scanout_alloc_errors: Vec<String> = Vec::new();
        let mut copy_vk_contexts: HashMap<RenderDeviceId, Arc<VkContext>> = HashMap::new();
        for (i, layout) in initial_scanout_rollback.outputs().iter().enumerate() {
            let w = u32::from(layout.width);
            let h = u32::from(layout.height);
            let device = initial_scanout_rollback
                .devices()
                .iter()
                .find(|device| device.key == layout.key.device_key)
                .ok_or_else(|| {
                    io::Error::other(format!(
                        "render PlatformBackend: output {} belongs to missing DRM device {}",
                        layout.key.connector_name, layout.key.device_key
                    ))
                })?;
            let scanout_route = selected_renderer.scanout_route_to(device);
            scanout_routes.push(scanout_route);
            let allocation: io::Result<OutputScanout> = if route_requires_copy_free_probe(
                scanout_route,
            ) {
                match allocate_copy_free_scanout_pool(
                    Arc::clone(&vk),
                    Rc::clone(&device.device),
                    &layout.output,
                    scanout_route,
                    w,
                    h,
                    &layout.output.scanout_modifiers,
                    false,
                ) {
                    Ok(prepared) => {
                        debug_assert!(prepared.committed_framebuffer.is_none());
                        Ok(OutputScanout::Shared(prepared.pool))
                    }
                    Err(CopyFreeScanoutError::TerminalDisposableProbe(error)) => {
                        retain_initialized_scanout_pools(&mut scanout_pools);
                        retain_live_vk_contexts_after_terminal_probe(&vk, &copy_vk_contexts);
                        retain_startup_gpu_owners(ops_command_pool, fence_pool, pixmap_pool);
                        return Err(io::Error::new(
                            error.kind(),
                            format!("render PlatformBackend: {error}"),
                        ));
                    }
                    Err(error @ CopyFreeScanoutError::LiveRendererLost(_)) => {
                        return Err(io::Error::other(format!("render PlatformBackend: {error}")));
                    }
                    Err(CopyFreeScanoutError::Candidates(shared_error)) => {
                        let copied_result = (|| {
                            let (sink_id, sink_selector) = resolve_copied_sink_renderer(
                                &render_devices,
                                selected_render_device,
                                device.key,
                            )
                            .map_err(CopiedScanoutError::Candidates)?;
                            let sink_vk = if let Some(vk) = copy_vk_contexts.get(&sink_id) {
                                Arc::clone(vk)
                            } else {
                                let sink_vk = VkContext::new_transfer_for_device(sink_selector)
                                    .map_err(|error| {
                                        CopiedScanoutError::Candidates(io::Error::other(format!(
                                            "copied sink Vulkan context for {sink_id:?}/{}: \
                                             {error}",
                                            device.key,
                                        )))
                                    })?;
                                copy_vk_contexts.insert(sink_id, Arc::clone(&sink_vk));
                                sink_vk
                            };
                            let destination_route =
                                ScanoutRoute::new(sink_id, device.key, RenderKmsRelationship::Same);
                            allocate_copied_scanout_pool(
                                Arc::clone(&vk),
                                sink_vk,
                                Rc::clone(&device.device),
                                &layout.output,
                                scanout_route,
                                destination_route,
                                w,
                                h,
                                &layout.output.scanout_modifiers,
                                false,
                            )
                        })();
                        match copied_result {
                            Ok(prepared) => {
                                debug_assert!(prepared.committed_framebuffer.is_none());
                                Ok(OutputScanout::Copied(prepared.pool))
                            }
                            Err(CopiedScanoutError::TerminalDisposableProbe(error)) => {
                                retain_initialized_scanout_pools(&mut scanout_pools);
                                retain_live_vk_contexts_after_terminal_probe(
                                    &vk,
                                    &copy_vk_contexts,
                                );
                                retain_startup_gpu_owners(
                                    ops_command_pool,
                                    fence_pool,
                                    pixmap_pool,
                                );
                                return Err(io::Error::new(
                                    error.kind(),
                                    format!("render PlatformBackend: {error}"),
                                ));
                            }
                            Err(error @ CopiedScanoutError::LiveDeviceLost { .. }) => {
                                return Err(io::Error::other(format!(
                                    "render PlatformBackend: {error}"
                                )));
                            }
                            Err(CopiedScanoutError::Candidates(copied_error))
                                if crate::kms::vk::scanout::scanout_error_is_device_lost(
                                    &copied_error,
                                ) =>
                            {
                                return Err(io::Error::other(format!(
                                    "render PlatformBackend: {copied_error}"
                                )));
                            }
                            Err(CopiedScanoutError::Candidates(copied_error)) => {
                                Err(io::Error::other(format!(
                                    "copy-free scanout: {shared_error}; copied scanout: {copied_error}"
                                )))
                            }
                        }
                    }
                }
            } else {
                ScanoutBoPool::allocate(
                    Arc::clone(&vk),
                    Rc::clone(&device.device),
                    scanout_route,
                    w,
                    h,
                    SCANOUT_POOL_DEPTH,
                    &layout.output.scanout_modifiers,
                )
                .map(OutputScanout::Shared)
            };
            match allocation {
                Ok(pool) => {
                    let n = pool.display_pool().bos.len();
                    scanout_pools.push(Some(pool));
                    bo_generations.push(vec![BoGenerationEntry::default(); n]);
                }
                Err(e) => {
                    log::warn!(
                        "render: ScanoutBoPool allocate failed for output {i} ({}x{}): {e:?} \
                         — output will be skipped from compose",
                        w,
                        h,
                    );
                    scanout_alloc_errors.push(format!("output {i} ({w}x{h}): {e}"));
                    scanout_pools.push(None);
                    bo_generations.push(Vec::new());
                }
            }
        }
        // Refuse to run invisibly: if there are connected outputs but none
        // got a scanout pool, there is nothing to display — fail loudly
        // instead of leaving a silent black screen. (Split-GPU scanout
        // with no shared modifier, e.g. RPi 4/400, lands here.)
        let live_pool_count = scanout_pools.iter().filter(|p| p.is_some()).count();
        if let Err(msg) = check_scanout_liveness(
            initial_scanout_rollback.outputs().len(),
            live_pool_count,
            &scanout_alloc_errors,
        ) {
            return Err(io::Error::other(format!("render PlatformBackend: {msg}")));
        }
        let first_pageflip_logged = vec![false; initial_scanout_rollback.outputs().len()];

        // Stage 5 Task 6.1: backend-internal poll FD + wakeup
        // eventfd for deferred PRESENT completion. The eventfd lives
        // inside the poll set under `WAKEUP_EVENTFD_TOKEN`; per-entry
        // sync_file FDs join later via the enqueue path.
        let wakeup_eventfd = nix::sys::eventfd::EventFd::from_value_and_flags(
            0,
            nix::sys::eventfd::EfdFlags::EFD_CLOEXEC | nix::sys::eventfd::EfdFlags::EFD_NONBLOCK,
        )
        .map_err(|e| io::Error::other(format!("eventfd: {e}")))?;

        // Backend-internal readiness set (epoll/kqueue). The wakeup
        // eventfd joins it under WAKEUP_EVENTFD_TOKEN; per-batch
        // sync_file FDs are added later via the enqueue path.
        let present_completion_epfd =
            crate::kms::render::completion_poller::CompletionPoller::new()?;
        present_completion_epfd.register(wakeup_eventfd.as_fd(), WAKEUP_EVENTFD_TOKEN)?;
        let scanout_render_completion_epfd =
            crate::kms::render::completion_poller::CompletionPoller::new()?;

        let submit_group = SubmitGroup::new();
        #[cfg(target_os = "linux")]
        let hotplug_monitor = match crate::kms::hotplug::DrmHotplugMonitor::new() {
            Ok(monitor) => monitor,
            Err(e) => {
                // Don't fail bring-up — yserver runs fine without runtime
                // hotplug — but surface WHY (udev/netlink/permission) so a
                // silently-disabled monitor is diagnosable.
                log::warn!(
                    "render PlatformBackend: DRM hotplug monitor unavailable ({e}); \
                     runtime display hotplug disabled"
                );
                None
            }
        };

        log::info!(
            "render PlatformBackend: ready — {} outputs, fb {}x{}, {} scanout pools live",
            initial_scanout_rollback.outputs().len(),
            fb_w,
            fb_h,
            scanout_pools.iter().filter(|p| p.is_some()).count(),
        );

        // `Self` construction below is infallible. Transfer rollback
        // responsibility from the borrowing stack guard to PlatformBackend's
        // Drop implementation so the outer KmsBackend constructor remains
        // covered until it explicitly disarms the completed backend.
        let initial_scanout_rollback_armed = initial_scanout_rollback.is_armed();
        initial_scanout_rollback.disarm();
        drop(initial_scanout_rollback);

        debug_assert_eq!(layouts.len(), scanout_routes.len());
        let outputs = layouts
            .into_iter()
            .zip(scanout_routes)
            .map(|(layout, route)| layout.qualify(route))
            .collect::<Vec<_>>();
        debug_assert!(outputs.iter().zip(&scanout_pools).all(|(output, pool)| {
            pool.as_ref()
                .is_none_or(|pool| pool.route() == output.scanout_route)
        }));

        Ok(Self {
            initial_scanout_rollback_armed,
            devices,
            render_devices,
            selected_render_device: Some(selected_render_device),
            outputs,
            fb_w,
            fb_h,
            output_transforms: HashMap::new(),
            ust_msc: std::collections::HashMap::new(),
            completion_clocks: std::collections::HashMap::new(),
            software_msc: std::collections::HashMap::new(),
            input_ctx,
            #[cfg(target_os = "linux")]
            hotplug_monitor,
            present_completion_epfd,
            wakeup_eventfd,
            scanout_render_completion_epfd,
            pending_scanout_render_completions: std::collections::VecDeque::new(),
            next_scanout_render_job_id: 1,
            vk: Some(vk),
            scanout_readback_op: None,
            ops_command_pool: Some(ops_command_pool),
            fence_pool: Some(fence_pool),
            scanout_readback: None,
            pixmap_pool,
            copy_vk_contexts,
            scanout_pools,
            bo_generations,
            next_present_generation: 0,
            first_pageflip_logged,
            renderer_failed: false,
            shutting_down: false,
            submit_group,
            last_flush_outcome: None,
            force_next_submit_failure: false,
            next_submit_cause: None,
            force_next_frame_record_failure: false,
            queue_submits: 0,
        })
    }

    /// Headless test seed. No live DRM device, no Vk, single
    /// stub 800×600 output. Mirrors `KmsBackend::for_tests`'s
    /// existing shape from Stage 1b.
    #[doc(hidden)]
    pub(crate) fn for_tests() -> Self {
        let wakeup_eventfd = nix::sys::eventfd::EventFd::from_value_and_flags(
            0,
            nix::sys::eventfd::EfdFlags::EFD_CLOEXEC | nix::sys::eventfd::EfdFlags::EFD_NONBLOCK,
        )
        .expect("test eventfd");

        let present_completion_epfd =
            crate::kms::render::completion_poller::CompletionPoller::new().expect("test poller");
        present_completion_epfd
            .register(wakeup_eventfd.as_fd(), WAKEUP_EVENTFD_TOKEN)
            .expect("test poller register");
        let scanout_render_completion_epfd =
            crate::kms::render::completion_poller::CompletionPoller::new()
                .expect("test scanout completion poller");
        #[cfg(target_os = "linux")]
        let hotplug_monitor = None;
        let device_key = crate::platform::drm::DrmDeviceKey { major: 0, minor: 0 };
        let test_route = ScanoutRoute::new(
            RenderDeviceId::UnverifiedFallback,
            device_key,
            RenderKmsRelationship::Unknown,
        );
        let device = Rc::new(drm::Device::for_tests().expect("test drm device"));
        Self {
            initial_scanout_rollback_armed: false,
            devices: vec![KmsDevice {
                key: device_key,
                device,
                cursor: KmsCursorState::new(),
            }],
            render_devices: Vec::new(),
            selected_render_device: None,
            outputs: vec![ActiveOutput::new(
                test_route,
                crate::platform::drm::Output {
                    connector: ::drm::control::from_u32(1).unwrap(),
                    connector_name: "test".to_string(),
                    encoder: ::drm::control::from_u32(1).unwrap(),
                    crtc: ::drm::control::from_u32(1).unwrap(),
                    plane: ::drm::control::from_u32(1).unwrap(),
                    // SAFETY: tests never pass this mode to DRM.
                    mode: unsafe { std::mem::zeroed() },
                    picked: crate::platform::drm::Mode {
                        name: "test".to_string(),
                        width: 800,
                        height: 600,
                        vrefresh: 60,
                        preferred: true,
                        ..Default::default()
                    },
                    plane_fb_id_prop: ::drm::control::from_u32(1).unwrap(),
                    plane_crtc_id_prop: ::drm::control::from_u32(1).unwrap(),
                    plane_src_x_prop: ::drm::control::from_u32(1).unwrap(),
                    plane_src_y_prop: ::drm::control::from_u32(1).unwrap(),
                    plane_src_w_prop: ::drm::control::from_u32(1).unwrap(),
                    plane_src_h_prop: ::drm::control::from_u32(1).unwrap(),
                    plane_crtc_x_prop: ::drm::control::from_u32(1).unwrap(),
                    plane_crtc_y_prop: ::drm::control::from_u32(1).unwrap(),
                    plane_crtc_w_prop: ::drm::control::from_u32(1).unwrap(),
                    plane_crtc_h_prop: ::drm::control::from_u32(1).unwrap(),
                    plane_in_fence_fd_prop: None,
                    crtc_out_fence_ptr_prop: None,
                    scanout_modifiers: Vec::new(),
                    mm_width: 0,
                    mm_height: 0,
                    edid: Vec::new(),
                    connector_type: "unknown".to_string(),
                    modes: vec![crate::platform::drm::Mode {
                        name: "test".to_string(),
                        width: 800,
                        height: 600,
                        vrefresh: 60,
                        preferred: true,
                        ..Default::default()
                    }],
                },
                drm::Swapchain::empty_for_tests(),
                0,
                0,
            )],
            fb_w: 800,
            fb_h: 600,
            output_transforms: HashMap::new(),
            ust_msc: std::collections::HashMap::new(),
            completion_clocks: std::collections::HashMap::new(),
            software_msc: std::collections::HashMap::new(),
            input_ctx: None,
            #[cfg(target_os = "linux")]
            hotplug_monitor,
            present_completion_epfd,
            wakeup_eventfd,
            scanout_render_completion_epfd,
            pending_scanout_render_completions: std::collections::VecDeque::new(),
            next_scanout_render_job_id: 1,
            vk: None,
            scanout_readback_op: None,
            ops_command_pool: None,
            fence_pool: None,
            scanout_readback: None,
            pixmap_pool: None,
            copy_vk_contexts: HashMap::new(),
            scanout_pools: vec![None],
            bo_generations: vec![Vec::new()],
            next_present_generation: 0,
            first_pageflip_logged: vec![false],
            renderer_failed: false,
            shutting_down: false,
            submit_group: SubmitGroup::new(),
            last_flush_outcome: None,
            force_next_submit_failure: false,
            next_submit_cause: None,
            force_next_frame_record_failure: false,
            queue_submits: 0,
        }
    }

    /// Attach a live Vulkan context to the headless test fixture while
    /// preserving the renderer-inventory invariant used by production.
    pub(crate) fn attach_test_vk_context(&mut self, vk: Arc<VkContext>) {
        let (render_devices, selected) = build_render_device_inventory(&vk, None)
            .expect("a test Vulkan context without an opened render node cannot mismatch");
        self.render_devices = render_devices;
        self.selected_render_device = Some(selected);
        let routes = self
            .outputs
            .iter()
            .map(|output| {
                self.scanout_route_for_kms(output.key.device_key)
                    .expect("test output has a KMS owner and selected renderer")
            })
            .collect::<Vec<_>>();
        for (output, route) in self.outputs.iter_mut().zip(routes) {
            output.scanout_route = route;
        }
        self.vk = Some(vk);
    }
}
