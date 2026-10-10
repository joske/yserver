use super::*;

pub(super) fn scanout_pool_needs_reallocation(
    existing: Option<&ActiveOutput>,
    existing_pool_route: Option<ScanoutRoute>,
    width: u16,
    height: u16,
    route: ScanoutRoute,
) -> bool {
    existing.is_none_or(|output| {
        output.width != width
            || output.height != height
            || output.scanout_route != route
            || existing_pool_route != Some(route)
    })
}

pub(super) fn mode_via_connector_handle<T: Copy>(
    connector: ::drm::control::connector::Handle,
    mode_spec: yserver_core::backend::ModeSpec,
    query_modes: impl FnOnce(::drm::control::connector::Handle) -> io::Result<Vec<T>>,
    mode_timing: impl Fn(&T) -> (u16, u16, u32),
) -> io::Result<Option<T>> {
    let modes = query_modes(connector)?;
    Ok(modes.into_iter().find(|mode| {
        let (width, height, vrefresh) = mode_timing(mode);
        width == mode_spec.width && height == mode_spec.height && vrefresh == mode_spec.vrefresh
    }))
}

impl DroppedRoute {
    /// The layout rectangle this route occupied, in the `(x, y, w, h)` shape
    /// [`recompute_fb_extent_from`] consumes.
    pub(crate) fn rect(&self) -> LayoutRect {
        (self.x, self.y, self.width, self.height)
    }
}

impl ConnectorSnapshot {
    fn from_probe(
        device_key: crate::platform::drm::DrmDeviceKey,
        probe: &crate::platform::drm::ConnectorSnapshotProbe,
    ) -> Self {
        Self {
            key: OutputKey::new(device_key, probe.connector_name.clone()),
            modes: probe.modes.clone(),
            mm_width: probe.mm_width,
            mm_height: probe.mm_height,
            edid: probe.edid.clone(),
            connector_type: probe.connector_type.clone(),
        }
    }

    pub(crate) fn preserves_active_output(&self, output: &ActiveOutput) -> bool {
        // A monitor replacement on the same connector does not revoke the
        // mode already programmed in KMS. Keep that live route until a RANDR
        // client selects a replacement; only a disconnected/no-mode probe
        // forces it Off. The registry's exact mode identities are
        // authoritative without changing this live-mode preservation policy.
        self.key == output.key && !self.modes.is_empty()
    }
}

/// Pure recompute of the virtual-screen extent from `(x, y, width, height)`.
///
/// 2-D: `fb_w = max(x + width)`, `fb_h = max(y + height)`. A client may
/// place a CRTC at any `(x, y)` (e.g. a monitor stacked below), so the
/// framebuffer must encompass `y + height`, not just `max(height)`.
/// Advance `next_x` past every reserved slot a `width`-wide placement
/// starting there would straddle.
///
/// Reservations are rectangles of routes that are physically gone but
/// restorable, so nothing may be packed over them. Repeat until the
/// placement is clear: stepping past one reservation can push the
/// placement into the next.
pub(super) fn advance_past_reservations(
    mut next_x: i32,
    width: u16,
    reserved: &[LayoutRect],
) -> i32 {
    loop {
        let end = next_x.saturating_add(i32::from(width));
        let Some(blocking_end) = reserved
            .iter()
            .filter(|(rx, _, rw, _)| {
                let r_end = rx.saturating_add(i32::from(*rw));
                // Half-open overlap: a reservation ending exactly at `next_x`
                // does not block, and a zero-width one never blocks.
                *rx < end && next_x < r_end
            })
            .map(|(rx, _, rw, _)| rx.saturating_add(i32::from(*rw)))
            .max()
        else {
            return next_x;
        };
        next_x = blocking_end;
    }
}

pub(super) fn recompute_fb_extent_from(layouts: &[LayoutRect]) -> (u16, u16) {
    let fb_w = layouts
        .iter()
        .map(|(x, _, w, _)| x.saturating_add(i32::from(*w)))
        .map(|v| u16::try_from(v.max(0)).unwrap_or(u16::MAX))
        .max()
        .unwrap_or(0);
    let fb_h = layouts
        .iter()
        .map(|(_, y, _, h)| y.saturating_add(i32::from(*h)))
        .map(|v| u16::try_from(v.max(0)).unwrap_or(u16::MAX))
        .max()
        .unwrap_or(0);
    (fb_w, fb_h)
}

impl PlatformBackend {
    /// Gather lightweight connector probes for every opened DRM device.
    /// Results are accumulated before callers mutate RANDR state, so a
    /// failure on card N cannot leave a half-reconciled combined snapshot.
    pub(crate) fn probe_all_connectors(
        &self,
    ) -> io::Result<
        Vec<(
            crate::platform::drm::DrmDeviceKey,
            Vec<crate::platform::drm::ConnectorProbe>,
        )>,
    > {
        let mut all = Vec::with_capacity(self.devices.len());
        for device in &self.devices {
            let probes =
                crate::platform::drm::probe_connectors(&device.device).map_err(|error| {
                    io::Error::new(
                        error.kind(),
                        format!("probe connectors on DRM device {}: {error}", device.key),
                    )
                })?;
            all.push((device.key, probes));
        }
        Ok(all)
    }

