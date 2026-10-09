use super::*;

pub(in crate::kms::render::backend) fn reconcile_connector_probe(
    randr_id_alloc: &mut RandrIdAllocator,
    device_key: crate::platform::drm::DrmDeviceKey,
    probes: &[crate::platform::drm::ConnectorProbe],
) -> ConnectorRegistryDelta {
    let mut delta = ConnectorRegistryDelta::default();
    let mut seen: HashSet<OutputKey> = HashSet::new();
    for probe in probes {
        let output_key = OutputKey::new(device_key, probe.connector_name.clone());
        seen.insert(output_key.clone());
        let newly_known = randr_id_alloc.entry(&output_key).is_none();
        let entry = randr_id_alloc.entry_mut(&output_key);
        // A dynamically-created connector resource changes the available
        // RANDR output set even when it first appears disconnected.
        let mut entry_changed = newly_known;
        let mut config_changed = newly_known;
        if entry.connected != probe.connected {
            entry.connected = probe.connected;
            entry_changed = true;
            config_changed = true;
        }
        // Retain the last-known mode list while disconnected, matching the
        // registry's existing hot-unplug semantics.
        if probe.connected && entry.modes != probe.modes {
            entry.modes.clone_from(&probe.modes);
            // A changed advertised list can identify a replacement monitor,
            // but the lightweight query deliberately does not read EDID or
            // dimensions. Do not expose the departed monitor's identity
            // before the next heavy physical-topology snapshot.
            entry.edid.clear();
            entry.mm_width = 0;
            entry.mm_height = 0;
            entry_changed = true;
            config_changed = true;
        }
        if !probe.connected {
            entry_changed |= !entry.edid.is_empty() || entry.mm_width != 0 || entry.mm_height != 0;
            entry.edid.clear();
            entry.mm_width = 0;
            entry.mm_height = 0;
        }
        if entry_changed {
            delta.changed_keys.push(output_key);
        }
        delta.config_changed |= config_changed;
    }
    // Connectors the registry knows but the probe no longer sees are
    // disconnected. Their mode lists and live/client configuration are
    // retained; only the later heavy topology path may detach a CRTC.
    let known: Vec<OutputKey> = randr_id_alloc
        .known_connectors()
        .into_iter()
        .map(|(key, _)| key)
        .filter(|key| key.device_key == device_key)
        .collect();
    for key in known {
        if !seen.contains(&key) {
            let entry = randr_id_alloc.entry_mut(&key);
            let mut entry_changed = false;
            let mut config_changed = false;
            if entry.connected {
                entry.connected = false;
                entry_changed = true;
                config_changed = true;
            }
            entry_changed |= !entry.edid.is_empty() || entry.mm_width != 0 || entry.mm_height != 0;
            entry.edid.clear();
            entry.mm_width = 0;
            entry.mm_height = 0;
            if entry_changed {
                delta.changed_keys.push(key.clone());
            }
            delta.config_changed |= config_changed;
        }
    }
    delta
}

/// Probe whether this Vulkan context can allocate and export a BGRA8
/// dma-buf image.  Called ONCE at backend construction; result is cached
/// in `KmsBackend::dmabuf_export_supported`.
///
/// Allocates a 1×1 `BGRA8` external-memory image and immediately
/// exports it.  Both the image and the fd are dropped at the end of
/// this function (`ExportableImage::Drop` destroys image + memory,
/// `DmabufExport::fd` is an `OwnedFd` that closes on drop), so no
/// resources leak from the probe.
/// Real kernel timing → RANDR `ModeInfo` (Xorg `drmmode_ConvertFromKMode`
/// parity), so the reported dot clock / blanking reproduce the exact
/// fractional refresh (e.g. 59.95) GNOME/MATE store in `monitors.xml`.
/// `clock_khz == 0` (synthetic/nested modes) → `None`, so the RANDR layer
/// falls back to synthesised blanking. The low 6 `DRM_MODE_FLAG_*` bits
/// (sync polarity / interlace / doublescan) coincide with the RANDR
/// `RR_*` flag bits; higher DRM-only bits are masked off.
pub(in crate::kms::render::backend) fn mode_timing(
    m: &crate::platform::drm::Mode,
) -> Option<yserver_core::randr::ModeTiming> {
    if m.clock_khz == 0 {
        return None;
    }
    Some(yserver_core::randr::ModeTiming {
        clock_khz: m.clock_khz,
        hsync_start: m.hsync_start,
        hsync_end: m.hsync_end,
        htotal: m.htotal,
        vsync_start: m.vsync_start,
        vsync_end: m.vsync_end,
        vtotal: m.vtotal,
        mode_flags: m.flags & 0x3F,
    })
}

/// Compare the effective vertical rates without collapsing fractional timings
/// to `Mode::vrefresh`'s integer. Kernel mode refresh is pixel clock divided by
/// horizontal and vertical totals, doubled for interlace and halved for
/// doublescan. Synthetic/test modes fall back to the advertised integer rate.
pub(in crate::kms::render::backend) fn effective_refresh_matches(
    a: &crate::platform::drm::Mode,
    b: &crate::platform::drm::Mode,
) -> bool {
    const DRM_MODE_FLAG_INTERLACE: u32 = 1 << 4;
    const DRM_MODE_FLAG_DBLSCAN: u32 = 1 << 5;

    fn ratio(mode: &crate::platform::drm::Mode) -> Option<(u128, u128)> {
        if mode.clock_khz == 0 || mode.htotal == 0 || mode.vtotal == 0 {
            return None;
        }
        let mut numerator = u128::from(mode.clock_khz) * 1_000;
        let mut denominator = u128::from(mode.htotal) * u128::from(mode.vtotal);
        if mode.flags & DRM_MODE_FLAG_INTERLACE != 0 {
            numerator *= 2;
        }
        if mode.flags & DRM_MODE_FLAG_DBLSCAN != 0 {
            denominator *= 2;
        }
        if mode.vscan > 1 {
            denominator *= u128::from(mode.vscan);
        }
        Some((numerator, denominator))
    }

    match (ratio(a), ratio(b)) {
        (Some((a_num, a_den)), Some((b_num, b_den))) => a_num * b_den == b_num * a_den,
        _ => a.vrefresh == b.vrefresh,
    }
}

pub(in crate::kms::render::backend) fn restore_primary_output_after_rebuild(
    previous: u32,
    was_explicit: bool,
    randr: &mut yserver_core::randr::RandrState,
) -> bool {
    if was_explicit
        && (previous == 0
            || randr
                .outputs
                .iter()
                .any(|output| output.output_id == previous))
    {
        randr.primary_output = previous;
        true
    } else {
        false
    }
}

impl KmsBackend {
    /// Virtual-screen extent — mirrors `KmsBackend::fb_dimensions`.
    /// Called by `lib.rs` during the pre-`Box<dyn Backend>` setup
    /// (capability advertisement, `ServerState::with_randr_outputs`).
    #[must_use]
    pub fn fb_dimensions(&self) -> (u16, u16) {
        self.platform.fb_dimensions()
    }

    /// Startup pointer position — centre of the primary output (output 0),
    /// matching Xorg. Used by `lib.rs` to seed the libinput thread's cursor so
    /// it agrees with the core (which is seeded the same way at construction).
    #[must_use]
    pub fn initial_pointer_position(&self) -> (i32, i32) {
        let (fw, fh) = self.fb_dimensions();
        crate::kms::backend::primary_output_center(&self.platform.outputs, fw, fh)
    }

    /// Seed the stable RANDR connector registry from every opened DRM card.
    /// Startup scanout remains limited to the outputs activated during
    /// platform bring-up; connected secondary-card connectors are registered
    /// as available but off until a RANDR client enables them.
    pub(in crate::kms::render::backend) fn seed_initial_connector_topology(
        &mut self,
    ) -> io::Result<()> {
        self.reserve_randr_provider_ids();

        // Preserve historical XID allocation order: active startup outputs
        // receive output/CRTC IDs before inactive secondary connectors.
        let live_keys: Vec<_> = self
            .platform
            .outputs
            .iter()
            .map(|layout| layout.key.clone())
            .collect();
        for key in &live_keys {
            let _ = self.randr_id_alloc.ids_for(key);
        }

        // Keep a lightweight all-connector pass for disconnected connector
        // XIDs/provider inventory, then enrich every connected entry from a
        // forced metadata snapshot. Gather both before mutating the registry,
        // so a failure on card N cannot leave a partially reconciled startup
        // view.
        let probes = self.platform.probe_all_connectors()?;
        let snapshots = self.platform.probe_connector_snapshot()?;
        self.seed_connector_topology_from_probes(&probes, &snapshots);
        Ok(())
    }

    /// Apply already-gathered startup probes. The cached pass reserves stable
    /// XIDs only; the forced connected-only snapshot is authoritative for
    /// connection, modes, and monitor metadata.
    pub(in crate::kms::render::backend) fn seed_connector_topology_from_probes(
        &mut self,
        probes: &[(
            crate::platform::drm::DrmDeviceKey,
            Vec<crate::platform::drm::ConnectorProbe>,
        )],
        snapshots: &[ConnectorSnapshot],
    ) {
        for (device_key, connectors) in probes {
            for connector in connectors {
                let key = OutputKey::new(*device_key, connector.connector_name.clone());
                let _ = self.randr_id_alloc.ids_for(&key);
            }
        }
        // Entries seen only by cached inventory remain default-disconnected.
        let _ = self.reconcile_connector_registry(snapshots, &[], &[]);

        let live_configs: Vec<_> = self
            .platform
            .outputs
            .iter()
            .map(|layout| {
                (
                    layout.key.clone(),
                    ConnectorConfig::Enabled {
                        mode_w: layout.width,
                        mode_h: layout.height,
                        vrefresh: layout.output.picked.vrefresh,
                        x: layout.x,
                        y: layout.y,
                    },
                )
            })
            .collect();
        for (key, config) in live_configs {
            let entry = self.randr_id_alloc.entry_mut(&key);
            if entry.connected && !entry.modes.is_empty() {
                entry.config = config;
                entry.crtc_associated = true;
            }
        }
    }

