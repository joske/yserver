use super::*;

/// Reconcile the physical-output gate after a successful CRTC mutation.
///
/// Enabling or reconfiguring an output commits an active mode, so it opens the
/// gate. Disabling the last output closes it. Disabling one output while other
/// outputs remain preserves the prior state: those survivors may already be
/// dark because DPMS is off, and an unrelated disable must not claim that they
/// were re-lit.
pub(in crate::kms::render::backend) fn kms_outputs_active_after_crtc_config(
    was_active: bool,
    request_enabled: bool,
    live_output_count: usize,
) -> bool {
    live_output_count != 0 && (was_active || request_enabled)
}

impl KmsBackend {
    /// Quiesce the old active-output set before a connector snapshot removes
    /// an output or drops its scanout pool.
    ///
    /// A physical disconnect does not prove that the KMS plane stopped
    /// referencing its framebuffer. Keep the old routes and allocations alive
    /// until every CRTC has been disabled successfully. On failure the caller
    /// fail-stops without applying the snapshot, which is safer than dropping
    /// a framebuffer the kernel may still scan out.
    pub(in crate::kms::render::backend) fn pending_pageflip_crtcs(&self) -> HashSet<CrtcKey> {
        self.platform
            .outputs
            .iter()
            .enumerate()
            .filter(|(output_idx, _)| {
                self.platform
                    .scanout_pools
                    .get(*output_idx)
                    .and_then(Option::as_ref)
                    .is_some_and(|pool| pool.display_pool().has_pending_pageflip())
                    || self
                        .scanout_m2
                        .pending
                        .as_ref()
                        .is_some_and(|frame| frame.awaiting_outputs.contains(output_idx))
                    || self.scanout_m2.unflip_awaiting_outputs.contains(output_idx)
            })
            .map(|(_, output)| CrtcKey::for_output(output))
            .collect()
    }

    pub(in crate::kms::render::backend) fn quiesce_before_topology_mutation(
        &mut self,
        context: &'static str,
    ) -> io::Result<()> {
        self.bump_crtc_config_topology_epoch(context);
        let old_pending_pageflips = self.pending_pageflip_crtcs();
        if self.scanout_m2.active() {
            if let Err(error) = self.teardown_direct_before_topology_requery(context) {
                // This also covers failure to materialize the direct source's
                // COW shadow. Consuming the topology event while retaining an
                // unquiesced client framebuffer would leave the session in a
                // permanently stale state, so fail-stop explicitly.
                self.request_exit();
                return Err(error);
            }
        } else {
            self.platform.wait_idle_bounded();
            if let Err(error) = self.platform.dpms_set_outputs_active(false) {
                log::error!(
                    "kms: {context}: could not disable the old output topology: {error}; exiting"
                );
                self.request_exit();
                return Err(error);
            }
            self.clear_all_armed_vblank_targets();
        }

        if let Err(error) = self.platform.discard_old_drm_events_after_all_off(
            &old_pending_pageflips,
            std::time::Duration::from_secs(1),
        ) {
            log::error!(
                "kms: {context}: old page-flip events could not be retired safely: {error}; exiting"
            );
            self.request_exit();
            return Err(error);
        }

        // Direct teardown may have submitted the lazy COW copy immediately
        // before disabling scanout. Wait for that work, then retire scene and
        // pool state while the old output indices are still authoritative.
        self.platform.wait_idle_bounded();
        self.scene.drain_all(&mut self.platform);
        if let Err(error) = self.platform.reset_scanout_bos_for_suspend() {
            self.kms_outputs_active = false;
            log::error!(
                "kms: {context}: copied scanout devices could not be quiesced after all outputs \
                 were disabled: {error}; preserving quarantine and exiting"
            );
            self.request_exit();
            return Err(error);
        }
        self.scanout_m1.clear(context);
        Ok(())
    }

