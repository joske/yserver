use crate::{
    kms::{
        backend::OutputKey,
        render::{
            backend::{
                ConnectorConfig, ConnectorRegistryDelta, GammaLut, KmsBackend, NotLiveConnector,
                ProjectedRandrOutputState, RandrIdAllocator, RandrProviderEndpoint,
            },
            platform::ConnectorSnapshot,
        },
    },
    platform::drm::ModeIdentity,
};
use std::{
    collections::{HashMap, HashSet},
    io,
};
use yserver_core::{backend::Backend as _, server::ServerState};

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
        // A disconnected output advertises no modes (Xorg
        // `xf86ProbeOutputModes`, xf86Crtc.c:1688-1691, deletes the probed
        // list and skips probing). A live CRTC keeps its current mode in the
        // screen's mode table through the projection instead.
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
            if !entry.modes.is_empty() {
                entry.modes.clear();
                entry_changed = true;
                config_changed = true;
            }
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
    // disconnected. Their mode lists are dropped (as above); their live and
    // client configuration is retained.
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
            if !entry.modes.is_empty() {
                entry.modes.clear();
                entry_changed = true;
                config_changed = true;
            }
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
    pub(super) fn seed_initial_connector_topology(&mut self) -> io::Result<()> {
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
            let _ = self.pstate.randr_id_alloc.ids_for(key);
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
                let _ = self.pstate.randr_id_alloc.ids_for(&key);
            }
        }
        // Entries seen only by cached inventory remain default-disconnected.
        let _ = self.reconcile_connector_registry(snapshots, &[]);

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
            let entry = self.pstate.randr_id_alloc.entry_mut(&key);
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
    pub(in crate::kms::render::backend) fn reconcile_connector_registry(
        &mut self,
        connected: &[ConnectorSnapshot],
        dropped: &[OutputKey],
    ) -> ConnectorRegistryDelta {
        let mut delta = ConnectorRegistryDelta::default();
        let connected_keys: HashSet<_> = connected.iter().map(|snapshot| &snapshot.key).collect();
        for key in dropped {
            let still_live = self
                .platform
                .outputs
                .iter()
                .any(|layout| &layout.key == key);
            let entry = self.pstate.randr_id_alloc.entry_mut(key);
            if connected_keys.contains(key) {
                // Physically connected but no longer usable by the live CRTC
                // (for example, its current mode vanished). The connected
                // snapshot below owns advertised state; this arm retires only
                // the route policy, so do not fabricate a disconnect/reconnect
                // pair or a second config timestamp.
                entry.crtc_associated |= matches!(entry.config, ConnectorConfig::Enabled { .. });
                entry.config = ConnectorConfig::Off;
                entry.client_configured = false;
                continue;
            }
            // Physically disconnected while its route is still live. Xorg's
            // modesetting driver issues no commit on a disconnect: the CRTC
            // keeps its mode, position and outputs and keeps scanning out
            // (`drmmode_update_kms_state`, drmmode_display.c:3965, ignores the
            // detect result). `xf86ProbeOutputModes` (xf86Crtc.c:1688-1691)
            // then reports the output Disconnected with no probed modes and
            // clears its EDID. Keep the client/auto-layout config untouched.
            if still_live {
                let config_changed = entry.connected || !entry.modes.is_empty();
                if config_changed
                    || !entry.edid.is_empty()
                    || entry.mm_width != 0
                    || entry.mm_height != 0
                {
                    delta.changed_keys.push(key.clone());
                }
                delta.config_changed |= config_changed;
                entry.crtc_associated = true;
                entry.connected = false;
                entry.modes.clear();
                entry.edid.clear();
                entry.mm_width = 0;
                entry.mm_height = 0;
                continue;
            }
            // A connector without a live route physically departed.
            let config_changed = entry.connected || !entry.modes.is_empty();
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
            // Retain stable XIDs and connector-owned type, but, like Xorg, do
            // not advertise the departed monitor's modes or identity while
            // disconnected.
            entry.modes.clear();
            entry.edid.clear();
            entry.mm_width = 0;
            entry.mm_height = 0;
        }
        for snapshot in connected {
            let entry = self.pstate.randr_id_alloc.entry_mut(&snapshot.key);
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
                reconcile_connector_probe(&mut self.pstate.randr_id_alloc, *device_key, connectors);
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
                    self.pstate
                        .randr_id_alloc
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
        let endpoint = self
            .pstate
            .randr_id_alloc
            .provider_endpoint_for_id(provider_id)?;
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
        source == sink || self.pstate.provider_output_sources.get(&kms_device_key) == Some(&source)
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
            if let Some(&previous) = self.pstate.provider_output_sources.get(&device.key) {
                if previous != source {
                    return Err(io::Error::other(format!(
                        "automatic KMS sink {} has conflicting sources {previous:?} and {source:?}",
                        device.key,
                    )));
                }
                continue;
            }
            self.pstate
                .provider_output_sources
                .insert(device.key, source);
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
            let _ = self.pstate.randr_id_alloc.provider_id_for(endpoint);
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
                let id = self.pstate.randr_id_alloc.provider_id_for(endpoint);
                (endpoint, id)
            })
            .collect();
        let kms_with_connector_inventory: HashSet<_> = self
            .pstate
            .randr_id_alloc
            .entries()
            .map(|(key, _)| key.device_key)
            .collect();
        let mut associations_by_endpoint: HashMap<
            RandrProviderEndpoint,
            Vec<RandrProviderAssociation>,
        > = HashMap::new();
        if let Some(source) = selected_endpoint {
            for (&sink_key, &configured_source) in &self.pstate.provider_output_sources {
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
            for (key, entry) in self.pstate.randr_id_alloc.entries() {
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

        // A disconnect (lightweight or heavy) leaves the registry config
        // Enabled, so the live CRTC remains projected with its mode and
        // position while `connected` flips false (Xorg parity). A route
        // retired for lack of a usable mode (and a startup force-probe miss)
        // sets config Off; do not let a stale `platform.outputs` row revive
        // that route.
        let live_keys: HashSet<OutputKey> = self
            .platform
            .outputs
            .iter()
            .filter(|layout| {
                self.pstate
                    .randr_id_alloc
                    .entry(&layout.key)
                    .is_none_or(|entry| matches!(entry.config, ConnectorConfig::Enabled { .. }))
            })
            .map(|layout| layout.key.clone())
            .collect();
        let mut outs: Vec<RandrOutput> = Vec::with_capacity(
            self.platform.outputs.len() + self.pstate.randr_id_alloc.known_connectors().len(),
        );
        // Rebuild the per-output identity map (EDID + ConnectorType) each
        // projection so the output-property handlers serve current data.
        self.pstate.output_identity_by_id.clear();
        self.pstate.output_key_by_id.clear();
        self.pstate.crtc_key_by_id.clear();
        for layout in self
            .platform
            .outputs
            .iter()
            .filter(|layout| live_keys.contains(&layout.key))
        {
            let vrefresh = layout.output.picked.vrefresh;
            let output_key = layout.key.clone();
            let registry_entry_exists = self.pstate.randr_id_alloc.entry(&output_key).is_some();
            let ids = self.pstate.randr_id_alloc.ids_for(&output_key);
            let (connected, connector_modes, edid, mm_width, mm_height, connector_type) = {
                let entry = self.pstate.randr_id_alloc.entry_mut(&output_key);
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
            self.pstate
                .output_key_by_id
                .insert(ids.output_id, output_key.clone());
            self.pstate.crtc_key_by_id.insert(ids.crtc_id, output_key);
            let mode_id = self.pstate.randr_id_alloc.mode_id(&layout.output.picked);
            if connected {
                self.pstate
                    .output_identity_by_id
                    .insert(ids.output_id, (edid, connector_type));
            }
            let mut mode_ids = Vec::with_capacity(connector_modes.len());
            let mut num_preferred: u16 = 0;
            for mode in &connector_modes {
                let mode_id = self.pstate.randr_id_alloc.mode_id(mode);
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
            .pstate
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
            let mode_id = self.pstate.randr_id_alloc.mode_id(&mode);
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
        // A disconnected connector advertises no modes (the registry clears
        // them), matching Xorg's `xf86ProbeOutputModes`.
        // Collect owned first to release the &self borrow before the
        // &mut self mode_id() allocations below.
        let not_live: Vec<NotLiveConnector> = self
            .pstate
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
                let mode_id = self.pstate.randr_id_alloc.mode_id(&mode);
                // See the live-output loop: list each exact XID once.
                if mode_ids.contains(&mode_id) {
                    continue;
                }
                mode_ids.push(mode_id);
                if mode.preferred {
                    num_preferred = num_preferred.saturating_add(1);
                }
            }
            self.pstate
                .output_key_by_id
                .insert(connector.ids.output_id, connector.key.clone());
            self.pstate
                .crtc_key_by_id
                .insert(connector.ids.crtc_id, connector.key.clone());
            if connector.connected {
                self.pstate.output_identity_by_id.insert(
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
    /// covers the live outputs, which include a disconnected monitor's
    /// still-configured CRTC (#273).
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
        let new_ts = set_time.unwrap_or(prev_ts);
        let ts_now = state.timestamp_now();
        state.randr =
            yserver_core::randr::RandrState::from_outputs_with_modes(new_ts, outputs, mode_table);
        // CRTC transforms are client-owned, like the logical size below.
        state.randr.restore_crtc_transforms(prev_transforms);
        let associations = self
            .pstate
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
            self.pstate.scanout_m1.clear("output topology changed");
        }

        if rebuild_scene
            && let Err(e) = self
                .scene
                .rebuild_outputs(&self.platform, &self.platform.scene_output_layouts())
        {
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
                self.pstate
                    .randr_id_alloc
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
    /// 0. The new root/COW storage is allocated and filled under
    ///    placeholder xids (`prepare_root_storage`), nothing published yet.
    /// 1. An active direct frame is snapshotted into its old COW and
    ///    unflipped -- publishing new root/COW storage changes the
    ///    fallback identity that frame holds.
    /// 2. Only then are `fb_w`/`fb_h`, the input extent and the root/COW
    ///    storage replaced.
    /// 3. The per-BO scanout damage model is invalidated, because the
    ///    storage under every scanout BO just changed while the BOs
    ///    themselves stayed valid.
    ///
    /// # Errors
    ///
    /// The new storage could not be allocated or filled, or an active
    /// direct frame could not be snapshotted. The old extent and every
    /// storage owner/pin are then left untouched.
    pub(in crate::kms::render::backend) fn apply_virtual_screen_extent(
        &mut self,
        w: u16,
        h: u16,
    ) -> io::Result<()> {
        // Every fallible storage step first: on failure nothing changed.
        let prepared = self.prepare_root_storage(w, h)?;

        // Reallocating root/COW storage changes the fallback identity held by
        // an active direct frame. Snapshot that frame into its old COW and
        // request the synchronized replacement first. On failure, leave the
        // old dimensions and every storage owner/pin untouched.
        if self.pstate.scanout_m2.active() {
            if let Err(e) = self.materialize_direct_shadow_for_unflip() {
                self.discard_root_storage(prepared);
                return Err(e);
            }
            self.request_direct_unflip("virtual_screen_extent_before_storage_reallocation");
        }

        // ── 1. Update the platform's logical extent ───────────────────────
        self.bump_crtc_config_topology_epoch("virtual screen extent changed");
        self.platform.fb_w = w;
        self.platform.fb_h = h;

        // Propagate the new extent to the input thread's absolute mapper so
        // absolute devices can reach the full virtual screen after a resize.
        self.update_input_extent(w, h);

        // ── 2. Root/COW storage and scene damage, shared with the surface ──
        self.commit_root_storage(prepared);

        Ok(())
    }

    /// Extent of the root drawable's backing storage, or `None` on a
    /// fixture whose root xid is not in the store.
    pub(in crate::kms::render::backend) fn root_storage_extent(&self) -> Option<ash::vk::Extent2D> {
        let id = self.store.lookup(self.core.window_id)?;
        self.store.get(id).map(|drawable| drawable.storage.extent)
    }

    pub(super) fn run_display_rescan(&mut self, state: &mut ServerState) {
        // Defer while VT-suspended: DRM master is dropped, so a rescan's
        // modeset ioctls would fail/wedge. (The old guard also required
        // VT switching drops DRM master, so gate rescans on `vt_state`.
        if self.pstate.vt_state != crate::vt::state::VtState::Active {
            log::debug!("kms: display rescan skipped (VT not Active)");
            return;
        }
        // Gather first. A card-level probe error aborts the combined rescan
        // without touching the last-known topology.
        // The same physical-topology probe VT resume uses (and its test seam).
        let snapshot = match self
            .pstate
            .vt_resume_executor
            .probe_connector_snapshot(&self.platform)
        {
            Ok(snapshot) => snapshot,
            Err(error) => {
                log::error!("kms: display rescan probe failed: {error}");
                return;
            }
        };
        // A physical disconnect keeps its route (Xorg parity); only a
        // connected connector without a usable mode retires one.
        let active_removed = self.platform.snapshot_retires_any_active_output(&snapshot);

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

        // ── 1. Apply the physical snapshot (topology ownership only) ──────
        let configured = self.pstate.randr_id_alloc.client_configured_keys();
        let known_connected = self.pstate.randr_id_alloc.connected_keys();
        let rescan = self
            .platform
            .apply_connector_snapshot(snapshot, &known_connected);
        if quiesced {
            self.platform.invalidate_live_cursor_applied();
        }

        // ── 2. Reconcile the registry (connection bits, modes, EDID) ──────
        let registry_delta =
            self.reconcile_connector_registry(&rescan.connected, &rescan.dropped_keys);

        // ── 3. Layout policy: a retired route leaves a hole; repack the
        //       auto-layout outputs and recompute the extent.
        if !rescan.dropped_old_indices.is_empty() {
            self.platform.recompact_horizontal_layout(&configured);
            self.platform.recompute_fb_extent();
        }

        // ── 4. Publish ────────────────────────────────────────────────────
        // `quiesced` subsumes `active_removed`. Publishing is also what
        // re-lights the topology we tore down.
        let should_publish = !registry_delta.is_empty() || quiesced;
        if !should_publish {
            log::debug!("kms: display rescan found no connector or active-topology change");
            return;
        }
        if !self.fire_randr_changes(
            state,
            rescan,
            &registry_delta.changed_keys,
            registry_delta.config_changed,
            active_removed,
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
    fn live_crtc_and_gamma_size(
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

    fn nominal_gamma_size(&self, output_key: &OutputKey) -> u16 {
        match self.live_crtc_and_gamma_size(output_key) {
            Ok(Some((_, _, size))) => size,
            Ok(None) => self
                .pstate
                .gamma_luts
                .borrow()
                .get(output_key)
                .map(|lut| u16::try_from(lut.len()).unwrap_or(u16::MAX))
                .unwrap_or(256),
            Err(e) => {
                log::warn!("kms gamma: {output_key:?} gamma-size query failed: {e}");
                self.pstate
                    .gamma_luts
                    .borrow()
                    .get(output_key)
                    .map(|lut| u16::try_from(lut.len()).unwrap_or(u16::MAX))
                    .unwrap_or(0)
            }
        }
    }

    fn cached_gamma(&self, output_key: &OutputKey) -> GammaLut {
        if let Some(lut) = self.pstate.gamma_luts.borrow().get(output_key).cloned() {
            return lut;
        }
        let lut = GammaLut::identity(self.nominal_gamma_size(output_key));
        self.pstate
            .gamma_luts
            .borrow_mut()
            .insert(output_key.clone(), lut.clone());
        lut
    }

    fn cached_gamma_for_current_size(&self, output_key: &OutputKey, size: u16) -> GammaLut {
        let lut = self.cached_gamma(output_key);
        if lut.len() == usize::from(size) {
            return lut;
        }
        let resampled = lut.resampled(size);
        self.pstate
            .gamma_luts
            .borrow_mut()
            .insert(output_key.clone(), resampled.clone());
        resampled
    }

    fn apply_gamma_to_live_output(&self, output_key: &OutputKey) -> io::Result<()> {
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

    pub(super) fn reapply_gamma_for_output(&self, output_key: &OutputKey) {
        if let Err(e) = self.apply_gamma_to_live_output(output_key) {
            log::warn!("kms gamma: reapply for {output_key:?} failed: {e}");
        }
    }

    pub(super) fn reapply_gamma_for_live_outputs(&self) {
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
        self.pstate
            .crtc_key_by_id
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
        let output_key = self
            .pstate
            .crtc_key_by_id
            .get(&crtc)
            .cloned()
            .ok_or_else(|| {
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
        self.pstate.gamma_luts.borrow_mut().insert(
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
        let Some(output_key) = self.pstate.crtc_key_by_id.get(&crtc) else {
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
                self.pstate.hotplug_rescan_deadline =
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
            .pstate
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

        let current_source = self.pstate.provider_output_sources.get(&sink_key).copied();
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
                self.pstate.provider_output_sources.insert(sink_key, source);
                log::info!(
                    "PRIME Output Source: sink provider {provider} ({sink_endpoint:?}) -> source provider {} ({source:?})",
                    source_provider.expect("attached source has an XID"),
                );
            }
            None => {
                // Startup auto-association is one-shot. Absence therefore
                // records the client's explicit detach for the rest of this
                // backend lifetime; registry rebuilds never repopulate it.
                self.pstate.provider_output_sources.remove(&sink_key);
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
                let key = self.pstate.output_key_by_id.get(&o.output_id)?.clone();
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
            if let Err(error) = self
                .scene
                .sync_output_layouts(&self.platform.scene_output_layouts())
            {
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