    /// Reconcile a complete connected-output metadata snapshot from a heavy
    /// physical-topology boundary. Returns connector keys whose advertised
    /// connection, modes, or identity changed, so callers can avoid spurious
    /// RANDR config-timestamp bumps. Retiring an already-light-disconnected
    /// CRTC still clears its internal config below, but is deliberately not a
    /// second advertised-config delta.
    ///
    /// `dropped_layouts` carries the live rectangle of every route the
    /// connector snapshot removed. The registry's own `config` can be stale
    /// after an auto-layout repack (nothing writes `(x, y)` back to it), so
    /// the remembered route is taken from the layout that actually departed.
    pub(in crate::kms::render::backend) fn reconcile_connector_registry(
        &mut self,
        connected: &[ConnectorSnapshot],
        dropped: &[OutputKey],
        dropped_layouts: &[crate::kms::render::platform::DroppedRoute],
    ) -> ConnectorRegistryDelta {
        let mut delta = ConnectorRegistryDelta::default();
        let connected_keys: HashSet<_> = connected.iter().map(|snapshot| &snapshot.key).collect();
        for key in dropped {
            let departed = dropped_layouts
                .iter()
                .find(|route| &route.key == key)
                .cloned();
            let entry = self.randr_id_alloc.entry_mut(key);
            if connected_keys.contains(key) {
                // Physically connected but no longer usable by the live CRTC
                // (for example, its current mode vanished). The connected
                // snapshot below owns advertised state; this arm retires only
                // the route policy, so do not fabricate a disconnect/reconnect
                // pair or a second config timestamp.
                entry.crtc_associated |= matches!(entry.config, ConnectorConfig::Enabled { .. });
                entry.config = ConnectorConfig::Off;
                entry.client_configured = false;
                // Not a physical departure — the connector is still here, it
                // just lost its route. There is nothing to relight on a
                // reconnect edge that will not come, and nothing to reserve.
                entry.last_enabled = None;
                continue;
            }
            // The connector physically departed. Remember the route it was
            // scanning out so the reconnect relights it without a client
            // request, and so its slot stays reserved meanwhile.
            if matches!(entry.config, ConnectorConfig::Enabled { .. })
                && let Some(route) = departed
            {
                entry.last_enabled = Some(ConnectorConfig::Enabled {
                    mode_w: route.width,
                    mode_h: route.height,
                    vrefresh: route.vrefresh,
                    x: route.x,
                    y: route.y,
                });
            }
            let config_changed = entry.connected;
            let output_changed = config_changed
                || !entry.edid.is_empty()
                || entry.mm_width != 0
                || entry.mm_height != 0;
            if output_changed {
                delta.changed_keys.push(key.clone());
            }
            delta.config_changed |= config_changed;
            entry.crtc_associated |= matches!(entry.config, ConnectorConfig::Enabled { .. });
            entry.connected = false;
            entry.config = ConnectorConfig::Off;
            entry.client_configured = false;
            // Retain stable mode resources and connector-owned type, but do
            // not project the departed monitor's identity while disconnected.
            entry.edid.clear();
            entry.mm_width = 0;
            entry.mm_height = 0;
        }
        for snapshot in connected {
            let entry = self.randr_id_alloc.entry_mut(&snapshot.key);
            let config_changed = !entry.connected || entry.modes != snapshot.modes;
            let output_changed = config_changed
                || entry.edid != snapshot.edid
                || entry.mm_width != snapshot.mm_width
                || entry.mm_height != snapshot.mm_height
                || entry.connector_type != snapshot.connector_type;
            if output_changed {
                delta.changed_keys.push(snapshot.key.clone());
            }
            delta.config_changed |= config_changed;
            entry.connected = true;
            entry.modes.clone_from(&snapshot.modes);
            entry.edid.clone_from(&snapshot.edid);
            entry.mm_width = snapshot.mm_width;
            entry.mm_height = snapshot.mm_height;
            entry.connector_type.clone_from(&snapshot.connector_type);
        }
        if !delta.is_empty() {
            self.bump_crtc_config_topology_epoch("connector registry changed");
        }
        delta
    }

    /// Merge cached connector-only results for an explicit RANDR query.
    /// This never mutates live KMS routing, but a changed client-visible
    /// registry is rebuilt and published immediately so a later debounced
    /// heavy snapshot cannot lose the corresponding disconnect notification.
    pub(in crate::kms::render::backend) fn publish_connector_probes(
        &mut self,
        state: &mut ServerState,
        probes: &[(
            crate::platform::drm::DrmDeviceKey,
            Vec<crate::platform::drm::ConnectorProbe>,
        )],
    ) -> bool {
        let mut changed_keys = HashSet::new();
        let mut config_changed = false;
        for (device_key, connectors) in probes {
            let delta =
                reconcile_connector_probe(&mut self.randr_id_alloc, *device_key, connectors);
            changed_keys.extend(delta.changed_keys);
            config_changed |= delta.config_changed;
        }
        let changed = !changed_keys.is_empty();
        if changed {
            self.bump_crtc_config_topology_epoch("connector probe changed");
        }
        // A force query never advances lastSetTime. It advances lastConfigTime
        // for connection/mode-list changes and notifies dirty outputs for
        // those or monitor-identity invalidation.
        self.rebuild_randr_state(state, None, config_changed);
        if changed {
            let changed_ids: HashSet<_> = changed_keys
                .iter()
                .filter_map(|key| {
                    self.randr_id_alloc
                        .entry(key)
                        .map(|entry| entry.ids.output_id)
                })
                .collect();
            let mut changed_outputs: Vec<_> = state
                .randr
                .outputs
                .iter()
                .filter(|output| changed_ids.contains(&output.output_id))
                .map(|output| (output.output_id, output.crtc_id, output.mode_id))
                .collect();
            changed_outputs.sort_unstable_by_key(|&(output, _, _)| output);
            yserver_core::core_loop::run::emit_randr_connector_change_notifications(
                state,
                &[],
                &changed_outputs,
            );
        }
        changed
    }

    /// RandR output list — mirrors `KmsBackend::randr_outputs`.
    #[must_use]
    pub fn randr_outputs(&mut self) -> Vec<yserver_core::randr::RandrOutput> {
        self.randr_outputs_and_modes().0
    }

    /// Provider endpoint used by the selected operational renderer.
    ///
    /// Vulkan primary-node metadata is diagnostic, not a KMS ownership
    /// claim. It coalesces the renderer with an existing KMS provider only
    /// when it exactly names an opened KMS device; every other selected
    /// renderer remains its own endpoint.
    pub(in crate::kms::render::backend) fn selected_render_provider_endpoint(
        &self,
    ) -> Option<RandrProviderEndpoint> {
        let renderer = self.platform.selected_render_device()?;
        if let Some(primary) = renderer.advertised_primary_node
            && self
                .platform
                .devices
                .iter()
                .any(|device| device.key == primary)
        {
            return Some(RandrProviderEndpoint::Kms(primary));
        }
        Some(RandrProviderEndpoint::Render(renderer.id))
    }

    /// Resolve a provider XID only when its endpoint is still projected now.
    /// `RandrIdAllocator` deliberately retains historical IDs, so using its
    /// reverse lookup alone at a request boundary could revive a stale source
    /// or sink after a future renderer/provider topology change.
    pub(in crate::kms::render::backend) fn current_provider_endpoint_for_id(
        &self,
        provider_id: u32,
    ) -> Option<RandrProviderEndpoint> {
        let endpoint = self.randr_id_alloc.provider_endpoint_for_id(provider_id)?;
        self.randr_provider_endpoints()
            .contains(&endpoint)
            .then_some(endpoint)
    }

    /// Whether the selected renderer is authorized to feed one KMS device.
    ///
    /// A coalesced renderer/KMS provider needs no self-association. Every
    /// distinct KMS sink must retain an explicit endpoint-level policy entry;
    /// the route's tri-state transport probe remains authoritative later when
    /// the scanout pool is allocated.
    pub(in crate::kms::render::backend) fn provider_output_source_allows(
        &self,
        kms_device_key: crate::platform::drm::DrmDeviceKey,
    ) -> bool {
        let Some(source) = self.selected_render_provider_endpoint() else {
            return false;
        };
        let sink = RandrProviderEndpoint::Kms(kms_device_key);
        source == sink || self.provider_output_sources.get(&kms_device_key) == Some(&source)
    }

    /// Validate the routes committed during platform bring-up and initialize
    /// the one-shot automatic PRIME Output Source policy.
    ///
    /// Every opened KMS endpoint distinct from the selected renderer is
    /// recorded, even when it currently has no connector inventory. Provider
    /// projection remains capability-aware and hides such an association until
    /// a connector is discovered. This initializer is constructor-only: a
    /// later explicit detach removes the map entry permanently and ordinary
    /// RANDR rebuilds or connector disconnect/reconnect cycles never recreate
    /// it.
    pub(in crate::kms::render::backend) fn initialize_provider_output_sources(
        &mut self,
    ) -> io::Result<()> {
        let Some(selected_renderer) = self.platform.selected_render_device() else {
            if self.platform.outputs.is_empty() {
                return Ok(());
            }
            return Err(io::Error::other(
                "active startup outputs exist without a selected renderer endpoint",
            ));
        };
        let selected_render_device_id = selected_renderer.id;
        let source = self
            .selected_render_provider_endpoint()
            .expect("selected renderer has a provider endpoint");

        for output in &self.platform.outputs {
            if output.scanout_route.render_device_id != selected_render_device_id {
                return Err(io::Error::other(format!(
                    "active startup output {} on {} records renderer {:?}, but the selected renderer is {:?}",
                    output.key.connector_name,
                    output.key.device_key,
                    output.scanout_route.render_device_id,
                    selected_render_device_id,
                )));
            }
            if output.scanout_route.kms_device_key != output.key.device_key {
                return Err(io::Error::other(format!(
                    "active startup output {} records KMS endpoint {}, but its owner is {}",
                    output.key.connector_name,
                    output.scanout_route.kms_device_key,
                    output.key.device_key,
                )));
            }
        }

        for device in &self.platform.devices {
            let sink = RandrProviderEndpoint::Kms(device.key);
            if sink == source {
                continue;
            }
            if let Some(&previous) = self.provider_output_sources.get(&device.key) {
                if previous != source {
                    return Err(io::Error::other(format!(
                        "automatic KMS sink {} has conflicting sources {previous:?} and {source:?}",
                        device.key,
                    )));
                }
                continue;
            }
            self.provider_output_sources.insert(device.key, source);
            log::info!(
                "PRIME Output Source: automatically associated sink {sink:?} -> source {source:?}",
            );
        }
        Ok(())
    }