    pub(in crate::kms::render::backend) fn recover_failed_crtc_config(
        &mut self,
        restore_old_topology: bool,
        original: io::Error,
    ) -> io::Error {
        let scene_recovery_failed = if let Err(scene_error) =
            self.scene.rebuild_outputs(&self.platform)
        {
            log::error!(
                "apply_crtc_config: scene recovery after failure also failed: {scene_error:?}; exiting"
            );
            self.kms_outputs_active = false;
            self.request_exit();
            true
        } else {
            false
        };
        match self.relight_after_direct_teardown(
            restore_old_topology,
            "RANDR CRTC configuration recovery",
        ) {
            Ok(()) => {
                if !scene_recovery_failed {
                    self.kms_outputs_active =
                        restore_old_topology && !self.platform.outputs.is_empty();
                }
                original
            }
            Err(relight_error) => relight_error,
        }
    }

    pub(in crate::kms::render::backend) fn scanout_m1_topology_signature(&self) -> u64 {
        use std::hash::{Hash, Hasher};

        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.platform.fb_w.hash(&mut hasher);
        self.platform.fb_h.hash(&mut hasher);
        for layout in &self.platform.outputs {
            layout.key.device_key.hash(&mut hasher);
            layout.x.hash(&mut hasher);
            layout.y.hash(&mut hasher);
            layout.width.hash(&mut hasher);
            layout.height.hash(&mut hasher);
            u32::from(layout.output.crtc).hash(&mut hasher);
            u32::from(layout.output.plane).hash(&mut hasher);
            layout.output.picked.clock_khz.hash(&mut hasher);
            layout.output.picked.htotal.hash(&mut hasher);
            layout.output.picked.vtotal.hash(&mut hasher);
            layout.output.picked.vscan.hash(&mut hasher);
            layout.output.picked.flags.hash(&mut hasher);
        }
        hasher.finish()
    }

    fn crtc_config_topology_signature(&self) -> u64 {
        use std::hash::{Hash, Hasher};

        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.scanout_m1_topology_signature().hash(&mut hasher);
        self.kms_outputs_active.hash(&mut hasher);
        for (index, output) in self.platform.outputs.iter().enumerate() {
            output.key.hash(&mut hasher);
            output.scanout_route.hash(&mut hasher);
            self.platform
                .scanout_pools
                .get(index)
                .and_then(Option::as_ref)
                .map(crate::kms::vk::scanout::OutputScanout::route)
                .hash(&mut hasher);
        }
        hasher.finish()
    }