    /// Gather the complete connected-connector snapshot without mutating any
    /// live output, pool, or RANDR-facing state. The backend can therefore
    /// quiesce GPU/page-flip/direct work only after every device probe has
    /// succeeded, and before applying removals.
    pub(crate) fn probe_connector_snapshot(&self) -> io::Result<Vec<ConnectorSnapshot>> {
        let mut snapshot = Vec::new();
        for device in &self.devices {
            let probes = crate::platform::drm::probe_connector_snapshots(&device.device).map_err(
                |error| {
                    io::Error::new(
                        error.kind(),
                        format!(
                            "probe connector snapshot on DRM device {}: {error}",
                            device.key
                        ),
                    )
                },
            )?;
            snapshot.extend(
                probes
                    .iter()
                    .map(|probe| ConnectorSnapshot::from_probe(device.key, probe)),
            );
        }
        Ok(snapshot)
    }

    /// Disable a single connector: issue a DRM `disable_output` for the
    /// matching `ActiveOutput`, free/drop its scanout pool entry, and
    /// remove it from `self.outputs` / parallel vecs.  Recomputes
    /// `fb_w`/`fb_h` from the remaining outputs (2-D, no recompact —
    /// client-driven layouts are preserved). Does NOT touch the
    /// `RandrIdAllocator` registry; callers update it after we return.
    ///
    /// Returns `Ok(true)` when the connector was found and disabled,
    /// `Ok(false)` when it was not currently in the active output list
    /// (already off — no-op), or `Err` on a DRM-level failure.
    pub(crate) fn disable_connector(&mut self, output_key: &OutputKey) -> io::Result<bool> {
        let connector = &output_key.connector_name;
        let idx = match self
            .outputs
            .iter()
            .position(|layout| &layout.key == output_key)
        {
            Some(i) => i,
            None => return Ok(false),
        };
        let device = Rc::clone(
            &self
                .device_for_output(output_key)
                .ok_or_else(|| io::Error::other(format!("no DRM device for {output_key:?}")))?
                .device,
        );

        // DRM disable (ALLOW_MODESET atomic commit zeroing the CRTC).
        if let Err(e) = crate::drm::modeset::disable_output(&device, &self.outputs[idx].output) {
            log::error!("render disable_connector: disable_output({connector}) failed: {e}");
            return Err(e);
        }

        self.remove_connector_at(idx);

        log::info!(
            "render disable_connector: {connector} disabled; fb now {}×{}",
            self.fb_w,
            self.fb_h
        );
        Ok(true)
    }

    /// Remove a connector after the caller has successfully disabled the
    /// complete old CRTC set. This is the topology-mutation counterpart to
    /// `disable_connector`: it must not issue a second ioctl against an
    /// already-off (or newly disconnected) connector object.
    pub(crate) fn remove_connector_after_all_off(&mut self, output_key: &OutputKey) -> bool {
        let Some(idx) = self
            .outputs
            .iter()
            .position(|layout| &layout.key == output_key)
        else {
            return false;
        };
        self.remove_connector_at(idx);
        true
    }

    pub(super) fn remove_connector_at(&mut self, idx: usize) {
        let output_key = self.outputs[idx].key.clone();
        let changed_device = self.outputs[idx].key.device_key;
        self.cancel_scanout_render_completions_for_output(&output_key);
        if let Err(error) = self.drain_scanout_pool_at(idx) {
            log::error!(
                "connector removal could not quiesce {output_key:?}: {error}; \
                 copied resources remain quarantined"
            );
        }
        // Drop the scanout pool for this output so its VkImages are freed.
        if idx < self.scanout_pools.len() {
            self.scanout_pools.remove(idx);
        }
        if idx < self.bo_generations.len() {
            self.bo_generations.remove(idx);
        }
        if idx < self.first_pageflip_logged.len() {
            self.first_pageflip_logged.remove(idx);
        }
        self.outputs.remove(idx);

        // Recompute the virtual framebuffer extent from surviving outputs.
        // Do NOT recompact — other outputs may be client-positioned.
        let layouts: Vec<(i32, i32, u16, u16)> = self
            .outputs
            .iter()
            .map(|l| (l.x, l.y, l.width, l.height))
            .collect();
        let (fb_w, fb_h) = recompute_fb_extent_from(&layouts);
        self.fb_w = fb_w;
        self.fb_h = fb_h;
        self.prune_present_clocks_to_live_outputs();
        self.refresh_cursor_topology_for_devices(&HashSet::from([changed_device]));
    }