    fn randr_provider_endpoints(&self) -> Vec<RandrProviderEndpoint> {
        let mut endpoints = Vec::with_capacity(self.platform.devices.len().saturating_add(1));
        if let Some(renderer) = self.selected_render_provider_endpoint() {
            endpoints.push(renderer);
        }
        for device in &self.platform.devices {
            let kms = RandrProviderEndpoint::Kms(device.key);
            if !endpoints.contains(&kms) {
                endpoints.push(kms);
            }
        }
        endpoints
    }

    fn reserve_randr_provider_ids(&mut self) {
        for endpoint in self.randr_provider_endpoints() {
            let _ = self.randr_id_alloc.provider_id_for(endpoint);
        }
    }

    fn kms_provider_name(device: &crate::kms::render::platform::KmsDevice) -> String {
        std::path::Path::new(device.device.path())
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .unwrap_or_else(|| device.device.path())
            .to_string()
    }

    fn render_provider_name(device: &crate::kms::render::platform::RenderDevice) -> String {
        if let Some(node) = &device.render_node {
            return node
                .path()
                .file_name()
                .and_then(std::ffi::OsStr::to_str)
                .map_or_else(|| node.path().display().to_string(), str::to_string);
        }
        match device.id {
            crate::kms::render::platform::RenderDeviceId::DrmRender(key) => {
                format!("drm-render-{key}")
            }
            crate::kms::render::platform::RenderDeviceId::UnverifiedFallback => {
                "vulkan-unverified".to_string()
            }
        }
    }

    /// Project opened KMS devices, the selected renderer, and persistent PRIME
    /// Output Source relationships into stable RANDR providers.
    ///
    /// The selected renderer coalesces with a KMS provider only when its
    /// advertised primary node exactly matches that KMS device. It advertises
    /// `SourceOutput`; distinct KMS endpoints with known connector inventory
    /// advertise only `SinkOutput`. Unselected metadata-only renderer inventory
    /// is not operational and is therefore omitted. KMS providers own every
    /// connector/CRTC XID allocated for their device, including connectors
    /// that are currently disconnected; a distinct render provider owns no
    /// display resources. Associations are the endpoint policy projected in
    /// both directions and never assert that a concrete DMA-BUF allocation is
    /// compatible.
    #[must_use]
    pub fn randr_providers(&mut self) -> Vec<yserver_core::randr::RandrProvider> {
        use yserver_core::randr::{RandrProvider, RandrProviderAssociation};
        use yserver_protocol::x11::randr::{
            PROVIDER_CAPABILITY_SINK_OUTPUT, PROVIDER_CAPABILITY_SOURCE_OUTPUT,
        };

        self.reserve_randr_provider_ids();
        let selected_endpoint = self.selected_render_provider_endpoint();
        let distinct_renderer = match selected_endpoint {
            Some(endpoint @ RandrProviderEndpoint::Render(_)) => self
                .platform
                .selected_render_device()
                .map(|device| (endpoint, Self::render_provider_name(device))),
            _ => None,
        };
        let endpoints = self.randr_provider_endpoints();
        let provider_ids: HashMap<_, _> = endpoints
            .iter()
            .copied()
            .map(|endpoint| {
                let id = self.randr_id_alloc.provider_id_for(endpoint);
                (endpoint, id)
            })
            .collect();
        let kms_with_connector_inventory: HashSet<_> = self
            .randr_id_alloc
            .entries()
            .map(|(key, _)| key.device_key)
            .collect();
        let mut associations_by_endpoint: HashMap<
            RandrProviderEndpoint,
            Vec<RandrProviderAssociation>,
        > = HashMap::new();
        if let Some(source) = selected_endpoint {
            for (&sink_key, &configured_source) in &self.provider_output_sources {
                let sink = RandrProviderEndpoint::Kms(sink_key);
                if configured_source != source
                    || sink == source
                    || !provider_ids.contains_key(&sink)
                    || !kms_with_connector_inventory.contains(&sink_key)
                {
                    continue;
                }
                let Some(&source_id) = provider_ids.get(&source) else {
                    continue;
                };
                let sink_id = provider_ids[&sink];
                associations_by_endpoint
                    .entry(sink)
                    .or_default()
                    .push(RandrProviderAssociation {
                        provider_id: source_id,
                        capability: PROVIDER_CAPABILITY_SOURCE_OUTPUT,
                    });
                associations_by_endpoint.entry(source).or_default().push(
                    RandrProviderAssociation {
                        provider_id: sink_id,
                        capability: PROVIDER_CAPABILITY_SINK_OUTPUT,
                    },
                );
            }
        }
        for associations in associations_by_endpoint.values_mut() {
            associations.sort_by_key(|association| association.provider_id);
        }

        let mut providers = Vec::with_capacity(endpoints.len());
        for device in &self.platform.devices {
            let endpoint = RandrProviderEndpoint::Kms(device.key);
            let provider_id = provider_ids[&endpoint];
            let mut crtcs = Vec::new();
            let mut outputs = Vec::new();
            for (key, entry) in self.randr_id_alloc.entries() {
                if key.device_key != device.key {
                    continue;
                }
                crtcs.push(entry.ids.crtc_id);
                outputs.push(entry.ids.output_id);
            }
            crtcs.sort_unstable();
            outputs.sort_unstable();
            let capabilities = match selected_endpoint {
                Some(source) if source == endpoint => PROVIDER_CAPABILITY_SOURCE_OUTPUT,
                Some(_) if kms_with_connector_inventory.contains(&device.key) => {
                    PROVIDER_CAPABILITY_SINK_OUTPUT
                }
                _ => 0,
            };
            providers.push(RandrProvider {
                provider_id,
                name: Self::kms_provider_name(device),
                capabilities,
                is_gpu: selected_endpoint.is_some_and(|source| source != endpoint),
                crtcs,
                outputs,
                associations: associations_by_endpoint
                    .remove(&endpoint)
                    .unwrap_or_default(),
            });
        }
        if let Some((endpoint, name)) = distinct_renderer {
            providers.push(RandrProvider {
                provider_id: provider_ids[&endpoint],
                name,
                capabilities: PROVIDER_CAPABILITY_SOURCE_OUTPUT,
                is_gpu: false,
                crtcs: Vec::new(),
                outputs: Vec::new(),
                associations: associations_by_endpoint
                    .remove(&endpoint)
                    .unwrap_or_default(),
            });
        }
        providers.sort_by_key(|provider| provider.provider_id);
        providers
    }