    /// Advance the global qualification snapshot and promptly retire every
    /// request still parked against the previous topology/DRM-master lifetime.
    ///
    /// The pending entry deliberately remains owned by the backend until the
    /// core finishes or cancels it. Cancelling the executor drops queued work;
    /// an already-running process is only logically retired and continues
    /// off-core so its eventual uncertain result can still poison its resource
    /// domain inside the executor.
    pub(in crate::kms::render::backend) fn bump_crtc_config_topology_epoch(
        &mut self,
        reason: &str,
    ) {
        self.crtc_config_topology_epoch = self.crtc_config_topology_epoch.wrapping_add(1);

        let mut tokens: Vec<_> = self.pending_crtc_config_probes.keys().copied().collect();
        tokens.sort_unstable_by_key(|token| token.0);
        let mut announced = false;
        for token in tokens {
            if self.invalidated_crtc_config_probes.contains(&token) {
                continue;
            }
            // A completed worker result may already have been announced but
            // not yet consumed by finish. Replace it so a topology change can
            // never install that stale plan, while avoiding a duplicate wake.
            let already_announced = self.ready_crtc_config_results.contains_key(&token);
            if let Some(executor) = self.crtc_config_probe_executor.as_mut() {
                executor.cancel(token);
            }
            self.ready_crtc_config_results.insert(
                token,
                Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    format!("asynchronous CRTC configuration {token:?} invalidated by {reason}"),
                )),
            );
            self.invalidated_crtc_config_probes.insert(token);
            if !already_announced {
                self.ready_crtc_config_announcements.push_back(token);
                announced = true;
            }
        }
        if announced {
            self.wake_crtc_config_ready();
        }
    }

    pub(in crate::kms::render::backend) fn wake_crtc_config_ready(&self) {
        let Some(sender) = self.input_sender.as_ref() else {
            return;
        };
        if let Err(error) = sender.send(yserver_core::core_loop::Message::CrtcConfigReady) {
            log::warn!("failed to wake core for invalidated CRTC configuration: {error}");
        }
    }

    pub(in crate::kms::render::backend) fn remove_crtc_config_ready_announcement(
        &mut self,
        token: CrtcConfigToken,
    ) {
        self.ready_crtc_config_announcements
            .retain(|announced| *announced != token);
    }

    fn next_crtc_config_token(&mut self) -> CrtcConfigToken {
        loop {
            let token = CrtcConfigToken(self.next_crtc_config_token.max(1));
            self.next_crtc_config_token = token.0.wrapping_add(1).max(1);
            if !self.pending_crtc_config_probes.contains_key(&token)
                && !self.ready_crtc_config_results.contains_key(&token)
            {
                return token;
            }
        }
    }

    pub(in crate::kms::render::backend) fn crtc_enable_needs_async_qualification(
        &self,
        output_key: &OutputKey,
        mode: yserver_core::backend::ModeSpec,
        route: ScanoutRoute,
    ) -> bool {
        if route.relationship == RenderKmsRelationship::Same {
            return false;
        }
        let existing_idx = self
            .platform
            .outputs
            .iter()
            .position(|output| &output.key == output_key);
        let existing = existing_idx.and_then(|index| self.platform.outputs.get(index));
        let pool_route = existing_idx
            .and_then(|index| self.platform.scanout_pools.get(index))
            .and_then(Option::as_ref)
            .map(crate::kms::vk::scanout::OutputScanout::route);
        existing.is_none_or(|output| {
            output.width != mode.width
                || output.height != mode.height
                || output.scanout_route != route
                || pool_route != Some(route)
        })
    }

    pub(in crate::kms::render::backend) fn enqueue_prepared_crtc_config_probe(
        &mut self,
        output_id: u32,
        output_key: OutputKey,
        connector: String,
        mode: yserver_core::backend::ModeSpec,
        x: i32,
        y: i32,
        prepared_output: crate::platform::drm::Output,
        route: ScanoutRoute,
    ) -> io::Result<CrtcConfigToken> {
        if self.crtc_config_probe_executor.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "no asynchronous CRTC probe executor installed",
            ));
        }
        let token = self.next_crtc_config_token();
        let kms_fd = self
            .platform
            .device_for_output(&output_key)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no KMS device for asynchronous output {output_key:?}"),
                )
            })?
            .device
            .as_fd()
            .try_clone_to_owned()?;
        let (source_selector, copied_sink) = self
            .platform
            .scanout_qualification_devices_for_kms(output_key.device_key)?;
        let request = RouteProbeRequest {
            token,
            mode,
            source_route: route,
            source_selector: source_selector.into(),
            copied_sink: copied_sink.map(Into::into),
            kms: ProbeKmsHandles {
                connector: prepared_output.connector,
                encoder: prepared_output.encoder,
                crtc: prepared_output.crtc,
                plane: prepared_output.plane,
            },
            fence_timeout_ns: crate::kms::render::platform::PRIME_RENDER_PROBE_TIMEOUT_NS,
        };
        let job = CrtcConfigProbeJob { kms_fd, request };
        log::debug!(
            "begin_crtc_config: enqueue token {:?} for {:?} {}x{}@{} at ({},{}), route {:?}, copied_sink={}, fence_timeout_ns={}",
            request.token,
            output_key,
            request.mode.width,
            request.mode.height,
            request.mode.vrefresh,
            x,
            y,
            request.source_route,
            request.copied_sink.is_some(),
            request.fence_timeout_ns,
        );
        let pending = PendingCrtcConfigProbe {
            output_id,
            output_key,
            connector,
            mode,
            x,
            y,
            route,
            prepared_output: Some(prepared_output),
            topology_signature: self.crtc_config_topology_signature(),
            topology_epoch: self.crtc_config_topology_epoch,
            vt_state: self.vt_state,
            was_active: self.kms_outputs_active,
        };
        self.pending_crtc_config_probes.insert(token, pending);
        let enqueue_result = self
            .crtc_config_probe_executor
            .as_mut()
            .expect("executor checked above")
            .enqueue(job);
        if let Err(error) = enqueue_result {
            self.pending_crtc_config_probes.remove(&token);
            self.crtc_config_probe_executor
                .as_mut()
                .expect("executor checked above")
                .cancel(token);
            return Err(error);
        }
        Ok(token)
    }

    pub(in crate::kms::render::backend) fn discover_crtc_config_output(
        &mut self,
        output_key: &OutputKey,
        output_device: &Rc<crate::drm::Device>,
        connector: &str,
    ) -> io::Result<crate::platform::drm::Output> {
        #[cfg(test)]
        if let Some(output) = self.crtc_config_discovery_override.take() {
            return Ok(output);
        }

        let reserved_routes: Vec<_> = self
            .platform
            .outputs
            .iter()
            .filter(|layout| {
                layout.key.device_key == output_key.device_key && layout.key != *output_key
            })
            .map(|layout| {
                (
                    layout.output.encoder,
                    layout.output.crtc,
                    layout.output.plane,
                )
            })
            .collect();
        crate::platform::drm::discover_output_for_connector(
            output_device,
            connector,
            &reserved_routes,
        )
        .map_err(|error| {
            log::error!("begin_crtc_config: target discovery for {connector} failed: {error}");
            error
        })
    }

    pub(in crate::kms::render::backend) fn stale_crtc_config_probe_reason(
        &self,
        pending: &PendingCrtcConfigProbe,
    ) -> Option<String> {
        if !self.scanout_allowed() {
            return Some(format!(
                "VT/DRM-master state is {:?}, not Active",
                self.vt_state
            ));
        }
        if self.vt_state != pending.vt_state {
            return Some(format!(
                "VT state changed from {:?} to {:?}",
                pending.vt_state, self.vt_state
            ));
        }
        if self.crtc_config_topology_epoch != pending.topology_epoch {
            return Some(format!(
                "topology epoch changed from {} to {}",
                pending.topology_epoch, self.crtc_config_topology_epoch
            ));
        }
        if self.crtc_config_topology_signature() != pending.topology_signature {
            return Some("live output topology changed".to_string());
        }
        if self.output_key_by_id.get(&pending.output_id) != Some(&pending.output_key) {
            return Some(format!(
                "RANDR output {} no longer names {:?}",
                pending.output_id, pending.output_key
            ));
        }
        let Some(entry) = self.randr_id_alloc.entry(&pending.output_key) else {
            return Some(format!("connector {} left the registry", pending.connector));
        };
        if !entry.connected {
            return Some(format!("connector {} disconnected", pending.connector));
        }
        if !entry.modes.iter().any(|candidate| {
            candidate.width == pending.mode.width
                && candidate.height == pending.mode.height
                && candidate.vrefresh == pending.mode.vrefresh
        }) {
            return Some(format!(
                "connector {} no longer advertises {}x{}@{}",
                pending.connector, pending.mode.width, pending.mode.height, pending.mode.vrefresh
            ));
        }
        if !self.provider_output_source_allows(pending.output_key.device_key) {
            return Some(format!(
                "PRIME Output Source policy for {} was detached",
                pending.output_key.device_key
            ));
        }
        match self
            .platform
            .scanout_route_for_kms(pending.output_key.device_key)
        {
            Ok(route) if route == pending.route => None,
            Ok(route) => Some(format!(
                "scanout route changed from {:?} to {:?}",
                pending.route, route
            )),
            Err(error) => Some(format!("scanout route is no longer available: {error}")),
        }
    }

    /// Install the process-helper adapter that owns qualification execution.
    /// Kept as a narrow injection seam so helper transport can land without
    /// exposing backend internals or changing the core-facing token contract.
    #[allow(dead_code)]
    pub(crate) fn set_crtc_config_probe_executor(
        &mut self,
        mut executor: Box<dyn CrtcConfigProbeExecutor>,
    ) {
        if let Some(sender) = self.input_sender.as_ref() {
            executor.set_core_sender(sender.clone_handle());
        }
        self.crtc_config_probe_executor = Some(executor);
    }
}
