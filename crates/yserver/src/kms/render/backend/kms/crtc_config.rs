use crate::{
    internal_probe::{ProbeKmsHandles, RouteProbeRequest},
    kms::{
        backend::OutputKey,
        render::{
            backend::{
                ConnectorConfig, CrtcConfigProbeExecutor, CrtcConfigProbeJob, KmsBackend,
                PendingCrtcConfigProbe, RandrProviderEndpoint,
            },
            platform::{CrtcKey, is_terminal_disposable_probe_error},
        },
        scanout_route::{RenderKmsRelationship, ScanoutRoute},
    },
};
use std::{collections::HashSet, io, os::fd::AsFd as _, rc::Rc};
use yserver_core::backend::{Backend as _, CrtcConfigApply, CrtcConfigToken};

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
    pub(super) fn pending_pageflip_crtcs(&self) -> HashSet<CrtcKey> {
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

    pub(super) fn quiesce_before_topology_mutation(
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

    fn recover_failed_crtc_config(
        &mut self,
        restore_old_topology: bool,
        original: io::Error,
    ) -> io::Error {
        let scene_recovery_failed = if let Err(scene_error) = self
            .scene
            .rebuild_outputs(&self.platform, &self.platform.scene_output_layouts())
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

    pub(super) fn scanout_m1_topology_signature(&self) -> u64 {
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

    pub(super) fn wake_crtc_config_ready(&self) {
        let Some(sender) = self.input_sender.as_ref() else {
            return;
        };
        if let Err(error) = sender.send(yserver_core::core_loop::Message::CrtcConfigReady) {
            log::warn!("failed to wake core for invalidated CRTC configuration: {error}");
        }
    }

    fn remove_crtc_config_ready_announcement(&mut self, token: CrtcConfigToken) {
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

    fn crtc_enable_needs_async_qualification(
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

    fn enqueue_prepared_crtc_config_probe(
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

    fn discover_crtc_config_output(
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

    fn stale_crtc_config_probe_reason(&self, pending: &PendingCrtcConfigProbe) -> Option<String> {
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

impl KmsBackend {
    pub(in crate::kms::render::backend) fn backend_randr_begin_crtc_config(
        &mut self,
        output_id: u32,
        connector: &str,
        mode: Option<yserver_core::backend::ModeSpec>,
        x: i32,
        y: i32,
    ) -> io::Result<CrtcConfigApply> {
        // Until a worker/helper transport is installed, preserve the existing
        // synchronous backend behavior exactly. Disables and same-device
        // changes also have no disposable PRIME qualification to move away
        // from the core thread.
        let Some(mode_spec) = mode else {
            return self
                .apply_crtc_config(output_id, connector, mode, x, y)
                .map(CrtcConfigApply::Applied);
        };
        if self.crtc_config_probe_executor.is_none() {
            return self
                .apply_crtc_config(output_id, connector, mode, x, y)
                .map(CrtcConfigApply::Applied);
        }

        let output_key = self
            .output_key_by_id
            .get(&output_id)
            .cloned()
            .ok_or_else(|| io::Error::other(format!("unknown RANDR output id {output_id}")))?;
        if output_key.connector_name != connector {
            return Err(io::Error::other(format!(
                "RANDR output {output_id} name mismatch: registry has {}, request resolved {connector}",
                output_key.connector_name
            )));
        }

        // Match apply_crtc_config's policy ordering: an idempotent request may
        // not silently reassert a split output after its provider association
        // was detached while another client was active.
        if !self.provider_output_source_allows(output_key.device_key) {
            let sink_endpoint = RandrProviderEndpoint::Kms(output_key.device_key);
            let sink_provider = self.randr_id_alloc.providers.get(&sink_endpoint).copied();
            let selected_source = self.selected_render_provider_endpoint();
            let source_provider = selected_source
                .and_then(|endpoint| self.randr_id_alloc.providers.get(&endpoint).copied());
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "output {connector} belongs to KMS sink provider {} ({sink_endpoint:?}); attach it to selected source provider {} ({selected_source:?}) with RANDR SetProviderOutputSource before enabling it",
                    sink_provider.map_or_else(|| "<unknown>".to_string(), |id| id.to_string()),
                    source_provider.map_or_else(|| "<none>".to_string(), |id| id.to_string()),
                ),
            ));
        }

        let requested = ConnectorConfig::Enabled {
            mode_w: mode_spec.width,
            mode_h: mode_spec.height,
            vrefresh: mode_spec.vrefresh,
            x,
            y,
        };
        let current = self
            .platform
            .outputs
            .iter()
            .find(|layout| layout.key == output_key)
            .map_or(ConnectorConfig::Off, |layout| ConnectorConfig::Enabled {
                mode_w: layout.width,
                mode_h: layout.height,
                vrefresh: layout.output.picked.vrefresh,
                x: layout.x,
                y: layout.y,
            });
        if current == requested {
            return self
                .apply_crtc_config(output_id, connector, mode, x, y)
                .map(CrtcConfigApply::Applied);
        }

        if self
            .platform
            .gpu
            .vk
            .as_ref()
            .is_some_and(|vk| vk.is_software_rasterizer())
            && std::env::var_os("YSERVER_ALLOW_SOFTWARE_VULKAN").is_none()
        {
            return Err(io::Error::other(format!(
                "begin_crtc_config: refusing to enable {connector} with a software Vulkan \
                 renderer; install a hardware Vulkan driver or set \
                 YSERVER_ALLOW_SOFTWARE_VULKAN=1 for a deliberate software-scanout setup"
            )));
        }
        if self.vt_state != crate::vt::state::VtState::Active {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                format!(
                    "begin_crtc_config: cannot qualify {connector} while VT is {:?}",
                    self.vt_state
                ),
            ));
        }

        let route = self.platform.scanout_route_for_kms(output_key.device_key)?;
        if !self.crtc_enable_needs_async_qualification(&output_key, mode_spec, route) {
            return self
                .apply_crtc_config(output_id, connector, mode, x, y)
                .map(CrtcConfigApply::Applied);
        }

        let output_device = Rc::clone(
            &self
                .platform
                .device_for_output(&output_key)
                .ok_or_else(|| {
                    io::Error::other(format!(
                        "RANDR output {output_id} belongs to unavailable DRM device {}",
                        output_key.device_key
                    ))
                })?
                .device,
        );
        // Discovery and advertised-mode validation are deliberately completed
        // while the old topology is still lit. The live DRM output stays in
        // the pending entry; the executor receives one owned KMS-fd duplicate
        // plus a scalar route request.
        let prepared_output =
            self.discover_crtc_config_output(&output_key, &output_device, connector)?;
        if prepared_output.connector_name != connector {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "begin_crtc_config: discovery returned connector {} for requested {connector}",
                    prepared_output.connector_name
                ),
            ));
        }
        if !prepared_output.modes.iter().any(|candidate| {
            candidate.width == mode_spec.width
                && candidate.height == mode_spec.height
                && candidate.vrefresh == mode_spec.vrefresh
        }) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "connector {connector}: mode {}x{}@{} not in advertised list",
                    mode_spec.width, mode_spec.height, mode_spec.vrefresh
                ),
            ));
        }

        let token = self.enqueue_prepared_crtc_config_probe(
            output_id,
            output_key,
            connector.to_string(),
            mode_spec,
            x,
            y,
            prepared_output,
            route,
        )?;
        Ok(CrtcConfigApply::Pending(token))
    }

    pub(in crate::kms::render::backend) fn backend_randr_drain_ready_crtc_configs(
        &mut self,
    ) -> Vec<CrtcConfigToken> {
        let completions = self
            .crtc_config_probe_executor
            .as_mut()
            .map(|executor| executor.drain_ready())
            .unwrap_or_default();
        for completion in completions {
            if !self
                .pending_crtc_config_probes
                .contains_key(&completion.token)
            {
                // Cancellation may race worker completion. The result is
                // resource-free, so dropping it is sufficient; still notify
                // the executor so it can retire transport bookkeeping.
                if let Some(executor) = self.crtc_config_probe_executor.as_mut() {
                    executor.cancel(completion.token);
                }
                continue;
            }
            if self
                .ready_crtc_config_results
                .contains_key(&completion.token)
            {
                log::debug!(
                    "asynchronous CRTC qualifier returned late/duplicate token {:?}; ignoring it",
                    completion.token
                );
                continue;
            }
            self.ready_crtc_config_results
                .insert(completion.token, completion.result);
            self.ready_crtc_config_announcements
                .push_back(completion.token);
        }
        self.ready_crtc_config_announcements.drain(..).collect()
    }

    pub(in crate::kms::render::backend) fn backend_randr_finish_crtc_config(
        &mut self,
        token: CrtcConfigToken,
    ) -> io::Result<bool> {
        self.remove_crtc_config_ready_announcement(token);
        self.invalidated_crtc_config_probes.remove(&token);
        let result = match self.ready_crtc_config_results.remove(&token) {
            Some(result) => result,
            None if self.pending_crtc_config_probes.contains_key(&token) => {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!("asynchronous CRTC configuration {token:?} is not ready"),
                ));
            }
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("unknown asynchronous CRTC configuration token {token:?}"),
                ));
            }
        };
        let Some(mut pending) = self.pending_crtc_config_probes.remove(&token) else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("orphaned asynchronous CRTC result for token {token:?}"),
            ));
        };
        if let Some(executor) = self.crtc_config_probe_executor.as_mut() {
            executor.cancel(token);
        }

        let stale_error = |stage: &str, reason: String| {
            io::Error::new(
                io::ErrorKind::Interrupted,
                format!("asynchronous CRTC configuration {token:?} became stale {stage}: {reason}"),
            )
        };
        if let Some(reason) = self.stale_crtc_config_probe_reason(&pending) {
            return Err(stale_error("before exact-plan replay", reason));
        }
        // Worker failures are terminal for this request but have not touched
        // the live topology. In particular, an indeterminate disposable probe
        // never enters quiesce/recovery on the core thread.
        let qualified = result?;
        let prepared_output = pending
            .prepared_output
            .take()
            .expect("pending CRTC qualification owns its discovered output");

        // Exact live allocation and TEST_ONLY happen while the old topology
        // is still scanning out. The returned object is opaque but owns all
        // uncommitted live resources, so an error or stale result can drop it
        // safely without a blackout or partial installation.
        let prepared = self.platform.prepare_qualified_connector_plan(
            &pending.output_key,
            prepared_output,
            pending.mode,
            pending.x,
            pending.y,
            qualified,
        )?;
        if let Some(reason) = self.stale_crtc_config_probe_reason(&pending) {
            return Err(stale_error("during exact-plan replay", reason));
        }

        let restore_old_on_failure = pending.was_active;
        self.quiesce_before_topology_mutation("asynchronous RANDR CRTC configuration changed")?;
        if let Err(error) = self.platform.install_prepared_connector_plan(prepared) {
            log::error!(
                "finish_crtc_config: installing qualified plan for {} failed: {error}",
                pending.connector
            );
            return Err(self.recover_failed_crtc_config(restore_old_on_failure, error));
        }

        {
            let entry = self.randr_id_alloc.entry_mut(&pending.output_key);
            entry.config = ConnectorConfig::Enabled {
                mode_w: pending.mode.width,
                mode_h: pending.mode.height,
                vrefresh: pending.mode.vrefresh,
                x: pending.x,
                y: pending.y,
            };
            entry.client_configured = true;
            entry.connected = true;
            // The client has placed this output itself; the remembered route
            // and its reserved slot are released.
            entry.last_enabled = None;
        }
        log::info!(
            "finish_crtc_config: enabled {} {}x{}@{} at ({},{}) with qualified plan",
            pending.connector,
            pending.mode.width,
            pending.mode.height,
            pending.mode.vrefresh,
            pending.x,
            pending.y,
        );

        self.prune_armed_targets_to_live_outputs();
        let desired_active = kms_outputs_active_after_crtc_config(
            restore_old_on_failure,
            true,
            self.platform.outputs.len(),
        );
        if let Err(error) = self
            .scene
            .rebuild_outputs(&self.platform, &self.platform.scene_output_layouts())
        {
            log::error!(
                "finish_crtc_config: scene rebuild failed after topology change: {error:?}"
            );
            let error = io::Error::other(format!(
                "finish_crtc_config: scene rebuild failed: {error:?}"
            ));
            let relight = self.relight_after_direct_teardown(
                desired_active,
                "asynchronous RANDR CRTC scene-rebuild failure",
            );
            self.kms_outputs_active = false;
            self.request_exit();
            return Err(relight.err().unwrap_or(error));
        }
        self.relight_after_direct_teardown(
            desired_active,
            "asynchronous RANDR CRTC configuration",
        )?;
        self.kms_outputs_active = desired_active;
        self.update_input_extent(self.platform.fb_w, self.platform.fb_h);
        self.scene.wake_for_damage();
        Ok(true)
    }

    pub(in crate::kms::render::backend) fn backend_randr_cancel_crtc_config(
        &mut self,
        token: CrtcConfigToken,
    ) {
        self.remove_crtc_config_ready_announcement(token);
        self.invalidated_crtc_config_probes.remove(&token);
        self.pending_crtc_config_probes.remove(&token);
        self.ready_crtc_config_results.remove(&token);
        if let Some(executor) = self.crtc_config_probe_executor.as_mut() {
            executor.cancel(token);
        }
    }

    pub(in crate::kms::render::backend) fn backend_randr_apply_crtc_config(
        &mut self,
        output_id: u32,
        connector: &str,
        mode: Option<yserver_core::backend::ModeSpec>,
        x: i32,
        y: i32,
    ) -> io::Result<bool> {
        let output_key = self
            .output_key_by_id
            .get(&output_id)
            .cloned()
            .ok_or_else(|| io::Error::other(format!("unknown RANDR output id {output_id}")))?;
        if output_key.connector_name != connector {
            return Err(io::Error::other(format!(
                "RANDR output {output_id} name mismatch: registry has {}, request resolved {connector}",
                output_key.connector_name
            )));
        }
        let output_device = Rc::clone(
            &self
                .platform
                .device_for_output(&output_key)
                .ok_or_else(|| {
                    io::Error::other(format!(
                        "RANDR output {output_id} belongs to unavailable DRM device {}",
                        output_key.device_key
                    ))
                })?
                .device,
        );

        // Provider policy authorizes only the attempt. Exact real-operation
        // DMA-BUF allocation/import/render/TEST_ONLY probing remains in
        // `enable_connector`; capability metadata there is diagnostic only.
        // Place this before the idempotency guard so an
        // already-active split output can never be silently reasserted under a
        // missing/stale policy. Production startup auto-associates every
        // distinct sink, while an explicit later detach remains persistent.
        if mode.is_some() && !self.provider_output_source_allows(output_key.device_key) {
            let sink_endpoint = RandrProviderEndpoint::Kms(output_key.device_key);
            let sink_provider = self.randr_id_alloc.providers.get(&sink_endpoint).copied();
            let selected_source = self.selected_render_provider_endpoint();
            let source_provider = selected_source
                .and_then(|endpoint| self.randr_id_alloc.providers.get(&endpoint).copied());
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "output {connector} belongs to KMS sink provider {} ({sink_endpoint:?}); attach it to selected source provider {} ({selected_source:?}) with RANDR SetProviderOutputSource before enabling it",
                    sink_provider.map_or_else(|| "<unknown>".to_string(), |id| id.to_string()),
                    source_provider.map_or_else(|| "<none>".to_string(), |id| id.to_string()),
                ),
            ));
        }

        // ── Idempotency guard (CRITICAL) ──────────────────────────────────
        //
        // MATE / mate-settings-daemon re-assert the SAME SetCrtcConfig many
        // times in a row (bursts of identical requests). Every call here used
        // to run a full quiesce + modeset + scene rebuild + repaint, which on
        // a steady-state desktop hammers the CRTC back-to-back → constant
        // flicker/tearing (observed single-screen, immediate zap). Compare the
        // request against the ACTUAL current scanout state (`platform.outputs`
        // is the source of truth) and no-op when nothing changed, so only a
        // genuine mode/position/on-off change pays the modeset cost.
        let requested = match mode {
            None => ConnectorConfig::Off,
            Some(m) => ConnectorConfig::Enabled {
                mode_w: m.width,
                mode_h: m.height,
                vrefresh: m.vrefresh,
                x,
                y,
            },
        };
        let current = self
            .platform
            .outputs
            .iter()
            .find(|layout| layout.key == output_key)
            .map_or(ConnectorConfig::Off, |l| ConnectorConfig::Enabled {
                mode_w: l.width,
                mode_h: l.height,
                vrefresh: l.output.picked.vrefresh,
                x: l.x,
                y: l.y,
            });
        if current == requested {
            log::debug!(
                "apply_crtc_config: {connector} already at requested config ({requested:?}); no-op"
            );
            // Keep the registry's current-config view in sync (cheap) without
            // touching the hardware. Return `false` = nothing changed, so the
            // handler skips the change-notify (Xorg RRTellChanged only fires
            // on a real change) — this is what breaks MATE's re-assert loop.
            let entry = self.randr_id_alloc.entry_mut(&output_key);
            entry.config = requested;
            // A client asserting a config is an explicit statement of intent
            // about this output, so it releases any remembered route (and
            // with it the reserved slot). Never resurrect a route the client
            // has spoken for.
            entry.last_enabled = None;
            return Ok(false);
        }

        // An opened card may have started with no connected outputs, in which
        // case software Vulkan is valid for headless X rendering. Refuse the
        // first later RANDR scanout enable unless the same explicit override
        // accepted by startup is present; exporting a software-Vulkan BO to
        // real KMS can hard-hang the machine.
        if mode.is_some()
            && self
                .platform
                .gpu
                .vk
                .as_ref()
                .is_some_and(|vk| vk.is_software_rasterizer())
            && std::env::var_os("YSERVER_ALLOW_SOFTWARE_VULKAN").is_none()
        {
            return Err(io::Error::other(format!(
                "apply_crtc_config: refusing to enable {connector} with a software Vulkan \
                 renderer; install a hardware Vulkan driver or set \
                 YSERVER_ALLOW_SOFTWARE_VULKAN=1 for a deliberate software-scanout setup"
            )));
        }

        // Resolve the requested connector before taking the old CRTC set
        // offline. A pure discovery failure must not blank a working desktop.
        // Pool allocation and the actual modeset remain in `enable_connector`
        // after quiescing, where failures can restore the old composed set.
        let prepared_output = if mode.is_some() {
            let reserved_routes: Vec<_> = self
                .platform
                .outputs
                .iter()
                .filter(|layout| {
                    layout.key.device_key == output_key.device_key && layout.key != output_key
                })
                .map(|layout| {
                    (
                        layout.output.encoder,
                        layout.output.crtc,
                        layout.output.plane,
                    )
                })
                .collect();
            Some(
                crate::platform::drm::discover_output_for_connector(
                    &output_device,
                    connector,
                    &reserved_routes,
                )
                .map_err(|e| {
                    log::error!("apply_crtc_config: target discovery for {connector} failed: {e}");
                    e
                })?,
            )
        } else {
            None
        };

        // ── Flip-safety: quiesce the complete old topology ────────────────
        //
        // Both enable and disable modify `platform.outputs` (topology),
        // so we need `drain_all` + `rebuild_outputs` — the same path
        // `fire_randr_changes` uses for hotplug.  The sequence below:
        //
        //   all CRTCs off      — proves no survivor still references a BO
        //                       whose userspace phase is about to be reset.
        //   wait/drain/reset   — retires GPU work and clears both the scene
        //                       ack ledger and matching platform BO phases.
        //   platform mutate   — disable_connector / enable_connector
        //   rebuild + relight — restores every surviving/new active CRTC.
        //
        // The `commit_modeset` / `disable_output` calls in the platform
        // helpers are ALLOW_MODESET atomic commits (not page-flips), so
        // they are always legal after drain_all.  After rebuild_outputs
        // the scene's `pending_acks` is fresh-empty for every output, so
        // the subsequent `wake_for_damage` tick is EBUSY-safe.
        let restore_old_on_failure = self.kms_outputs_active;
        self.quiesce_before_topology_mutation("RANDR CRTC configuration changed")?;

        match mode {
            None => {
                // ── Disable path ─────────────────────────────────────────
                if self.platform.remove_connector_after_all_off(&output_key) {
                    log::info!("apply_crtc_config: disabled {connector}");
                } else {
                    // Already off — still update the registry.
                    log::debug!("apply_crtc_config: {connector} was already off");
                }
                // Update registry: connector stays known, config → Off.
                {
                    let entry = self.randr_id_alloc.entry_mut(&output_key);
                    entry.config = ConnectorConfig::Off;
                    entry.crtc_associated = false;
                    // client_configured is set to record that a client
                    // explicitly disabled this output (not an auto-layout op).
                    entry.client_configured = true;
                    // An explicit disable must not be undone by a later
                    // auto-relight (invariant 6): unplugging a deliberately
                    // disabled monitor may not resurrect it.
                    entry.last_enabled = None;
                }
            }
            Some(mode_spec) => {
                // ── Enable / mode-change path ────────────────────────────
                let output = prepared_output.expect("enabled request prepared its connector");

                // enable_connector handles: mode resolution, pool
                // (re)alloc, commit_modeset, ActiveOutput update,
                // fb extent recompute.
                if let Err(e) = self
                    .platform
                    .enable_connector(&output_key, output, mode_spec, x, y)
                {
                    log::error!("apply_crtc_config: enable_connector({connector}) failed: {e}");
                    if is_terminal_disposable_probe_error(&e) {
                        // The old topology is already quiesced and dark. A
                        // terminal probe failure deliberately retains GPU
                        // owners; rebuilding the scene here would drop the old
                        // composite rings and re-enter vkDeviceWaitIdle on the
                        // same physical GPU. Re-commit only the unchanged old
                        // KMS framebuffers, without rebuilding or dropping any
                        // Vulkan owner, then return to the core loop so
                        // input/VT handling remains responsive.
                        if restore_old_on_failure {
                            match self.platform.dpms_set_outputs_active(true) {
                                Ok(()) => {
                                    self.reapply_gamma_for_live_outputs();
                                    self.kms_outputs_active = !self.platform.outputs.is_empty();
                                    log::error!(
                                        "apply_crtc_config: terminal disposable probe failure; \
                                         restored the unchanged old KMS topology and skipped \
                                         Vulkan teardown/recovery"
                                    );
                                }
                                Err(relight_error) => {
                                    self.kms_outputs_active = false;
                                    log::error!(
                                        "apply_crtc_config: terminal disposable probe failure; \
                                         old KMS topology relight also failed: {relight_error}; \
                                         skipped Vulkan teardown/recovery"
                                    );
                                }
                            }
                        } else {
                            self.kms_outputs_active = false;
                            log::error!(
                                "apply_crtc_config: terminal disposable probe failure from a \
                                 previously headless topology; skipped Vulkan teardown/recovery"
                            );
                        }
                        return Err(e);
                    }
                    return Err(self.recover_failed_crtc_config(restore_old_on_failure, e));
                }

                // Update registry.
                {
                    let entry = self.randr_id_alloc.entry_mut(&output_key);
                    entry.config = ConnectorConfig::Enabled {
                        mode_w: mode_spec.width,
                        mode_h: mode_spec.height,
                        vrefresh: mode_spec.vrefresh,
                        x,
                        y,
                    };
                    entry.crtc_associated = true;
                    entry.client_configured = true;
                    entry.connected = true;
                    // The client has placed this output itself; the
                    // remembered route and its reserved slot are released.
                    entry.last_enabled = None;
                }

                log::info!(
                    "apply_crtc_config: enabled {connector} {}×{}@{} at ({x},{y})",
                    mode_spec.width,
                    mode_spec.height,
                    mode_spec.vrefresh
                );
            }
        }

        self.prune_armed_targets_to_live_outputs();

        // Decide whether the new topology should be lit. A first enable from
        // headless opens the gate; disabling while the old topology was dark
        // keeps surviving outputs dark.
        let desired_active = kms_outputs_active_after_crtc_config(
            restore_old_on_failure,
            matches!(requested, ConnectorConfig::Enabled { .. }),
            self.platform.outputs.len(),
        );

        // ── Scene + RANDR rebuild ─────────────────────────────────────────
        if let Err(e) = self
            .scene
            .rebuild_outputs(&self.platform, &self.platform.scene_output_layouts())
        {
            log::error!("apply_crtc_config: scene rebuild failed after topology change: {e:?}");
            let error = io::Error::other(format!("apply_crtc_config: scene rebuild failed: {e:?}"));
            let relight = self
                .relight_after_direct_teardown(desired_active, "RANDR CRTC scene-rebuild failure");
            // Hardware/platform/registry state has already changed, but the
            // core RANDR projection cannot be rebuilt consistently. Rollback
            // would itself require another fallible modeset, so fail-stop
            // instead of continuing with two contradictory topologies.
            self.kms_outputs_active = false;
            self.request_exit();
            return Err(relight.err().unwrap_or(error));
        }
        self.relight_after_direct_teardown(desired_active, "RANDR CRTC configuration")?;
        self.kms_outputs_active = desired_active;

        // Update input extent (cursor clamp) to reflect new fb size.
        let (new_fb_w, new_fb_h) = (self.platform.fb_w, self.platform.fb_h);
        self.update_input_extent(new_fb_w, new_fb_h);

        self.scene.wake_for_damage();
        Ok(true)
    }
}