    /// RandR outputs plus the full deduped mode table.
    #[must_use]
    pub fn randr_outputs_and_modes(
        &mut self,
    ) -> (
        Vec<yserver_core::randr::RandrOutput>,
        Vec<yserver_core::randr::RandrMode>,
    ) {
        use yserver_core::randr::{RandrMode, RandrOutput};

        // Provider ids share the same XID source as outputs, CRTCs, and modes
        // even before providers are exposed on the wire. Reserve every
        // projected endpoint now (selected renderer first, then deduplicated
        // KMS devices) so identities stay stable and collision-free.
        self.reserve_randr_provider_ids();

        // A lightweight disconnect leaves the registry config Enabled, so
        // the live CRTC remains projected while `connected` flips false. A
        // heavy drop (and a startup force-probe miss) sets config Off; do not
        // let a stale `platform.outputs` row revive that route.
        let live_keys: HashSet<OutputKey> = self
            .platform
            .outputs
            .iter()
            .filter(|layout| {
                self.randr_id_alloc
                    .entry(&layout.key)
                    .is_none_or(|entry| matches!(entry.config, ConnectorConfig::Enabled { .. }))
            })
            .map(|layout| layout.key.clone())
            .collect();
        let mut outs: Vec<RandrOutput> = Vec::with_capacity(
            self.platform.outputs.len() + self.randr_id_alloc.known_connectors().len(),
        );
        // Rebuild the per-output identity map (EDID + ConnectorType) each
        // projection so the output-property handlers serve current data.
        self.output_identity_by_id.clear();
        self.output_key_by_id.clear();
        self.crtc_key_by_id.clear();
        for layout in self
            .platform
            .outputs
            .iter()
            .filter(|layout| live_keys.contains(&layout.key))
        {
            let vrefresh = layout.output.picked.vrefresh;
            let output_key = layout.key.clone();
            let registry_entry_exists = self.randr_id_alloc.entry(&output_key).is_some();
            let ids = self.randr_id_alloc.ids_for(&output_key);
            let (connected, connector_modes, edid, mm_width, mm_height, connector_type) = {
                let entry = self.randr_id_alloc.entry_mut(&output_key);
                // Production seeds every connected connector from a heavy
                // snapshot before projection. Keep a defensive fallback for
                // synthetic fixtures which construct ActiveOutput directly.
                if !registry_entry_exists {
                    entry.modes.clone_from(&layout.output.modes);
                    entry.edid.clone_from(&layout.output.edid);
                    entry.mm_width = layout.output.mm_width;
                    entry.mm_height = layout.output.mm_height;
                    entry
                        .connector_type
                        .clone_from(&layout.output.connector_type);
                    entry.connected = true;
                    entry.config = ConnectorConfig::Enabled {
                        mode_w: layout.width,
                        mode_h: layout.height,
                        vrefresh,
                        x: layout.x,
                        y: layout.y,
                    };
                    entry.crtc_associated = true;
                }
                // A live KMS route is necessarily attached. This also seeds
                // fixtures whose registry entry predates the active-output
                // projection, before a later physical disconnect preserves
                // the association while retiring the route.
                entry.crtc_associated = true;
                (
                    entry.connected,
                    entry.modes.clone(),
                    entry.edid.clone(),
                    entry.mm_width,
                    entry.mm_height,
                    entry.connector_type.clone(),
                )
            };
            self.output_key_by_id
                .insert(ids.output_id, output_key.clone());
            self.crtc_key_by_id.insert(ids.crtc_id, output_key);
            let mode_id = self.randr_id_alloc.mode_id(&layout.output.picked);
            if connected {
                self.output_identity_by_id
                    .insert(ids.output_id, (edid, connector_type));
            }
            let mut mode_ids = Vec::with_capacity(connector_modes.len());
            let mut num_preferred: u16 = 0;
            for mode in &connector_modes {
                let mode_id = self.randr_id_alloc.mode_id(mode);
                // A connector can advertise the same exact resource more
                // than once. List each XID only once (issue #48).
                if mode_ids.contains(&mode_id) {
                    continue;
                }
                mode_ids.push(mode_id);
                if mode.preferred {
                    num_preferred = num_preferred.saturating_add(1);
                }
            }
            outs.push(RandrOutput {
                name: layout.output.connector_name.clone(),
                output_id: ids.output_id,
                crtc_id: ids.crtc_id,
                mode_id,
                connected,
                x: i16::try_from(layout.x).unwrap_or(i16::MAX),
                y: i16::try_from(layout.y).unwrap_or(i16::MAX),
                width: layout.width,
                height: layout.height,
                vrefresh,
                timing: mode_timing(&layout.output.picked),
                mm_width: if connected { mm_width } else { 0 },
                mm_height: if connected { mm_height } else { 0 },
                mode_ids,
                num_preferred,
                pending_transform: Default::default(),
                current_transform: Default::default(),
                rotation: yserver_core::randr::RR_ROTATE_0,
            });
        }

        let advertised_modes: Vec<crate::platform::drm::Mode> = self
            .randr_id_alloc
            .entries()
            .flat_map(|(_, entry)| entry.modes.clone())
            .collect();
        // A replacement monitor can omit the timing still programmed on a
        // live CRTC. Retain that current mode resource until a client selects
        // a replacement-advertised mode, avoiding a dangling mode XID.
        let current_modes: Vec<crate::platform::drm::Mode> = self
            .platform
            .outputs
            .iter()
            .filter(|layout| live_keys.contains(&layout.key))
            .map(|layout| layout.output.picked.clone())
            .collect();
        let mut modes: Vec<RandrMode> = Vec::new();
        let mut mode_identities = HashSet::new();
        for mode in current_modes.into_iter().chain(advertised_modes) {
            let identity = ModeIdentity::from(&mode);
            let mode_id = self.randr_id_alloc.mode_id(&mode);
            if mode_identities.insert(identity) {
                modes.push(RandrMode {
                    mode_id,
                    width: mode.width,
                    height: mode.height,
                    vrefresh: mode.vrefresh,
                    timing: mode_timing(&mode),
                });
            }
        }

        // Not-live (registered but not in `platform.outputs`) connectors.
        // Two sub-states, distinguished by the registry `connected` flag:
        //   - connected=true  → hotplugged but not yet enabled (Task 5.2):
        //     report RR_Connected with mode=0/crtc-unassigned so a client
        //     can enable it (GetOutputInfo offers the advertised modes).
        //   - connected=false → physically disconnected: RR_Disconnected.
        // Either way the last-known advertised mode list is retained so
        // GetOutputInfo stays consistent with the GetScreenResources union.
        // Collect owned first to release the &self borrow before the
        // &mut self mode_id() allocations below.
        let not_live: Vec<NotLiveConnector> = self
            .randr_id_alloc
            .entries()
            .filter(|(key, _)| !live_keys.contains(*key))
            .map(|(key, entry)| NotLiveConnector {
                key: key.clone(),
                ids: entry.ids,
                connected: entry.connected,
                modes: entry.modes.clone(),
                edid: entry.edid.clone(),
                mm_width: entry.mm_width,
                mm_height: entry.mm_height,
                connector_type: entry.connector_type.clone(),
            })
            .collect();
        for connector in not_live {
            let mut mode_ids = Vec::with_capacity(connector.modes.len());
            let mut num_preferred: u16 = 0;
            for mode in connector.modes {
                let mode_id = self.randr_id_alloc.mode_id(&mode);
                // See the live-output loop: list each exact XID once.
                if mode_ids.contains(&mode_id) {
                    continue;
                }
                mode_ids.push(mode_id);
                if mode.preferred {
                    num_preferred = num_preferred.saturating_add(1);
                }
            }
            self.output_key_by_id
                .insert(connector.ids.output_id, connector.key.clone());
            self.crtc_key_by_id
                .insert(connector.ids.crtc_id, connector.key.clone());
            if connector.connected {
                self.output_identity_by_id.insert(
                    connector.ids.output_id,
                    (connector.edid, connector.connector_type),
                );
            }
            outs.push(RandrOutput {
                name: connector.key.connector_name,
                output_id: connector.ids.output_id,
                crtc_id: connector.ids.crtc_id,
                mode_id: 0,
                connected: connector.connected,
                x: 0,
                y: 0,
                width: 0,
                height: 0,
                vrefresh: 0,
                timing: None,
                mm_width: if connector.connected {
                    connector.mm_width
                } else {
                    0
                },
                mm_height: if connector.connected {
                    connector.mm_height
                } else {
                    0
                },
                mode_ids,
                num_preferred,
                pending_transform: Default::default(),
                current_transform: Default::default(),
                rotation: yserver_core::randr::RR_ROTATE_0,
            });
        }
        outs.sort_by_key(|o| o.output_id);
        modes.sort_by_key(|m| m.mode_id);
        self.refresh_present_crtc_clock_epochs();
        (outs, modes)
    }

    /// The one place `state.randr` is rebuilt from the backend's
    /// registry/outputs.
    /// - `set_time`: `Some(t)` sets `timestamp` (lastSetTime) to `t`
    ///   (SetCrtcConfig uses the client-provided request timestamp);
    ///   `None` preserves the prior value (connector discovery and no-op
    ///   reprobes must not advance lastSetTime).
    /// - `config_changed`: advances `config_timestamp` (lastConfigTime)
    ///   only when the available config (outputs/modes/connection)
    ///   changed — i.e. hotplug/reprobe-with-change, NOT a CRTC set.
    ///
    /// The current logical screen size (`screen_width`/`screen_height`
    /// and `*_mm`) is OWNED by `RRSetScreenSize` once set and is carried
    /// forward across every rebuild — a re-probe or CRTC set never
    /// collapses a client-resized screen back to the bounding box
    /// (Xorg keeps `pScreen->width/height` until the client resizes).
    /// The extent the projection *derives* underneath that carry-forward
    /// covers the live outputs unioned with the slots still reserved by
    /// departed-but-restorable routes, so it never describes a screen that
    /// shrank around a monitor we are about to re-light.
    pub(in crate::kms::render::backend) fn rebuild_randr_state(
        &mut self,
        state: &mut ServerState,
        set_time: Option<u32>,
        config_changed: bool,
    ) {
        let prev_ts = state.randr.timestamp;
        let prev_ct = state.randr.config_timestamp;
        let prev_primary = state.randr.primary_output;
        let prev_primary_explicit = state.randr_primary_output_explicit;
        // Snapshot the client-owned logical screen size to carry forward.
        let prev_screen = (
            state.randr.screen_width,
            state.randr.screen_height,
            state.randr.width_mm,
            state.randr.height_mm,
        );
        let prev_transforms = state.randr.crtc_transforms();
        let (outputs, mode_table) = self.randr_outputs_and_modes();
        let providers = self.randr_providers();
        let reserved = self.reserved_layout_slots();
        let new_ts = set_time.unwrap_or(prev_ts);
        let ts_now = state.timestamp_now();
        state.randr = yserver_core::randr::RandrState::from_outputs_with_modes_and_reservations(
            new_ts, outputs, mode_table, &reserved,
        );
        // CRTC transforms are client-owned, like the logical size below.
        state.randr.restore_crtc_transforms(prev_transforms);
        let associations = self
            .randr_id_alloc
            .entries()
            .filter(|(_, entry)| entry.crtc_associated)
            .map(|(_, entry)| (entry.ids.output_id, entry.ids.crtc_id));
        state.randr.set_output_crtc_associations(associations);
        state.randr.set_providers(providers);
        // Carry forward the client-set logical size (from_outputs reseeds
        // it to the bbox; that is only correct at boot, where prev_screen
        // already equals the bbox).
        state.randr.screen_width = prev_screen.0;
        state.randr.screen_height = prev_screen.1;
        state.randr.width_mm = prev_screen.2;
        state.randr.height_mm = prev_screen.3;
        // SetOutputPrimary is client-owned metadata. Keep it across a
        // topology rebuild while the output resource still exists, including
        // a temporarily disconnected output. If it vanished entirely, retain
        // from_outputs' best available fallback instead.
        state.randr_primary_output_explicit = restore_primary_output_after_rebuild(
            prev_primary,
            prev_primary_explicit,
            &mut state.randr,
        );
        state.randr.config_timestamp = if config_changed { ts_now } else { prev_ct };
    }