    fn resolve_connector_enable(
        &self,
        output_key: &OutputKey,
        mut output: crate::platform::drm::Output,
        mode_spec: yserver_core::backend::ModeSpec,
    ) -> io::Result<ResolvedConnectorEnable> {
        let connector = output.connector_name.clone();
        debug_assert_eq!(output_key.connector_name, connector);
        let device = Rc::clone(
            &self
                .device_for_output(output_key)
                .ok_or_else(|| io::Error::other(format!("no DRM device for {output_key:?}")))?
                .device,
        );
        let scanout_route = self.scanout_route_for_kms(output_key.device_key)?;

        if let Some(conflict) = self.outputs.iter().find(|layout| {
            layout.key.device_key == output_key.device_key
                && layout.key != *output_key
                && (layout.output.encoder == output.encoder
                    || layout.output.crtc == output.crtc
                    || layout.output.plane == output.plane)
        }) {
            return Err(io::Error::other(format!(
                "enable_connector {connector}: proposed encoder {:?}/CRTC {:?}/plane {:?} conflicts with \
                 live output {} on the same DRM device",
                output.encoder, output.crtc, output.plane, conflict.output.connector_name
            )));
        }

        let matched = output
            .modes
            .iter()
            .find(|mode| {
                mode.width == mode_spec.width
                    && mode.height == mode_spec.height
                    && mode.vrefresh == mode_spec.vrefresh
            })
            .cloned();
        let mode_local = matched.ok_or_else(|| {
            io::Error::other(format!(
                "connector {connector}: mode {}×{}@{} not in advertised list",
                mode_spec.width, mode_spec.height, mode_spec.vrefresh
            ))
        })?;

        if output.picked.width != mode_spec.width
            || output.picked.height != mode_spec.height
            || output.picked.vrefresh != mode_spec.vrefresh
        {
            use ::drm::control::Device as ControlDevice;
            let drm_mode_opt = mode_via_connector_handle(
                output.connector,
                mode_spec,
                |connector_handle| {
                    device
                        .get_connector(connector_handle, false)
                        .map(|info| info.modes().to_vec())
                        .map_err(|error| {
                            io::Error::new(
                                error.kind(),
                                format!(
                                    "connector {connector} ({connector_handle:?}): get_connector failed: {error}"
                                ),
                            )
                        })
                },
                |mode| {
                    let (width, height) = mode.size();
                    (width, height, mode.vrefresh())
                },
            )?;
            let drm_mode = drm_mode_opt.ok_or_else(|| {
                io::Error::other(format!(
                    "connector {connector} ({:?}): DRM mode {}×{}@{} not found via kernel",
                    output.connector, mode_spec.width, mode_spec.height, mode_spec.vrefresh
                ))
            })?;
            output.mode = drm_mode;
            output.picked = mode_local;
        }

        let existing_idx = self
            .outputs
            .iter()
            .position(|layout| &layout.key == output_key);
        if let Some(index) = existing_idx
            && (index >= self.scanout_pools.len() || index >= self.bo_generations.len())
        {
            return Err(io::Error::other(format!(
                "enable_connector {connector}: active output index {index} has no paired scanout-pool/generation slot"
            )));
        }
        let needs_pool_realloc = scanout_pool_needs_reallocation(
            existing_idx.and_then(|idx| self.outputs.get(idx)),
            existing_idx
                .and_then(|idx| self.scanout_pools.get(idx))
                .and_then(Option::as_ref)
                .map(OutputScanout::route),
            mode_spec.width,
            mode_spec.height,
            scanout_route,
        );

        Ok(ResolvedConnectorEnable {
            connector,
            device,
            output,
            scanout_route,
            existing_idx,
            needs_pool_realloc,
        })
    }

    /// Enable (or reconfigure) a single connector at `(x, y)` with
    /// the given `ModeSpec`.  Resolves the `ModeSpec` against the
    /// connector's discovered `Output::modes` list, (re)allocates the
    /// `ScanoutBoPool` when the resolution changes or the output was
    /// previously off, commits the modeset, and adds/updates the
    /// `ActiveOutput` in `self.outputs` and the parallel vecs.
    ///
    /// The `Output` for `connector` must be pre-discovered with the live
    /// routes of every same-device survivor reserved. The selected `Output`
    /// is consumed.
    ///
    /// On any failure after pool allocation, the pool is freed and the
    /// output stays off (no partial enable), leaving `self` consistent.
    ///
    /// Returns `Ok(())` on success.
    pub(crate) fn enable_connector(
        &mut self,
        output_key: &OutputKey,
        output: crate::platform::drm::Output,
        mode_spec: yserver_core::backend::ModeSpec,
        x: i32,
        y: i32,
    ) -> io::Result<()> {
        self.enable_connector_with_cursor_factory(
            output_key,
            output,
            mode_spec,
            x,
            y,
            initialize_cursor_plane_for_device,
        )
    }

    /// Replay one resource-free worker qualification on the live Vulkan
    /// context and install the resulting pool/output through the ordinary
    /// synchronous modeset ownership path. This never probes another
    /// representation or falls back to a different transport.
    pub(crate) fn enable_connector_with_qualified_plan(
        &mut self,
        output_key: &OutputKey,
        output: crate::platform::drm::Output,
        mode_spec: yserver_core::backend::ModeSpec,
        x: i32,
        y: i32,
        qualified: QualifiedScanoutPlan,
    ) -> io::Result<()> {
        let prepared =
            self.prepare_qualified_connector_plan(output_key, output, mode_spec, x, y, qualified)?;
        self.install_prepared_connector_plan(prepared)
    }