    /// Reconcile scene, RANDR state, and client notifications after a
    /// connector topology change.
    pub(in crate::kms::render::backend) fn fire_randr_changes(
        &mut self,
        state: &mut ServerState,
        rescan: crate::kms::render::platform::RescanResult,
        registry_changed_keys: &[OutputKey],
        config_changed: bool,
        rebuild_scene: bool,
        relight_after_quiesce: bool,
    ) -> bool {
        let previous_outputs: HashMap<u32, ProjectedRandrOutputState> = state
            .randr
            .outputs
            .iter()
            .map(|output| {
                (
                    output.output_id,
                    (
                        output.connected,
                        output.crtc_id,
                        output.mode_id,
                        output.x,
                        output.y,
                        output.width,
                        output.height,
                    ),
                )
            })
            .collect();
        let physically_connected: HashSet<&OutputKey> =
            rescan.connected.iter().map(|entry| &entry.key).collect();
        for key in &rescan.added_keys {
            log::info!(
                "kms: RandR output connected: {} on {}",
                key.connector_name,
                key.device_key
            );
        }
        for key in &rescan.dropped_keys {
            if physically_connected.contains(key) {
                log::info!(
                    "kms: active output disabled (no usable modes): {} on {}",
                    key.connector_name,
                    key.device_key
                );
            } else {
                log::info!(
                    "kms: RandR output disconnected: {} on {}",
                    key.connector_name,
                    key.device_key
                );
            }
        }

        // Any active-output removal was quiesced against the old authoritative
        // CRTC set before the connector snapshot mutated platform vectors.
        // Metadata-only/inactive-connector changes need no scene disruption.
        // A metadata-only/inactive-connector update does not replace the
        // active scanout topology. M2 owns the framebuffer cached by M1, so
        // clearing it in that case could RMFB a client buffer still being
        // scanned. Active removals were quiesced (and M1 cleared) before this
        // method; retain the cache for all other publications.
        if rebuild_scene {
            self.scanout_m1.clear("output topology changed");
        }

        if rebuild_scene && let Err(e) = self.scene.rebuild_outputs(&self.platform) {
            log::error!("kms: scene rebuild after topology change failed: {e:?}; exiting");
            self.request_exit();
            return false;
        }
        let relit = relight_after_quiesce && state.dpms.power_level == 0;
        if let Err(_error) = self.relight_after_direct_teardown(relit, "output-topology change") {
            return false;
        }

        // A relit or newly enabled output can extend the virtual screen past
        // the root backing storage the last logical resize allocated: the
        // enable path recomputes `fb_w`/`fb_h`, but nothing else resizes
        // root/COW storage, so the newly covered columns have no root pixels
        // behind them and the relit monitor shows no background at all
        // (measured on a dual-head MATE session: a client `RRSetScreenSize`
        // shrank root storage to the survivor on the unplug, the relight grew
        // the extent back, and the reconnected monitor stayed blank).
        //
        // Route through the same helper `set_logical_screen_size` uses -- the
        // direct-unflip preamble before the reallocation is load-bearing.
        //
        // Grow-to-cover only: an extent that SHRANK leaves storage that still
        // covers every visible pixel, so reallocating it would wipe root
        // content that is still on screen and buy nothing.
        let (fb_w, fb_h) = (self.platform.fb_w, self.platform.fb_h);
        let undersized = self.root_storage_extent().is_some_and(|extent| {
            extent.width < u32::from(fb_w) || extent.height < u32::from(fb_h)
        });
        if undersized {
            if let Err(error) = self.apply_virtual_screen_extent(fb_w, fb_h) {
                // Not fatal: the old storage still covers the outputs it
                // covered before, so keep publishing the topology rather
                // than dropping the whole change.
                log::error!(
                    "kms: root storage could not be grown to {fb_w}×{fb_h} after a \
                     topology change: {error}"
                );
            } else {
                log::info!("kms: grew root storage to {fb_w}×{fb_h} for the new output topology");
            }
        }

        // Asynchronous physical discovery never represents a client Set, so
        // it preserves lastSetTime even when a live CRTC is retired. A fresh
        // connection or mode-list delta advances lastConfigTime; monitor
        // identity metadata alone does not. If a lightweight force query
        // already published the connector delta, its later heavy CRTC
        // retirement preserves both timestamps.
        self.rebuild_randr_state(state, None, config_changed);

        // Propagate the new virtual extent (set while applying the connector
        // snapshot) to absolute input mapping. This allows absolute devices to
        // reach the full multi-monitor span after a hotplug.
        let (new_fb_w, new_fb_h) = (self.platform.fb_w, self.platform.fb_h);
        self.update_input_extent(new_fb_w, new_fb_h);

        if relit {
            self.kms_outputs_active = !self.platform.outputs.is_empty();
        } else if self.platform.outputs.is_empty() {
            self.kms_outputs_active = false;
        }

        let (w, h) = (state.randr.screen_width, state.randr.screen_height);
        if let Some(root) = state
            .resources
            .window_mut(yserver_core::resources::ROOT_WINDOW)
        {
            root.width = w;
            root.height = h;
        }
        if let Some(overlay) = state
            .resources
            .window_mut(yserver_core::resources::COMPOSITE_OVERLAY_WINDOW)
        {
            overlay.width = w;
            overlay.height = h;
        }

        let registry_changed_ids: HashSet<_> = registry_changed_keys
            .iter()
            .filter_map(|key| {
                self.randr_id_alloc
                    .entry(key)
                    .map(|entry| entry.ids.output_id)
            })
            .collect();
        let mut crtc_changed = Vec::new();
        let mut output_changed = Vec::new();
        for output in &state.randr.outputs {
            let before = previous_outputs.get(&output.output_id).copied();
            let before_crtc = before.map(|(_, crtc, mode, x, y, width, height)| {
                (mode != 0, crtc, mode, x, y, width, height)
            });
            let after_crtc = (
                output.mode_id != 0,
                output.crtc_id,
                output.mode_id,
                output.x,
                output.y,
                output.width,
                output.height,
            );
            if before_crtc != Some(after_crtc)
                && (before_crtc.is_some_and(|state| state.0) || after_crtc.0)
            {
                crtc_changed.push((output.output_id, output.crtc_id, output.mode_id));
            }

            let before_output =
                before.map(|(connected, crtc, mode, ..)| (connected, (mode != 0).then_some(crtc)));
            let after_output = (
                output.connected,
                (output.mode_id != 0).then_some(output.crtc_id),
            );
            if registry_changed_ids.contains(&output.output_id)
                || before_output != Some(after_output)
            {
                output_changed.push((output.output_id, output.crtc_id, output.mode_id));
            }
        }
        crtc_changed.sort_unstable_by_key(|&(output, _, _)| output);
        output_changed.sort_unstable_by_key(|&(output, _, _)| output);
        yserver_core::core_loop::run::emit_randr_connector_change_notifications(
            state,
            &crtc_changed,
            &output_changed,
        );
        self.scene.wake_for_damage();
        true
    }

    /// Re-point the virtual screen -- `platform.fb_w`/`fb_h` plus the root
    /// (and, if materialised, the COW) backing storage -- at a `w`x`h`
    /// extent, and tell the compositor what just changed under it.
    ///
    /// Shared by the two paths that change the virtual extent:
    /// `set_logical_screen_size` (a client `RRSetScreenSize`) and
    /// `fire_randr_changes` (a connector hotplug/relight that grew it).
    /// The ordering inside is load-bearing and is the reason the hotplug
    /// path routes through here instead of reallocating storage itself:
    ///
    /// 1. An active direct frame is snapshotted into its old COW and
    ///    unflipped FIRST -- reallocating root/COW storage changes the
    ///    fallback identity that frame holds.
    /// 2. Only then are `fb_w`/`fb_h`, the input extent and the root/COW
    ///    storage replaced.
    /// 3. The per-BO scanout damage model is invalidated, because the
    ///    storage under every scanout BO just changed while the BOs
    ///    themselves stayed valid.
    ///
    /// # Errors
    ///
    /// Returns the direct-scanout materialisation error when an active
    /// direct frame cannot be snapshotted. In that case the old extent and
    /// every storage owner/pin are left untouched.
    pub(in crate::kms::render::backend) fn apply_virtual_screen_extent(
        &mut self,
        w: u16,
        h: u16,
    ) -> io::Result<()> {
        // Reallocating root/COW storage changes the fallback identity held by
        // an active direct frame. Snapshot that frame into its old COW and
        // request the synchronized replacement first. On failure, leave the
        // old dimensions and every storage owner/pin untouched.
        if self.scanout_m2.active() {
            self.materialize_direct_shadow_for_unflip()?;
            self.request_direct_unflip("virtual_screen_extent_before_storage_reallocation");
        }

        // ── 1. Update the platform's logical extent ───────────────────────
        self.bump_crtc_config_topology_epoch("virtual screen extent changed");
        self.platform.fb_w = w;
        self.platform.fb_h = h;

        // Propagate the new extent to the input thread's absolute mapper so
        // absolute devices can reach the full virtual screen after a resize.
        self.update_input_extent(w, h);

        // ── 2. Resize root backing storage ────────────────────────────────
        // The root drawable is always allocated (init_root_storage runs at
        // boot). Resize it with the same detach→decref→allocate→fill
        // pattern used by configure_subwindow.
        let root_xid = self.core.window_id;
        if let Some(old_id) = self.store.lookup(root_xid) {
            self.store.detach_xid(root_xid);
            self.store_decref_with_invalidate(old_id);
            match self.platform.allocate_drawable_storage_as(
                w,
                h,
                32,
                crate::kms::vk::mem_accounting::MemCategory::WindowStorage,
            ) {
                Ok(storage) => {
                    self.telemetry.record_storage_allocation();
                    self.telemetry.record_image_view_create();
                    match self.store_alloc(root_xid, DrawableKind::Root, 32, true, storage) {
                        Ok(new_id) => {
                            let rect = ash::vk::Rect2D {
                                offset: ash::vk::Offset2D::default(),
                                extent: ash::vk::Extent2D {
                                    width: u32::from(w),
                                    height: u32::from(h),
                                },
                            };
                            if let Err(e) = self.engine.fill_rect(
                                &mut self.store,
                                &mut self.platform,
                                Dst::server_internal(new_id),
                                rect,
                                decode_x11_pixel_for_storage(
                                    self.core.bg_pixel.unwrap_or(
                                        yserver_core::resources::ROOT_DEFAULT_BACKGROUND_PIXEL,
                                    ),
                                    24,
                                    PlatformBackend::format_for_depth(24),
                                ),
                            ) && self.platform.vk.is_some()
                            {
                                log::warn!(
                                    "render apply_virtual_screen_extent: root fill failed: {e:?}"
                                );
                            }
                        }
                        Err(e) => {
                            log::warn!(
                                "render apply_virtual_screen_extent: root store.allocate failed: {e:?}"
                            );
                        }
                    }
                }
                Err(e) => {
                    // No Vk (test fixture): allocate a null-view stub so the
                    // xid remains live and tests can continue.
                    log::debug!(
                        "render apply_virtual_screen_extent: no Vk, stub root storage: {e:?}"
                    );
                    let storage = Storage::for_tests_null(
                        ash::vk::Extent2D {
                            width: u32::from(w),
                            height: u32::from(h),
                        },
                        PlatformBackend::format_for_depth(32),
                    );
                    if let Err(e) =
                        self.store_alloc(root_xid, DrawableKind::Root, 32, true, storage)
                    {
                        log::warn!(
                            "render apply_virtual_screen_extent: root stub alloc failed: {e:?}"
                        );
                    }
                }
            }
        }

        // The fill above is the pixel background; a background pixmap tiles
        // over it, as Xorg repaints the whole resized root with its tile.
        if let Some(bg_pixmap) = self.core.bg_pixmap {
            self.tile_root_background_pixmap(bg_pixmap.as_raw());
        }

        // ── 3. Resize COW backing storage (if materialised) ──────────────
        // The COW is lazily allocated on the first CompositeGetOverlayWindow
        // call. If it hasn't been created yet, fb_w/fb_h are already updated
        // above so the first allocation will use the new dimensions.
        if let Some(old_cow_id) = self.cow_id.take() {
            let cow_xid = yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0;
            self.store.detach_xid(cow_xid);
            self.store_decref_with_invalidate(old_cow_id);
            match self.platform.allocate_drawable_storage_as(
                w,
                h,
                24,
                crate::kms::vk::mem_accounting::MemCategory::WindowStorage,
            ) {
                Ok(storage) => {
                    self.telemetry.record_storage_allocation();
                    self.telemetry.record_image_view_create();
                    match self.store_alloc(cow_xid, DrawableKind::Window, 24, true, storage) {
                        Ok(new_cow_id) => {
                            // Fill so the compositor doesn't see
                            // recycled GPU content on its next paint.
                            // OPAQUE black, not transparent: the COW is
                            // a depth-24 drawable, and on X11 depth-24
                            // has no alpha channel — it is opaque by
                            // definition. See `default_window_init_color`.
                            let rect = ash::vk::Rect2D {
                                offset: ash::vk::Offset2D::default(),
                                extent: ash::vk::Extent2D {
                                    width: u32::from(w),
                                    height: u32::from(h),
                                },
                            };
                            if let Err(e) = self.engine.fill_rect(
                                &mut self.store,
                                &mut self.platform,
                                Dst::server_internal(new_cow_id),
                                rect,
                                default_window_init_color(24),
                            ) && self.platform.vk.is_some()
                            {
                                log::warn!(
                                    "render apply_virtual_screen_extent: COW init fill failed: {e:?}"
                                );
                            }
                            self.cow_id = Some(new_cow_id);
                            // Update the windows geometry so scene assembly
                            // uses the new dimensions.
                            if let Some(geom) = self.windows.get_mut(&cow_xid) {
                                geom.width = w;
                                geom.height = h;
                            }
                        }
                        Err(e) => {
                            log::warn!(
                                "render apply_virtual_screen_extent: COW store.allocate failed: {e:?}"
                            );
                            // cow_id stays None (taken above); the COW will be
                            // re-materialised on the next CompositeGetOverlayWindow.
                        }
                    }
                }
                Err(e) => {
                    log::debug!(
                        "render apply_virtual_screen_extent: no Vk, stub COW storage: {e:?}"
                    );
                    let storage = crate::kms::render::store::Storage::for_tests_null(
                        ash::vk::Extent2D {
                            width: u32::from(w),
                            height: u32::from(h),
                        },
                        PlatformBackend::format_for_depth(24),
                    );
                    match self.store_alloc(cow_xid, DrawableKind::Window, 24, true, storage) {
                        Ok(new_cow_id) => {
                            self.cow_id = Some(new_cow_id);
                            if let Some(geom) = self.windows.get_mut(&cow_xid) {
                                geom.width = w;
                                geom.height = h;
                            }
                        }
                        Err(e) => {
                            log::warn!(
                                "render apply_virtual_screen_extent: COW stub alloc failed: {e:?}"
                            );
                        }
                    }
                }
            }
        }

        // ── 4. Mark scene dirty — no drain/rebuild needed ─────────────────
        // A logical resize (RRSetScreenSize) does NOT change per-output
        // scanout pool geometry or output positions; only the root/COW
        // *source* dimensions change.  The scene resolves root and COW
        // storage by xid on every frame (see `build_scene` → `store.lookup(
        // core.window_id)`), so the reallocated storage above will be picked
        // up automatically on the next compose tick.
        //
        // Crucially, we must NOT call `drain_all` + `rebuild_outputs` here.
        // Those paths clear the scene's `pending_acks` queue (the per-output
        // "flip in flight" gate) but do NOT drain the kernel's DRM event
        // queue.  If output N has a pending atomic flip when we call
        // drain_all, the scene thinks the CRTC is free and immediately
        // attempts a new commit on the next tick — but the kernel still has
        // the old flip pending, returning EBUSY.  This is what caused the
        // observed "output 1 freezes black after RRSetScreenSize" on a live
        // 2-monitor session.
        //
        // The existing per-output flip-pending gate (`pending_acks.is_empty()`
        // in `tick_one_output`) already prevents EBUSY: if a flip is in
        // flight the tick skips that output until the page-flip-complete event
        // arrives.  So the correct fix is simply to defer to that gate.
        //
        // Old storage (root/COW) is released safely: `store_decref_with_
        // invalidate` parks the DrawableId in `pending_retire` if the GPU
        // fence has not yet signaled, deferring the VkImage destroy until the
        // compose CB finishes — no `wait_idle_bounded` needed.
        //
        // root-overlay is root-absolute + layout-dependent; drop it on
        // topology change. `rebuild_outputs` isn't called here (see above),
        // so this logical-resize path needs its own explicit clear.
        self.request_direct_unflip("virtual_screen_extent_complete");
        self.scene.root_overlay_clear();
        // Step 3 — the root/COW storage under every scanout BO just changed
        // size, while the BOs themselves stay valid. Nothing else tells the
        // per-BO damage model that: the `RRSetScreenSize` caller deliberately
        // skips `drain_all` + `rebuild_outputs` (see above), and
        // `wake_for_damage` adds no region.
        //
        // For the hotplug caller this is belt-and-braces rather than load
        // bearing: `fire_randr_changes` grows the extent only on a topology
        // change, which has already run `scene.rebuild_outputs`, and that
        // replaces EVERY output's `ScanoutDamage` with a fresh one whose every
        // BO starts wholly missing (`ScanoutDamage::new`) — the relit output
        // and the survivors alike. It is kept unconditional because it is the
        // storage reallocation, not the scene rebuild, that makes the per-BO
        // history meaningless, and the cost is at most one full repaint that
        // was already going to happen.
        self.scene.invalidate_all_scanout_damage();
        self.scene.wake_for_damage();

        Ok(())
    }

    /// Extent of the root drawable's backing storage, or `None` on a
    /// fixture whose root xid is not in the store.
    pub(in crate::kms::render::backend) fn root_storage_extent(&self) -> Option<ash::vk::Extent2D> {
        let id = self.store.lookup(self.core.window_id)?;
        self.store.get(id).map(|drawable| drawable.storage.extent)
    }

    /// Rectangles held by routes that are physically gone but restorable.
    ///
    /// The reservation record *is* [`ConnectorEntry::last_enabled`], so a
    /// slot is released exactly by clearing that field — there is no second
    /// record that could go out of step with it. A remembered route that is
    /// live again reserves nothing, which is why the live output list is
    /// subtracted here rather than tracked separately.
    pub(in crate::kms::render::backend) fn reserved_layout_slots(
        &self,
    ) -> Vec<crate::kms::render::platform::LayoutRect> {
        let live: HashSet<&OutputKey> = self
            .platform
            .outputs
            .iter()
            .map(|layout| &layout.key)
            .collect();
        let mut reserved: Vec<_> = self
            .randr_id_alloc
            .entries()
            .filter(|(key, _)| !live.contains(key))
            .filter_map(|(_, entry)| entry.last_enabled?.placed_rect())
            .collect();
        // `entries()` walks a HashMap; keep the result order stable so the
        // packing decision does not depend on hash iteration order.
        reserved.sort_unstable();
        reserved
    }

    /// Decide which remembered routes this rescan restores, and release the
    /// slots of the ones it can no longer restore.
    ///
    /// Registry-only — no DRM object is touched — so the whole P1 policy is
    /// deterministic and unit testable. A returning connector whose refreshed
    /// mode list still advertises the remembered mode is restored; one whose
    /// refreshed list no longer does is left connected-but-Off with
    /// `last_enabled` cleared, which both retires the route and releases its
    /// reserved slot so the next compaction can reclaim it. A connector that
    /// has not come back keeps its reservation untouched.
    pub(in crate::kms::render::backend) fn take_relight_requests(&mut self) -> Vec<RelightRequest> {
        let mut restore: Vec<RelightRequest> = Vec::new();
        let mut stale: Vec<OutputKey> = Vec::new();
        for (key, entry) in self.randr_id_alloc.entries() {
            let Some(route) = entry
                .last_enabled
                .and_then(ConnectorConfig::restorable_route)
            else {
                continue;
            };
            if !entry.connected {
                // Still away. Keep the reservation; nothing to relight yet.
                continue;
            }
            if !matches!(entry.config, ConnectorConfig::Off) {
                continue;
            }
            if entry.modes.iter().any(|mode| {
                mode.width == route.mode.width
                    && mode.height == route.mode.height
                    && mode.vrefresh == route.mode.vrefresh
            }) {
                restore.push(RelightRequest {
                    key: key.clone(),
                    mode: route.mode,
                    x: route.x,
                    y: route.y,
                });
            } else {
                stale.push(key.clone());
            }
        }
        for key in stale {
            log::info!(
                "kms: {} on {} returned without its previous mode; leaving it off and \
                 releasing its reserved slot",
                key.connector_name,
                key.device_key,
            );
            self.randr_id_alloc.entry_mut(&key).last_enabled = None;
        }
        // `entries()` walks a HashMap; relight in a deterministic order.
        restore.sort_by(|a, b| a.key.cmp(&b.key));
        restore
    }