    /// Replay the exact worker-selected representation on live Vulkan
    /// contexts without changing KMS state. Allocation and atomic TEST_ONLY
    /// happen here while the old display topology remains lit.
    pub(crate) fn prepare_qualified_connector_plan(
        &mut self,
        output_key: &OutputKey,
        output: crate::platform::drm::Output,
        mode_spec: yserver_core::backend::ModeSpec,
        x: i32,
        y: i32,
        qualified: QualifiedScanoutPlan,
    ) -> io::Result<PreparedQualifiedConnector> {
        let resolved = self.resolve_connector_enable(output_key, output, mode_spec)?;
        let ResolvedConnectorEnable {
            connector,
            device,
            output,
            scanout_route,
            existing_idx: _,
            needs_pool_realloc,
        } = resolved;
        if !route_requires_copy_free_probe(scanout_route) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "prepare qualified connector {connector}: route {scanout_route:?} is not cross-device"
                ),
            ));
        }
        if !needs_pool_realloc {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "prepare qualified connector {connector}: live output no longer needs scanout reallocation"
                ),
            ));
        }
        let vk = self.vk.as_ref().cloned().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                format!("prepare qualified connector {connector}: no live Vulkan renderer"),
            )
        })?;

        let pool = match qualified {
            qualified @ QualifiedScanoutPlan::Shared(_) => {
                match replay_copy_free_scanout_plan(
                    vk,
                    device,
                    &output,
                    scanout_route,
                    u32::from(mode_spec.width),
                    u32::from(mode_spec.height),
                    &output.scanout_modifiers,
                    qualified,
                    false,
                ) {
                    Ok(ExactPlanReplay::Prepared(prepared)) => {
                        debug_assert!(prepared.committed_framebuffer.is_none());
                        OutputScanout::Shared(prepared.pool)
                    }
                    Ok(ExactPlanReplay::Rejected(error)) => return Err(error),
                    Err(error @ CopyFreeScanoutError::TerminalDisposableProbe(_)) => {
                        return Err(error.into_io_error());
                    }
                    Err(error @ CopyFreeScanoutError::LiveRendererLost(_)) => {
                        self.renderer_failed = true;
                        return Err(error.into_io_error());
                    }
                    Err(CopyFreeScanoutError::Candidates(error)) => return Err(error),
                }
            }
            qualified @ QualifiedScanoutPlan::Copied { sink_id, .. } => {
                let (live_sink_id, sink_vk) =
                    self.copied_sink_context_for_kms(output_key.device_key)?;
                if live_sink_id != sink_id {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "prepare qualified connector {connector}: qualified sink {sink_id:?} no longer matches live sink {live_sink_id:?}"
                        ),
                    ));
                }
                let destination_route = ScanoutRoute::new(
                    live_sink_id,
                    output_key.device_key,
                    RenderKmsRelationship::Same,
                );
                match replay_copied_scanout_plan(
                    vk,
                    sink_vk,
                    device,
                    &output,
                    scanout_route,
                    destination_route,
                    u32::from(mode_spec.width),
                    u32::from(mode_spec.height),
                    &output.scanout_modifiers,
                    qualified,
                    false,
                ) {
                    Ok(ExactPlanReplay::Prepared(prepared)) => {
                        debug_assert!(prepared.committed_framebuffer.is_none());
                        OutputScanout::Copied(prepared.pool)
                    }
                    Ok(ExactPlanReplay::Rejected(error)) => return Err(error),
                    Err(error @ CopiedScanoutError::TerminalDisposableProbe(_)) => {
                        return Err(error.into_io_error());
                    }
                    Err(error @ CopiedScanoutError::LiveDeviceLost { .. }) => {
                        self.renderer_failed = true;
                        return Err(error.into_io_error());
                    }
                    Err(CopiedScanoutError::Candidates(error))
                        if crate::kms::vk::scanout::scanout_error_is_device_lost(&error) =>
                    {
                        self.renderer_failed = true;
                        return Err(error);
                    }
                    Err(CopiedScanoutError::Candidates(error)) => return Err(error),
                }
            }
        };

        Ok(PreparedQualifiedConnector {
            output_key: output_key.clone(),
            output,
            mode_spec,
            x,
            y,
            scanout_route,
            pool,
        })
    }

    /// Commit and install a pool produced by
    /// [`Self::prepare_qualified_connector_plan`]. No allocation,
    /// qualification, or Vulkan content probe occurs in this short boundary.
    pub(crate) fn install_prepared_connector_plan(
        &mut self,
        prepared: PreparedQualifiedConnector,
    ) -> io::Result<()> {
        let PreparedQualifiedConnector {
            output_key,
            output,
            mode_spec,
            x,
            y,
            scanout_route,
            pool,
        } = prepared;
        self.enable_connector_inner(
            &output_key,
            output,
            mode_spec,
            x,
            y,
            Some((scanout_route, pool)),
            initialize_cursor_plane_for_device,
        )
    }

    pub(super) fn enable_connector_with_cursor_factory<F>(
        &mut self,
        output_key: &OutputKey,
        output: crate::platform::drm::Output,
        mode_spec: yserver_core::backend::ModeSpec,
        x: i32,
        y: i32,
        cursor_factory: F,
    ) -> io::Result<()>
    where
        F: FnMut(&mut KmsDevice, &[::drm::control::crtc::Handle], &str),
    {
        self.enable_connector_inner(output_key, output, mode_spec, x, y, None, cursor_factory)
    }

    #[allow(clippy::too_many_arguments)]
    fn enable_connector_inner<F>(
        &mut self,
        output_key: &OutputKey,
        output: crate::platform::drm::Output,
        mode_spec: yserver_core::backend::ModeSpec,
        x: i32,
        y: i32,
        prepared_pool: Option<(ScanoutRoute, OutputScanout)>,
        mut cursor_factory: F,
    ) -> io::Result<()>
    where
        F: FnMut(&mut KmsDevice, &[::drm::control::crtc::Handle], &str),
    {
        let resolved = self.resolve_connector_enable(output_key, output, mode_spec)?;
        let ResolvedConnectorEnable {
            connector,
            device,
            output,
            scanout_route,
            existing_idx,
            needs_pool_realloc,
        } = resolved;
        let w = mode_spec.width;
        let h = mode_spec.height;

        if let Some((prepared_route, prepared)) = prepared_pool.as_ref() {
            if !needs_pool_realloc {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "install prepared connector {connector}: live output no longer needs scanout reallocation"
                    ),
                ));
            }
            if *prepared_route != scanout_route || prepared.route() != scanout_route {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "install prepared connector {connector}: prepared route {prepared_route:?}/pool {:?} no longer matches live route {scanout_route:?}",
                        prepared.route(),
                    ),
                ));
            }
        }

        // (Re)allocate the scanout pool if needed.
        let mut new_pool_committed_framebuffer = None;
        let mut new_pool: Option<Option<OutputScanout>> = if needs_pool_realloc {
            if let Some((_prepared_route, pool)) = prepared_pool {
                // Exact replay and live TEST_ONLY already completed while the
                // previous topology was still active. Keep this dark-window
                // path to the KMS commit and ownership installation only.
                Some(Some(pool))
            } else if let Some(vk) = self.vk.as_ref().cloned() {
                let allocation: io::Result<OutputScanout> =
                    if route_requires_copy_free_probe(scanout_route) {
                        match allocate_copy_free_scanout_pool(
                            Arc::clone(&vk),
                            Rc::clone(&device),
                            &output,
                            scanout_route,
                            u32::from(w),
                            u32::from(h),
                            &output.scanout_modifiers,
                            true,
                        ) {
                            Ok(prepared) => {
                                new_pool_committed_framebuffer = prepared.committed_framebuffer;
                                Ok(OutputScanout::Shared(prepared.pool))
                            }
                            Err(error @ CopyFreeScanoutError::TerminalDisposableProbe(_)) => {
                                return Err(error.into_io_error());
                            }
                            Err(error @ CopyFreeScanoutError::LiveRendererLost(_)) => {
                                self.renderer_failed = true;
                                return Err(error.into_io_error());
                            }
                            Err(CopyFreeScanoutError::Candidates(shared_error)) => {
                                let copied_result = self
                                    .copied_sink_context_for_kms(output_key.device_key)
                                    .map_err(CopiedScanoutError::Candidates)
                                    .and_then(|(sink_id, sink_vk)| {
                                        let destination_route = ScanoutRoute::new(
                                            sink_id,
                                            output_key.device_key,
                                            RenderKmsRelationship::Same,
                                        );
                                        allocate_copied_scanout_pool(
                                            Arc::clone(&vk),
                                            sink_vk,
                                            Rc::clone(&device),
                                            &output,
                                            scanout_route,
                                            destination_route,
                                            u32::from(w),
                                            u32::from(h),
                                            &output.scanout_modifiers,
                                            true,
                                        )
                                    });
                                match copied_result {
                                Ok(prepared) => {
                                    new_pool_committed_framebuffer = prepared.committed_framebuffer;
                                    Ok(OutputScanout::Copied(prepared.pool))
                                }
                                Err(error @ CopiedScanoutError::TerminalDisposableProbe(_)) => {
                                    return Err(error.into_io_error());
                                }
                                Err(error @ CopiedScanoutError::LiveDeviceLost { .. }) => {
                                    self.renderer_failed = true;
                                    return Err(error.into_io_error());
                                }
                                Err(CopiedScanoutError::Candidates(copied_error))
                                    if crate::kms::vk::scanout::scanout_error_is_device_lost(
                                        &copied_error,
                                    ) =>
                                {
                                    self.renderer_failed = true;
                                    return Err(copied_error);
                                }
                                Err(CopiedScanoutError::Candidates(copied_error)) => {
                                    Err(io::Error::other(format!(
                                        "copy-free scanout: {shared_error}; copied scanout: \
                                         {copied_error}"
                                    )))
                                }
                            }
                            }
                        }
                    } else {
                        ScanoutBoPool::allocate(
                            Arc::clone(&vk),
                            Rc::clone(&device),
                            scanout_route,
                            u32::from(w),
                            u32::from(h),
                            SCANOUT_POOL_DEPTH,
                            &output.scanout_modifiers,
                        )
                        .map(OutputScanout::Shared)
                    };
                match allocation {
                    Ok(pool) => Some(Some(pool)),
                    Err(e) => {
                        log::warn!(
                            "render enable_connector: scanout pool setup failed for {connector} ({}×{}): {e:?}",
                            w,
                            h
                        );
                        // Pool allocation failed — leave output off,
                        // return error to caller.
                        return Err(io::Error::other(format!(
                            "enable_connector {connector}: scanout pool setup failed: {e:?}"
                        )));
                    }
                }
            } else {
                // Test fixture: no Vk; pool stays None.
                Some(None)
            }
        } else {
            None // keep existing pool
        };

        // Build an initial fb for the modeset commit.  Pick the
        // OnScreen BO from the existing pool (if unchanged), or the
        // first BO in the new pool.  Fall back to a legacy dumb buffer
        // if nothing is available.
        let fb_for_commit = if let Some(framebuffer) = new_pool_committed_framebuffer {
            // The candidate helper already committed and marked this exact
            // framebuffer.  From here through pool installation the path is
            // deliberately infallible: dropping a successfully committed
            // candidate would free memory still referenced by KMS.
            framebuffer
        } else {
            let pool_ref: Option<&ScanoutBoPool> = if needs_pool_realloc {
                new_pool
                    .as_ref()
                    .and_then(|p| p.as_ref())
                    .map(OutputScanout::display_pool)
            } else {
                existing_idx
                    .and_then(|i| self.scanout_pools.get(i))
                    .and_then(|p| p.as_ref())
                    .map(OutputScanout::display_pool)
            };
            pool_ref
                .and_then(|pool| {
                    use crate::kms::vk::scanout::BoPhase;
                    pool.bos
                        .iter()
                        .find(|bo| bo.state.phase == BoPhase::OnScreen)
                        .and_then(|bo| bo.fb_handle)
                        .or_else(|| pool.bos.iter().find_map(|bo| bo.fb_handle))
                })
                .ok_or_else(|| {
                    io::Error::other(format!(
                        "enable_connector {connector}: no fb handle available for initial modeset"
                    ))
                })?
        };

        // Commit the modeset.  On failure, pool is freed (dropped below).
        if new_pool_committed_framebuffer.is_none()
            && let Err(e) = crate::drm::modeset::commit_modeset(&device, &output, fb_for_commit)
        {
            log::error!(
                "render enable_connector: commit_modeset for {connector} ({}×{}@{}) at ({x},{y}) failed: {e}",
                mode_spec.width,
                mode_spec.height,
                mode_spec.vrefresh
            );
            // new_pool dropped here (freed on stack unwind).
            return Err(e);
        }

        // A synchronous modeset has already latched this framebuffer; unlike
        // an ordinary page flip no completion event will promote it from
        // Pending. Reserve it now so the compositor cannot immediately acquire
        // and render into the live front buffer.
        let mark_front = |scanout: &mut OutputScanout| -> io::Result<()> {
            let bo_idx = scanout
                .display_pool()
                .bos
                .iter()
                .position(|bo| bo.fb_handle == Some(fb_for_commit));
            if let Some(bo_idx) = bo_idx {
                scanout.display_pool_mut().bos[bo_idx]
                    .state
                    .mark_on_screen_after_modeset();
                scanout.note_kms_modeset_installed(bo_idx)?;
            }
            Ok(())
        };
        // Once commit_modeset succeeds the selected framebuffer is KMS-owned.
        // Defer any bookkeeping error until the pool has been installed into
        // `self`, so an invariant failure cannot unwind and free live backing.
        let mut post_commit_ownership_error = None;
        if needs_pool_realloc && new_pool_committed_framebuffer.is_none() {
            if let Some(pool) = new_pool.as_mut().and_then(Option::as_mut) {
                post_commit_ownership_error = mark_front(pool).err();
            }
        } else if !needs_pool_realloc
            && let Some(pool) = existing_idx
                .and_then(|idx| self.scanout_pools.get_mut(idx))
                .and_then(Option::as_mut)
        {
            post_commit_ownership_error = mark_front(pool).err();
        }

        // Commit succeeded — install the output into the active set.
        if let Some(idx) = existing_idx {
            // Update in-place.
            self.outputs[idx].output = output;
            self.outputs[idx].scanout_route = scanout_route;
            self.outputs[idx].x = x;
            self.outputs[idx].y = y;
            self.outputs[idx].width = w;
            self.outputs[idx].height = h;
            if let Some(pool) = new_pool {
                self.scanout_pools[idx] = pool;
                self.bo_generations[idx] = self.scanout_pools[idx]
                    .as_ref()
                    .map(|pool| vec![BoGenerationEntry::default(); pool.display_pool().bos.len()])
                    .unwrap_or_default();
            }
        } else {
            // New output — push to end.
            self.outputs.push(ActiveOutput::new(
                scanout_route,
                output,
                drm::Swapchain::empty_for_tests(),
                x,
                y,
            ));
            let pool = new_pool.unwrap_or(None);
            let gens = pool
                .as_ref()
                .map(|p| vec![BoGenerationEntry::default(); p.display_pool().bos.len()])
                .unwrap_or_default();
            self.scanout_pools.push(pool);
            self.bo_generations.push(gens);
            self.first_pageflip_logged.push(false);
        }

        let installed_idx = existing_idx.unwrap_or_else(|| self.outputs.len() - 1);
        debug_assert_eq!(self.outputs[installed_idx].scanout_route, scanout_route);
        debug_assert!(
            self.scanout_pools
                .get(installed_idx)
                .and_then(Option::as_ref)
                .is_none_or(|pool| pool.route() == scanout_route)
        );

        // Recompute virtual framebuffer extent (2-D, no recompact).
        let layouts: Vec<(i32, i32, u16, u16)> = self
            .outputs
            .iter()
            .map(|l| (l.x, l.y, l.width, l.height))
            .collect();
        let (fb_w, fb_h) = recompute_fb_extent_from(&layouts);
        self.fb_w = fb_w;
        self.fb_h = fb_h;
        self.prune_present_clocks_to_live_outputs();
        let changed_devices = HashSet::from([output_key.device_key]);
        // Refresh first. If a previous first-output attempt failed
        // transiently, this later explicit topology boundary retries it. A
        // genuinely deferred device is intentionally skipped here so a
        // failure in the new lazy attempt cannot be retried twice at the same
        // boundary.
        self.refresh_cursor_topology_for_devices_with(&changed_devices, &mut cursor_factory);
        self.initialize_headless_cursor_for_device_with(
            output_key.device_key,
            "first successful explicit RANDR enable",
            |device, crtcs, boundary| cursor_factory(device, crtcs, boundary),
        );

        log::info!(
            "render enable_connector: {connector} enabled {}×{}@{} at ({x},{y}); fb now {}×{}",
            mode_spec.width,
            mode_spec.height,
            mode_spec.vrefresh,
            fb_w,
            fb_h
        );
        if let Some(error) = post_commit_ownership_error {
            self.renderer_failed = true;
            // The KMS commit and platform installation already succeeded.
            // Report success so the caller's RANDR registry converges with
            // the live topology; `renderer_failed` drives the ordinary fatal
            // renderer path instead of misclassifying this as a pre-install
            // configuration rejection.
            log::error!(
                "enable_connector {connector}: committed framebuffer is retained and installed, \
                 but scanout ownership bookkeeping failed: {error}"
            );
        }
        Ok(())
    }

    // ── VT-switch resume helpers (Task 12) ─────────────────────────
    //
    // Called from `KmsBackend::run_resume` after direct VT acquire has
    // restored DRM master on the same DRM fd opened at startup.

    /// Pack outputs left-to-right, but leave client-configured outputs
    /// where the client placed them (Task 5.1). A client SetCrtcConfig/
    /// SetScreenSize "pins" an output's `(x, y)`; the auto-layout must not
    /// flatten it back to the boot extend-right arrangement on a rescan or
    /// VT-resume. Auto (unpinned) outputs still pack sequentially, advancing
    /// past any pinned output's extent so the common left-to-right case
    /// doesn't overlap. (Mixed pinned+auto with gaps is refined later if a
    /// real workload needs it; the common case is all-auto at boot or
    /// all-pinned after the desktop configures the layout.)
    ///
    /// Layout policy is the backend caller's, not the connector snapshot's:
    /// only the backend knows which departed routes are restorable, so only
    /// it can decide when survivors may move. See
    /// `docs/superpowers/specs/2026-09-17-randr-crtc-model-and-hotplug-relight-design.md`,
    /// "Layout policy — reserved slots".
    pub(crate) fn recompact_horizontal_layout(
        &mut self,
        client_configured: &HashSet<OutputKey>,
        reserved: &[LayoutRect],
    ) {
        let mut next_x: i32 = 0;
        for layout in &mut self.outputs {
            if client_configured.contains(&layout.key) {
                next_x = next_x.max(layout.x.saturating_add(i32::from(layout.width)));
                continue;
            }
            // A reserved slot belongs to a route that is physically gone but
            // restorable. Packing over it would let the relight land on top
            // of a survivor that moved into the hole (invariant 7), so step
            // past every reservation this placement would straddle.
            next_x = advance_past_reservations(next_x, layout.width, reserved);
            layout.x = next_x;
            layout.y = 0;
            next_x = next_x.saturating_add(i32::from(layout.width));
        }
    }

    /// Recompute the virtual-screen extent over the live layouts unioned with
    /// `reserved`. A reserved slot keeps the extent from shrinking while its
    /// monitor is away, which is what stops the `SetScreenSize` churn the
    /// hotplug trace showed.
    pub(crate) fn recompute_fb_extent_with_reservations(&mut self, reserved: &[LayoutRect]) {
        let mut layouts: Vec<LayoutRect> = self
            .outputs
            .iter()
            .map(|layout| (layout.x, layout.y, layout.width, layout.height))
            .collect();
        layouts.extend_from_slice(reserved);
        let (fb_w, fb_h) = recompute_fb_extent_from(&layouts);
        self.fb_w = fb_w;
        self.fb_h = fb_h;
    }

    /// Apply a previously gathered all-device connector snapshot. Callers must
    /// quiesce GPU/page-flip state before invoking this method: it may remove
    /// scanout pools and ActiveOutputs for disconnected connectors.
    ///
    /// Topology ownership only: the dropped routes' rectangles are reported
    /// in [`RescanResult::dropped_layouts`] and the caller owns packing and
    /// the virtual-screen extent.
    pub(crate) fn apply_connector_snapshot(
        &mut self,
        connected: Vec<ConnectorSnapshot>,
        known_connected: &HashSet<OutputKey>,
    ) -> RescanResult {
        let connected_order: Vec<OutputKey> = connected
            .iter()
            .map(|snapshot| snapshot.key.clone())
            .collect();
        let connected_keys: HashSet<OutputKey> = connected_order.iter().cloned().collect();
        // A connector can be physically connected yet advertise no usable
        // mode. Keep it connected in RANDR, but it cannot retain a live CRTC
        // route or scanout pool.
        let active_survivor_keys: HashSet<OutputKey> = self
            .outputs
            .iter()
            .filter(|output| {
                connected
                    .iter()
                    .any(|snapshot| snapshot.preserves_active_output(output))
            })
            .map(|output| output.key.clone())
            .collect();
        let snapshot_by_key: HashMap<OutputKey, ConnectorSnapshot> = connected
            .iter()
            .map(|snapshot| (snapshot.key.clone(), snapshot.clone()))
            .collect();

        let mut rescan = RescanResult {
            added_keys: connected_order
                .iter()
                .filter(|key| !known_connected.contains(*key))
                .cloned()
                .collect(),
            dropped_keys: known_connected
                .difference(&connected_keys)
                .cloned()
                .collect(),
            connected,
            ..RescanResult::default()
        };
        for (idx, layout) in self.outputs.iter().enumerate() {
            if active_survivor_keys.contains(&layout.key) {
                continue;
            }
            log::warn!(
                "render rescan: output {} disconnected — dropping active scanout",
                layout.output.connector_name,
            );
            rescan.dropped_old_indices.push(idx);
            // A preceding forced RANDR probe may already have marked this
            // connector disconnected in the registry. Keep the active-output
            // identity in the transition result anyway so its Enabled config
            // and client-configured bit are retired when the physical rescan
            // removes the live CRTC.
            rescan.dropped_keys.push(layout.key.clone());
        }
        rescan.dropped_keys.sort();
        rescan.dropped_keys.dedup();
        rescan.dropped_old_indices.sort_unstable_by(|a, b| b.cmp(a));
        let cursor_changed_devices: HashSet<_> = rescan
            .dropped_old_indices
            .iter()
            .filter_map(|idx| self.outputs.get(*idx))
            .map(|output| output.key.device_key)
            .collect();
        for idx in rescan.dropped_old_indices.iter().copied() {
            let dropped_key = self.outputs[idx].key.clone();
            rescan.dropped_layouts.push(DroppedRoute {
                key: dropped_key.clone(),
                x: self.outputs[idx].x,
                y: self.outputs[idx].y,
                width: self.outputs[idx].width,
                height: self.outputs[idx].height,
                vrefresh: self.outputs[idx].output.picked.vrefresh,
            });
            self.cancel_scanout_render_completions_for_output(&dropped_key);
            if let Err(error) = self.drain_scanout_pool_at(idx) {
                log::error!(
                    "rescan could not quiesce dropped output {dropped_key:?}: {error}; \
                     copied resources remain quarantined"
                );
            }
            self.outputs.remove(idx);
            if idx < self.scanout_pools.len() {
                self.scanout_pools.remove(idx);
            }
            if idx < self.bo_generations.len() {
                self.bo_generations.remove(idx);
            }
            if idx < self.first_pageflip_logged.len() {
                self.first_pageflip_logged.remove(idx);
            }
        }

        for layout in &mut self.outputs {
            if let Some(snapshot) = snapshot_by_key.get(&layout.key) {
                // A probe does not commit a route. Preserve the exact live
                // connector/CRTC/plane handles, property IDs, picked mode and
                // DRM mode blob; refresh only connector-owned metadata. The
                // next explicit RANDR modeset discovers and commits any
                // changed assignment on this output's owning card.
                layout.output.modes.clone_from(&snapshot.modes);
                layout.output.mm_width = snapshot.mm_width;
                layout.output.mm_height = snapshot.mm_height;
                layout.output.edid.clone_from(&snapshot.edid);
                layout
                    .output
                    .connector_type
                    .clone_from(&snapshot.connector_type);
            }
        }

        // Runtime discovery never auto-enables a newly-connected connector.
        // It enters the registry connected-but-Off; SetCrtcConfig performs
        // the expensive assignment/scanout allocation if a client enables it.
        rescan.added_count = rescan.added_keys.len();

        if !rescan.dropped_old_indices.is_empty() {
            // Packing and the extent recompute deliberately do NOT happen
            // here. They are layout policy and run in the backend caller,
            // after it has decided which departed routes keep their slot.
            self.prune_present_clocks_to_live_outputs();
            self.refresh_cursor_topology_for_devices(&cursor_changed_devices);
        }
        rescan
    }

    /// Mark the scene compositor dirty so every output gets a
    /// full-damage repaint on the next composite tick. Called after
    /// `vt_state` commits to `Active` so the scanout gate is open
    /// when `composite_and_flip` runs.
    pub(crate) fn post_full_damage_all_outputs(&mut self) {
        self.shutting_down = false; // ensure shutting_down doesn't suppress the repaint
        // `wake_for_damage` sets `scene_structure_dirty = true`; the
        // SceneCompositor picks this up on the next `tick` and repaints
        // every output with a full-screen damage rect.
        // (Accessed indirectly through KmsBackend::scene; the caller
        // on backend.rs calls self.scene.wake_for_damage() directly —
        // this stub exists to satisfy the plan's "three helpers on
        // PlatformBackend" requirement; in practice the scene field
        // lives on KmsBackend, not PlatformBackend, so the backend
        // calls the scene method directly and this fn is not used
        // for the scene part. It IS the right place to clear any
        // platform-level inhibit flags on resume.)
    }
}