    /// Restore one remembered route through the ordinary enable path — the
    /// same `enable_connector` a client `SetCrtcConfig` drives, so pool
    /// allocation, the modeset and the `ActiveOutput` update are all handled.
    ///
    /// The caller must have quiesced the old topology first. Returns whether
    /// the output is scanning out again.
    fn relight_remembered_route(&mut self, request: &RelightRequest) -> bool {
        let connector = request.key.connector_name.clone();
        let Some(device) = self
            .platform
            .device_for_output(&request.key)
            .map(|kms| Rc::clone(&kms.device))
        else {
            log::warn!(
                "kms: relight of {connector} skipped: DRM device {} is gone",
                request.key.device_key,
            );
            return false;
        };
        let reserved_routes: Vec<_> = self
            .platform
            .outputs
            .iter()
            .filter(|layout| {
                layout.key.device_key == request.key.device_key && layout.key != request.key
            })
            .map(|layout| {
                (
                    layout.output.encoder,
                    layout.output.crtc,
                    layout.output.plane,
                )
            })
            .collect();
        let output = match crate::platform::drm::discover_output_for_connector(
            &device,
            &connector,
            &reserved_routes,
        ) {
            Ok(output) => output,
            Err(error) => {
                log::error!("kms: relight of {connector}: target discovery failed: {error}");
                return false;
            }
        };
        if let Err(error) =
            self.platform
                .enable_connector(&request.key, output, request.mode, request.x, request.y)
        {
            log::error!("kms: relight of {connector}: enable_connector failed: {error}");
            return false;
        }
        self.commit_relit_route(request);
        log::info!(
            "kms: relit {connector} {}x{}@{} at ({},{}) after reconnect",
            request.mode.width,
            request.mode.height,
            request.mode.vrefresh,
            request.x,
            request.y,
        );
        true
    }

    /// Record a route the relight has just put back on the hardware.
    ///
    /// Releases the reservation (clearing `last_enabled` *is* the release) and
    /// deliberately does **not** set `client_configured`: an auto-relight
    /// restores a previous state, it does not record a new client intent, so
    /// the auto-layout stays free to move this output later.
    pub(in crate::kms::render::backend) fn commit_relit_route(&mut self, request: &RelightRequest) {
        let entry = self.randr_id_alloc.entry_mut(&request.key);
        entry.config = ConnectorConfig::Enabled {
            mode_w: request.mode.width,
            mode_h: request.mode.height,
            vrefresh: request.mode.vrefresh,
            x: request.x,
            y: request.y,
        };
        entry.connected = true;
        entry.last_enabled = None;
    }

    pub(in crate::kms::render::backend) fn run_display_rescan(&mut self, state: &mut ServerState) {
        // Defer while VT-suspended: DRM master is dropped, so a rescan's
        // modeset ioctls would fail/wedge. (The old guard also required
        // VT switching drops DRM master, so gate rescans on `vt_state`.
        if self.vt_state != crate::vt::state::VtState::Active {
            log::debug!("kms: display rescan skipped (VT not Active)");
            return;
        }
        // Gather first. A card-level probe error aborts the combined rescan
        // without touching the last-known topology.
        let snapshot = match self.platform.probe_connector_snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                log::error!("kms: display rescan probe failed: {error}");
                return;
            }
        };
        let active_removed = self.platform.outputs.iter().any(|output| {
            !snapshot
                .iter()
                .any(|entry| entry.preserves_active_output(output))
        });

        // Only an active removal can drop a pool or invalidate the scene's
        // output-index ledger. Metadata and inactive-connector changes leave
        // normal composition and direct scanout undisturbed.
        let mut quiesced = false;
        if active_removed {
            match self.quiesce_before_topology_mutation("display hotplug rescan") {
                Ok(()) => quiesced = true,
                Err(error) => {
                    log::error!("kms: display rescan could not quiesce old topology: {error}");
                    return;
                }
            }
        }

        // The five steps below are ordered so that clients never observe the
        // intermediate output-less state: the relight sits between registry
        // reconciliation and publication, and layout policy runs only once
        // the relight decision is known. See the design's
        // "Ordering inside `run_display_rescan` is load-bearing".

        // ── 1. Apply the physical snapshot (topology ownership only) ──────
        let configured = self.randr_id_alloc.client_configured_keys();
        let known_connected = self.randr_id_alloc.connected_keys();
        let rescan = self
            .platform
            .apply_connector_snapshot(snapshot, &known_connected);

        // ── 2. Reconcile the registry (connection bits, modes, EDID) ──────
        let registry_delta = self.reconcile_connector_registry(
            &rescan.connected,
            &rescan.dropped_keys,
            &rescan.dropped_layouts,
        );

        // ── 3. Relight every route lost to a physical disconnect whose
        //       connector has returned with a compatible mode. Not gated on
        //       `client_configured`: a boot auto-layout session never sets it
        //       and is exactly the reported configuration.
        let relight_requests = self.take_relight_requests();
        let mut relit = false;
        if !relight_requests.is_empty() {
            if !quiesced {
                match self.quiesce_before_topology_mutation("display hotplug relight") {
                    Ok(()) => quiesced = true,
                    Err(error) => {
                        log::error!(
                            "kms: display rescan could not quiesce before relight: {error}"
                        );
                        return;
                    }
                }
            }
            for request in relight_requests {
                relit |= self.relight_remembered_route(&request);
            }
        }

        // ── 4. Layout policy: pack the auto-layout outputs around the slots
        //       still reserved by departed-but-restorable routes, then take
        //       the extent over the live layouts unioned with those slots.
        let reserved = self.reserved_layout_slots();
        if !rescan.dropped_old_indices.is_empty() {
            self.platform
                .recompact_horizontal_layout(&configured, &reserved);
        }
        if !rescan.dropped_old_indices.is_empty() || relit {
            // `enable_connector` already recomputed the extent over the live
            // layouts alone; redo it so surviving reservations are counted.
            self.platform
                .recompute_fb_extent_with_reservations(&reserved);
        }

        // ── 5. Publish ────────────────────────────────────────────────────
        // `quiesced` subsumes `active_removed`. Publishing is also what
        // re-lights the topology we tore down, so a quiesce whose relight then
        // failed must still go through `fire_randr_changes` rather than
        // returning with every CRTC dark.
        let should_publish = !registry_delta.is_empty() || quiesced || relit;
        if !should_publish {
            log::debug!("kms: display rescan found no connector or active-topology change");
            return;
        }
        if !self.fire_randr_changes(
            state,
            rescan,
            &registry_delta.changed_keys,
            registry_delta.config_changed,
            active_removed || relit,
            quiesced && state.dpms.power_level == 0,
        ) {
            return;
        }
        // A retired connector's CRTC is gone; drop any stale armed entry so it
        // cannot block re-arm or mis-dedup a reused id.
        self.prune_armed_targets_to_live_outputs();
    }
}

// ───────────────────────────────────────────────────────────────
// `Backend` trait implementation. The shape:
//
// A. Pure accessors — return values from `self.core` or local
//    constants identical to v1.
// B. Bookkeeping mutations — mutate `self.core` (XID map etc.).
// C. Mixed bookkeeping + storage — log a gap; for ops that must
//    return a handle, mint a fresh xid via `self.core.next_host_xid()`
//    so subsequent xid_map lookups stay consistent.
// D. Paint / RENDER / scene — log a gap, return Ok or the
//    default-impl shape.
// ───────────────────────────────────────────────────────────────

impl KmsBackend {
    pub(in crate::kms::render::backend) fn live_crtc_and_gamma_size(
        &self,
        output_key: &OutputKey,
    ) -> io::Result<
        Option<(
            crate::platform::drm::DrmDeviceKey,
            ::drm::control::crtc::Handle,
            u16,
        )>,
    > {
        use ::drm::control::Device as ControlDevice;

        let Some(layout) = self
            .platform
            .outputs
            .iter()
            .find(|layout| layout.key == *output_key)
        else {
            return Ok(None);
        };
        let crtc = layout.output.crtc;
        let Some(device) = self.platform.device_for_output(&layout.key) else {
            return Ok(None);
        };
        let info = device.device.get_crtc(crtc).map_err(|e| {
            io::Error::other(format!(
                "get_crtc gamma size for {output_key:?} failed: {e}"
            ))
        })?;
        let size = u16::try_from(info.gamma_length()).unwrap_or(u16::MAX);
        Ok(Some((layout.key.device_key, crtc, size)))
    }

    pub(in crate::kms::render::backend) fn nominal_gamma_size(
        &self,
        output_key: &OutputKey,
    ) -> u16 {
        match self.live_crtc_and_gamma_size(output_key) {
            Ok(Some((_, _, size))) => size,
            Ok(None) => self
                .gamma_luts
                .borrow()
                .get(output_key)
                .map(|lut| u16::try_from(lut.len()).unwrap_or(u16::MAX))
                .unwrap_or(256),
            Err(e) => {
                log::warn!("kms gamma: {output_key:?} gamma-size query failed: {e}");
                self.gamma_luts
                    .borrow()
                    .get(output_key)
                    .map(|lut| u16::try_from(lut.len()).unwrap_or(u16::MAX))
                    .unwrap_or(0)
            }
        }
    }

    pub(in crate::kms::render::backend) fn cached_gamma(&self, output_key: &OutputKey) -> GammaLut {
        if let Some(lut) = self.gamma_luts.borrow().get(output_key).cloned() {
            return lut;
        }
        let lut = GammaLut::identity(self.nominal_gamma_size(output_key));
        self.gamma_luts
            .borrow_mut()
            .insert(output_key.clone(), lut.clone());
        lut
    }

    pub(in crate::kms::render::backend) fn cached_gamma_for_current_size(
        &self,
        output_key: &OutputKey,
        size: u16,
    ) -> GammaLut {
        let lut = self.cached_gamma(output_key);
        if lut.len() == usize::from(size) {
            return lut;
        }
        let resampled = lut.resampled(size);
        self.gamma_luts
            .borrow_mut()
            .insert(output_key.clone(), resampled.clone());
        resampled
    }

    pub(in crate::kms::render::backend) fn apply_gamma_to_live_output(
        &self,
        output_key: &OutputKey,
    ) -> io::Result<()> {
        use ::drm::control::Device as ControlDevice;

        let Some((device_key, crtc, gamma_size)) = self.live_crtc_and_gamma_size(output_key)?
        else {
            return Ok(());
        };
        if gamma_size == 0 {
            return Ok(());
        }
        let lut = self.cached_gamma_for_current_size(output_key, gamma_size);
        let Some(device) = self.platform.device_for_key(device_key) else {
            return Ok(());
        };
        device
            .device
            .set_gamma(crtc, &lut.red, &lut.green, &lut.blue)
            .map_err(|e| io::Error::other(format!("set_gamma for {output_key:?} failed: {e}")))
    }

    fn reapply_gamma_for_output(&self, output_key: &OutputKey) {
        if let Err(e) = self.apply_gamma_to_live_output(output_key) {
            log::warn!("kms gamma: reapply for {output_key:?} failed: {e}");
        }
    }

    pub(in crate::kms::render::backend) fn reapply_gamma_for_live_outputs(&self) {
        let output_keys: Vec<OutputKey> = self
            .platform
            .outputs
            .iter()
            .map(|layout| layout.key.clone())
            .collect();
        for output_key in output_keys {
            self.reapply_gamma_for_output(&output_key);
        }
    }
}

impl KmsBackend {
    pub(in crate::kms::render::backend) fn backend_randr_crtc_gamma_size(&self, crtc: u32) -> u16 {
        self.crtc_key_by_id
            .get(&crtc)
            .map_or(0, |output_key| self.nominal_gamma_size(output_key))
    }

    pub(in crate::kms::render::backend) fn backend_randr_set_crtc_gamma(
        &mut self,
        crtc: u32,
        red: &[u16],
        green: &[u16],
        blue: &[u16],
    ) -> io::Result<()> {
        let output_key = self.crtc_key_by_id.get(&crtc).cloned().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("unknown RANDR CRTC 0x{crtc:x}"),
            )
        })?;
        let expected = usize::from(self.crtc_gamma_size(crtc));
        if red.len() != expected || green.len() != expected || blue.len() != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "CRTC 0x{crtc:x} ({output_key:?}): gamma length mismatch (expected {expected}, got {}/{}/{})",
                    red.len(),
                    green.len(),
                    blue.len(),
                ),
            ));
        }
        self.gamma_luts.borrow_mut().insert(
            output_key.clone(),
            GammaLut {
                red: red.to_vec(),
                green: green.to_vec(),
                blue: blue.to_vec(),
            },
        );
        self.apply_gamma_to_live_output(&output_key)
    }

    pub(in crate::kms::render::backend) fn backend_randr_get_crtc_gamma(
        &self,
        crtc: u32,
    ) -> (Vec<u16>, Vec<u16>, Vec<u16>) {
        let Some(output_key) = self.crtc_key_by_id.get(&crtc) else {
            return (Vec::new(), Vec::new(), Vec::new());
        };
        let lut = match self.live_crtc_and_gamma_size(output_key) {
            Ok(Some((_, _, size))) => self.cached_gamma_for_current_size(output_key, size),
            Ok(None) => self.cached_gamma(output_key),
            Err(e) => {
                log::warn!("kms gamma: {output_key:?} get gamma size failed: {e}");
                self.cached_gamma(output_key)
            }
        };
        (lut.red, lut.green, lut.blue)
    }

    pub(in crate::kms::render::backend) fn backend_randr_on_display_hotplug(
        &mut self,
        _state: &mut ServerState,
    ) {
        #[cfg(target_os = "linux")]
        {
            let saw_change = self
                .platform
                .hotplug_monitor
                .as_mut()
                .map(|monitor| monitor.drain())
                .unwrap_or(false);
            if saw_change {
                self.hotplug_rescan_deadline =
                    Some(std::time::Instant::now() + std::time::Duration::from_millis(150));
                log::debug!("kms: display hotplug edge — rescan armed (+150ms)");
            }
        }
    }

    pub(in crate::kms::render::backend) fn backend_randr_reprobe_connectors(
        &mut self,
        state: &mut ServerState,
    ) -> io::Result<()> {
        // RANDR's forced resource refresh only needs connector presence and
        // mode lists. Full `discover_outputs` also enumerates planes,
        // properties and modifiers and computes hypothetical assignments;
        // under Cinnamon/GPU load that unrelated work blocked dispatch for
        // 90–113 ms every time the desktop polled GetScreenResources.
        let probes = self.platform.probe_all_connectors()?;
        let _ = self.publish_connector_probes(state, &probes);
        Ok(())
    }

    pub(in crate::kms::render::backend) fn backend_randr_set_provider_output_source(
        &mut self,
        state: &mut ServerState,
        provider: u32,
        source_provider: Option<u32>,
    ) -> io::Result<bool> {
        let invalid = |message: String| io::Error::new(io::ErrorKind::InvalidInput, message);
        let sink_endpoint = self
            .current_provider_endpoint_for_id(provider)
            .ok_or_else(|| invalid(format!("unknown or inactive RANDR provider id {provider}")))?;
        let RandrProviderEndpoint::Kms(sink_key) = sink_endpoint else {
            return Err(invalid(format!(
                "RANDR provider {provider} ({sink_endpoint:?}) is not a KMS output sink",
            )));
        };
        let sink_has_connector_inventory = self
            .randr_id_alloc
            .entries()
            .any(|(key, _)| key.device_key == sink_key);
        if !sink_has_connector_inventory {
            return Err(invalid(format!(
                "RANDR provider {provider} ({sink_endpoint:?}) has no connector inventory and is not an output sink",
            )));
        }

        let selected_source = self.selected_render_provider_endpoint();
        if selected_source == Some(sink_endpoint) {
            return Err(invalid(format!(
                "RANDR provider {provider} is the selected renderer's coalesced KMS endpoint; same-device scanout is implicit and it cannot be an output sink",
            )));
        }
        let requested_source = source_provider
            .map(|source_id| {
                self.current_provider_endpoint_for_id(source_id)
                    .ok_or_else(|| {
                        invalid(format!(
                            "unknown or inactive RANDR source provider id {source_id}",
                        ))
                    })
            })
            .transpose()?;
        if let Some(source) = requested_source
            && Some(source) != selected_source
        {
            return Err(invalid(format!(
                "RANDR source provider {} ({source:?}) is not the selected operational renderer {:?}",
                source_provider.expect("requested source has an XID"),
                selected_source,
            )));
        }

        let current_source = self.provider_output_sources.get(&sink_key).copied();
        let source_changes = requested_source != current_source;
        if !source_changes {
            return Ok(false);
        }

        // `platform.outputs` is the authoritative lifetime inventory even
        // while DPMS is off or the VT is suspended: those routes still own
        // scanout pools and can be re-lit. Never revoke or replace their source
        // policy in place.
        if current_source.is_some() {
            let active_connectors: Vec<_> = self
                .platform
                .outputs
                .iter()
                .filter(|output| output.key.device_key == sink_key)
                .map(|output| format!("{} on {}", output.key.connector_name, output.key.device_key))
                .collect();
            if !active_connectors.is_empty() {
                return Err(invalid(format!(
                    "cannot change PRIME Output Source for active sink provider {provider}: {}",
                    active_connectors.join(", "),
                )));
            }
        }

        match requested_source {
            Some(source) => {
                self.provider_output_sources.insert(sink_key, source);
                log::info!(
                    "PRIME Output Source: sink provider {provider} ({sink_endpoint:?}) -> source provider {} ({source:?})",
                    source_provider.expect("attached source has an XID"),
                );
            }
            None => {
                // Startup auto-association is one-shot. Absence therefore
                // records the client's explicit detach for the rest of this
                // backend lifetime; registry rebuilds never repopulate it.
                self.provider_output_sources.remove(&sink_key);
                log::info!(
                    "PRIME Output Source: detached sink provider {provider} ({sink_endpoint:?})"
                );
            }
        }
        self.bump_crtc_config_topology_epoch("PRIME provider output source changed");

        // A provider relationship changes neither the CRTC configuration nor
        // available connector/mode inventory. Rebuild only the projection and
        // preserve both lastSetTime and configTimestamp.
        self.rebuild_randr_state(state, None, false);
        Ok(true)
    }

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
        if let Err(error) = self.scene.rebuild_outputs(&self.platform) {
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
        if let Err(e) = self.scene.rebuild_outputs(&self.platform) {
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

    pub(in crate::kms::render::backend) fn backend_randr_randr_layout_changed(
        &mut self,
        state: &mut ServerState,
    ) {
        // The root extent is the client's (spec D3, "Two extents"); a CRTC
        // set recomputes `fb_w`/`fb_h` from the modes, which is neither the
        // root nor any footprint.
        let root = (
            state.randr.screen_width.max(1),
            state.randr.screen_height.max(1),
        );
        let root_changed = root != (self.platform.fb_w, self.platform.fb_h);
        if root_changed {
            (self.platform.fb_w, self.platform.fb_h) = root;
            self.update_input_extent(root.0, root.1);
        }
        // Rotation and reflection combined with the client transform, as
        // `RRTransformCompute`: one matrix for footprint, pass and readback.
        let transforms: HashMap<OutputKey, yserver_core::randr::CrtcTransform> = state
            .randr
            .outputs
            .iter()
            .map(|o| (o, o.crtc_transform()))
            .filter(|(_, t)| !t.is_identity())
            .filter_map(|(o, t)| {
                let key = self.output_key_by_id.get(&o.output_id)?.clone();
                Some((key, t))
            })
            .collect();
        let transforms_changed = transforms != self.platform.output_transforms;
        if transforms_changed {
            // No transformed CRTC is ever flipped directly (spec D5).
            self.request_direct_unflip("crtc_transform_changed");
            self.platform.output_transforms = transforms;
            log::info!(
                "kms: CRTC transforms now on {} output(s)",
                self.platform.output_transforms.len()
            );
        }
        if root_changed || transforms_changed {
            if let Err(error) = self.scene.sync_output_layouts(&self.platform) {
                log::error!("kms: scene could not follow the RANDR layout: {error}");
            }
            self.scene.wake_for_damage();
        }
        self.move_pointer_to_nearest_crtc(state);
    }

    pub(in crate::kms::render::backend) fn backend_randr_set_logical_screen_size(
        &mut self,
        w: u16,
        h: u16,
    ) -> io::Result<()> {
        let w = w.max(1);
        let h = h.max(1);
        if (w, h) == (self.platform.fb_w, self.platform.fb_h) {
            return Ok(());
        }

        self.apply_virtual_screen_extent(w, h)?;
        log::info!("render set_logical_screen_size: resized virtual screen to {w}×{h}");
        Ok(())
    }
}
