use std::{
    collections::{HashMap, HashSet},
    io,
    os::fd::BorrowedFd,
    rc::Rc,
};

use drm::{
    buffer::{DrmFourcc, DrmModifier, Handle as DrmBufferHandle, PlanarBuffer},
    control::{
        AtomicCommitFlags, Device as ControlDevice, FbCmd2Flags, Mode as DrmMode, ModeTypeFlags,
        PlaneType, ResourceHandles, atomic::AtomicModeReq, connector, crtc, encoder, framebuffer,
        plane, property,
    },
};
use yserver_core::backend::ModeSpec;

use crate::drm::Device;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Mode {
    pub name: String,
    pub width: u16,
    pub height: u16,
    pub vrefresh: u32,
    pub preferred: bool,
    /// Real kernel timing (from `drmModeModeInfo`), carried through so
    /// the RANDR `ModeInfo` reply reproduces the exact fractional refresh
    /// Xorg reports (e.g. 59.95, not integer 60). `clock_khz == 0` means
    /// timing is unknown (synthetic/test modes) and the RANDR layer falls
    /// back to synthesising blanking. See `yserver_core::randr::ModeTiming`.
    pub clock_khz: u32,
    pub hsync_start: u16,
    pub hsync_end: u16,
    pub htotal: u16,
    pub vsync_start: u16,
    pub vsync_end: u16,
    pub vtotal: u16,
    /// Vertical scan multiplier from `drmModeModeInfo::vscan`. Values 0 and
    /// 1 both mean a single scan; values above 1 divide the effective refresh.
    pub vscan: u16,
    /// Raw `DRM_MODE_FLAG_*` bits; mapped to RANDR flags at report time.
    pub flags: u32,
}

/// Fields which make one client-visible RANDR mode resource distinct.
///
/// `preferred` is an output association rather than mode identity. A zero
/// clock is projected with synthetic timing, so dormant kernel timing fields
/// are likewise excluded in that case. The mode name and `vscan` are not
/// exposed in RANDR's `ModeInfo` timing either.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ModeIdentity {
    width: u16,
    height: u16,
    vrefresh: u32,
    clock_khz: u32,
    hsync_start: u16,
    hsync_end: u16,
    htotal: u16,
    vsync_start: u16,
    vsync_end: u16,
    vtotal: u16,
    flags: u32,
}

impl From<&Mode> for ModeIdentity {
    fn from(mode: &Mode) -> Self {
        let (hsync_start, hsync_end, htotal, vsync_start, vsync_end, vtotal, flags) =
            if mode.clock_khz == 0 {
                (0, 0, 0, 0, 0, 0, 0)
            } else {
                (
                    mode.hsync_start,
                    mode.hsync_end,
                    mode.htotal,
                    mode.vsync_start,
                    mode.vsync_end,
                    mode.vtotal,
                    mode.flags & 0x3f,
                )
            };
        Self {
            width: mode.width,
            height: mode.height,
            vrefresh: mode.vrefresh,
            clock_khz: mode.clock_khz,
            hsync_start,
            hsync_end,
            htotal,
            vsync_start,
            vsync_end,
            vtotal,
            flags,
        }
    }
}

pub fn pick_mode(modes: &[Mode]) -> Option<&Mode> {
    // Optional override: YSERVER_MODE=WxH (e.g. "1024x768") wins over the
    // kernel-reported PREFERRED mode. Useful when virtio-gpu's EDID hint
    // is ignored and the driver advertises 640x480 as preferred. Refresh
    // is matched best-effort: 60 Hz first, then any rate.
    if let Ok(spec) = std::env::var("YSERVER_MODE")
        && let Some((w, h)) = parse_mode_spec(&spec)
    {
        if let Some(m) = modes
            .iter()
            .find(|m| m.width == w && m.height == h && m.vrefresh == 60)
        {
            return Some(m);
        }
        if let Some(m) = modes.iter().find(|m| m.width == w && m.height == h) {
            return Some(m);
        }
        log::warn!(
            "YSERVER_MODE={spec} not advertised by the connector; falling back to preferred mode"
        );
    }
    if let Some(m) = modes.iter().find(|m| m.preferred) {
        return Some(m);
    }
    if let Some(m) = modes
        .iter()
        .find(|m| m.width == 1024 && m.height == 768 && m.vrefresh == 60)
    {
        return Some(m);
    }
    modes.first()
}

/// Collapse modes that share the selectable `(width, height, vrefresh)`
/// identity, keeping the first occurrence.
///
/// `SetCrtcConfig` still selects a DRM mode by this nominal triple, so exposing
/// two connector-local timings with the same triple would make the second XID
/// unselectable and regress issue #48. Exact timing remains part of global
/// RANDR mode identity across different outputs and monitor replacements.
/// Callers pass a preferred-first list so the preferred instance survives.
fn collapse_duplicate_modes(modes: Vec<Mode>) -> Vec<Mode> {
    let mut seen: HashSet<(u16, u16, u32)> = HashSet::new();
    modes
        .into_iter()
        .filter(|mode| seen.insert((mode.width, mode.height, mode.vrefresh)))
        .collect()
}

fn parse_mode_spec(spec: &str) -> Option<(u16, u16)> {
    let (w, h) = spec.split_once('x')?;
    let w: u16 = w.trim().parse().ok()?;
    let h: u16 = h.trim().parse().ok()?;
    Some((w, h))
}

fn local_mode_from(m: &DrmMode) -> Mode {
    let (w, h) = m.size();
    // Xorg `drmmode_ConvertFromKMode`: copy kernel timing verbatim.
    let (hsync_start, hsync_end, htotal) = m.hsync();
    let (vsync_start, vsync_end, vtotal) = m.vsync();
    Mode {
        name: m.name().to_string_lossy().into_owned(),
        width: w,
        height: h,
        vrefresh: m.vrefresh(),
        preferred: m.mode_type().contains(ModeTypeFlags::PREFERRED),
        clock_khz: m.clock(),
        hsync_start,
        hsync_end,
        htotal,
        vsync_start,
        vsync_end,
        vtotal,
        vscan: m.vscan(),
        flags: m.flags().bits(),
    }
}

#[derive(Debug)]
pub struct Output {
    pub connector: connector::Handle,
    pub connector_name: String,
    pub encoder: encoder::Handle,
    pub crtc: crtc::Handle,
    pub plane: plane::Handle,
    pub mode: DrmMode,
    pub picked: Mode,
    pub plane_fb_id_prop: property::Handle,
    pub plane_crtc_id_prop: property::Handle,
    pub plane_src_x_prop: property::Handle,
    pub plane_src_y_prop: property::Handle,
    pub plane_src_w_prop: property::Handle,
    pub plane_src_h_prop: property::Handle,
    pub plane_crtc_x_prop: property::Handle,
    pub plane_crtc_y_prop: property::Handle,
    pub plane_crtc_w_prop: property::Handle,
    pub plane_crtc_h_prop: property::Handle,
    /// Cached explicit-sync plane property. `None` means the driver
    /// did not expose it during modeset discovery; page-flip submission
    /// falls back to lookup so compatibility stays unchanged.
    pub plane_in_fence_fd_prop: Option<property::Handle>,
    /// Cached explicit-sync CRTC property. See
    /// [`Self::plane_in_fence_fd_prop`].
    pub crtc_out_fence_ptr_prop: Option<property::Handle>,
    /// DRM modifiers accepted by the primary plane for XRGB8888
    /// scanout, parsed from the optional IN_FORMATS property. Empty
    /// means the driver did not expose IN_FORMATS or parsing failed;
    /// callers should fall back to conservative legacy probing.
    pub scanout_modifiers: Vec<u64>,
    /// EDID-derived physical width of the connected display in
    /// millimeters. 0 if the connector did not report a size (e.g.
    /// virtio-gpu, displays without EDID); callers should fall back
    /// to a 96-DPI synthesis from pixel dimensions.
    pub mm_width: u32,
    /// EDID-derived physical height; see [`Self::mm_width`].
    pub mm_height: u32,
    /// Raw EDID blob read from the connector's `EDID` property (128
    /// bytes, or 256 with an extension block). Empty when the connector
    /// exposes no EDID (virtio-gpu, headless). Served to RANDR clients
    /// as the `EDID`/`EDID_DATA` output property so monitor-identity
    /// matching (mate/mutter `monitors.xml`) works.
    pub edid: Vec<u8>,
    /// RANDR `ConnectorType` property value name mapped from the DRM
    /// connector interface (`"DisplayPort"`, `"HDMI"`, `"DVI-D"`,
    /// `"VGA"`, `"Panel"`, …; `"unknown"` when unmappable).
    pub connector_type: String,
    /// The connector's full local mode list, preferred-first, as
    /// reported by the kernel/EDID. `picked` is the boot default and
    /// is always present in this list. Used by RANDR to advertise the
    /// selectable mode set (`GetOutputInfo` / `GetScreenResources`) and
    /// by `apply_crtc_config` to resolve a client-requested mode.
    pub modes: Vec<Mode>,
}

/// Lightweight connector state used by RANDR's forced
/// `GetScreenResources` refresh. Unlike [`Output`], this deliberately does
/// not resolve encoders, CRTCs, planes, properties, or scanout modifiers.
#[derive(Debug)]
pub(crate) struct ConnectorProbe {
    pub(crate) connector_name: String,
    pub(crate) connected: bool,
    pub(crate) modes: Vec<Mode>,
}

/// Connector-owned metadata gathered only at physical topology boundaries.
///
/// Unlike [`ConnectorProbe`], this forces current connector state and may read
/// EDID/property blobs. Keep it out of forced `RRGetScreenResources`: the
/// backend stores these heavier snapshots authoritatively at startup,
/// hotplug, and resume boundaries.
#[derive(Debug)]
pub(crate) struct ConnectorSnapshotProbe {
    pub(crate) connector_name: String,
    pub(crate) modes: Vec<Mode>,
    pub(crate) mm_width: u32,
    pub(crate) mm_height: u32,
    pub(crate) edid: Vec<u8>,
    pub(crate) connector_type: String,
}

fn probe_modes(info: &connector::Info) -> Vec<Mode> {
    let mut modes: Vec<Mode> = info.modes().iter().map(local_mode_from).collect();
    modes.sort_by_key(|mode| !mode.preferred);
    collapse_duplicate_modes(modes)
}

/// Refresh only the connector state RANDR needs for a forced resource query.
///
/// Full [`discover_outputs`] is a boot/hotplug configuration operation: it
/// enumerates every plane and property, computes CRTC/plane assignments, and
/// reads scanout modifiers. Calling it from `RRGetScreenResources` made a
/// read-only desktop query stall the X event loop for more than 100 ms under
/// GPU load. Xorg's forced RANDR probe refreshes connector connection/mode
/// state; it does not rebuild the active scanout pipeline.
pub(crate) fn probe_connectors(device: &Device) -> io::Result<Vec<ConnectorProbe>> {
    let resources = device.resource_handles()?;
    let mut probes = Vec::with_capacity(resources.connectors().len());
    for &handle in resources.connectors() {
        let info = device.get_connector(handle, false)?;
        let connected = info.state() == connector::State::Connected;
        let connector_name = xorg_output_name(info.interface(), info.interface_id());
        let modes = if connected {
            probe_modes(&info)
        } else {
            Vec::new()
        };
        probes.push(ConnectorProbe {
            connector_name,
            connected,
            modes,
        });
    }
    Ok(probes)
}

/// Gather connected-connector metadata for startup/hotplug/resume without
/// resolving encoders, CRTCs, planes, or scanout modifiers.
pub(crate) fn probe_connector_snapshots(
    device: &Device,
) -> io::Result<Vec<ConnectorSnapshotProbe>> {
    let resources = device.resource_handles()?;
    let mut probes = Vec::with_capacity(resources.connectors().len());
    for &handle in resources.connectors() {
        let info = device.get_connector(handle, true)?;
        if info.state() != connector::State::Connected {
            continue;
        }
        let connector_name = xorg_output_name(info.interface(), info.interface_id());
        let modes = probe_modes(&info);
        let (mm_width, mm_height) = info.size().unwrap_or((0, 0));
        probes.push(ConnectorSnapshotProbe {
            connector_type: randr_connector_type_name(&connector_name),
            connector_name,
            modes,
            mm_width,
            mm_height,
            edid: connector_edid_blob(device, handle),
        });
    }
    Ok(probes)
}

/// One connected connector along with its candidate CRTCs and primary planes.
///
/// `candidate_planes` is each plane paired with the set of CRTCs that plane
/// can drive (i.e. the plane's `possible_crtcs` mask, already filtered to
/// `resources.crtcs()`). `assign_outputs` uses this to verify the final
/// (CRTC, plane) pairing for each connector.
pub(crate) struct ConnectorCandidate {
    pub connector: connector::Handle,
    pub connector_name: String,
    pub encoder: encoder::Handle,
    pub candidate_crtcs: Vec<crtc::Handle>,
    pub candidate_planes: Vec<(plane::Handle, HashSet<crtc::Handle>)>,
}

#[derive(Debug)]
pub(crate) struct Assignment {
    pub connector: connector::Handle,
    pub connector_name: String,
    pub encoder: encoder::Handle,
    pub crtc: crtc::Handle,
    pub plane: plane::Handle,
}

fn primary_plane_candidates(
    device: &Device,
    resources: &ResourceHandles,
) -> io::Result<Vec<(plane::Handle, HashSet<crtc::Handle>)>> {
    let mut primary_planes = Vec::new();
    for handle in device.plane_handles()? {
        let info = device.get_plane(handle)?;
        let props = device.get_properties(handle)?;
        let map = props.as_hashmap(device)?;
        let Some(type_info) = map.get("type") else {
            continue;
        };
        let raw = props
            .iter()
            .find(|(h, _)| **h == type_info.handle())
            .map(|(_, v)| *v)
            .unwrap_or(0);
        if raw != PlaneType::Primary as u64 {
            continue;
        }
        let drivable = resources
            .filter_crtcs(info.possible_crtcs())
            .into_iter()
            .collect();
        primary_planes.push((handle, drivable));
    }
    Ok(primary_planes)
}

fn connector_candidate(
    device: &Device,
    resources: &ResourceHandles,
    primary_planes: &[(plane::Handle, HashSet<crtc::Handle>)],
    handle: connector::Handle,
    info: &connector::Info,
) -> io::Result<ConnectorCandidate> {
    let connector_name = xorg_output_name(info.interface(), info.interface_id());
    let encoder_handle = info
        .current_encoder()
        .or_else(|| info.encoders().first().copied())
        .ok_or_else(|| {
            io::Error::other(format!("connector {connector_name} has no usable encoder"))
        })?;
    let encoder_info = device.get_encoder(encoder_handle)?;
    let mut candidate_crtcs = resources.filter_crtcs(encoder_info.possible_crtcs());
    // If the encoder is already bound to a CRTC, prefer it first.
    if let Some(current) = encoder_info.crtc() {
        if let Some(idx) = candidate_crtcs.iter().position(|c| *c == current) {
            candidate_crtcs.swap(0, idx);
        } else {
            candidate_crtcs.insert(0, current);
        }
    }
    if candidate_crtcs.is_empty() {
        return Err(io::Error::other(format!(
            "encoder for connector {connector_name} has no possible CRTC",
        )));
    }
    let candidate_crtc_set: HashSet<crtc::Handle> = candidate_crtcs.iter().copied().collect();
    let candidate_planes = primary_planes
        .iter()
        .filter(|(_, drivable)| drivable.iter().any(|c| candidate_crtc_set.contains(c)))
        .cloned()
        .collect();

    Ok(ConnectorCandidate {
        connector: handle,
        connector_name,
        encoder: encoder_handle,
        candidate_crtcs,
        candidate_planes,
    })
}

fn assign_output_avoiding(
    candidate: &ConnectorCandidate,
    reserved_routes: &[(encoder::Handle, crtc::Handle, plane::Handle)],
) -> Result<Assignment, String> {
    let reserved_encoders: HashSet<encoder::Handle> = reserved_routes
        .iter()
        .map(|(encoder, _, _)| *encoder)
        .collect();
    let reserved_crtcs: HashSet<crtc::Handle> =
        reserved_routes.iter().map(|(_, crtc, _)| *crtc).collect();
    let reserved_planes: HashSet<plane::Handle> =
        reserved_routes.iter().map(|(_, _, plane)| *plane).collect();

    if reserved_encoders.contains(&candidate.encoder) {
        return Err(candidate.connector_name.clone());
    }

    for &crtc in &candidate.candidate_crtcs {
        if reserved_crtcs.contains(&crtc) {
            continue;
        }
        let Some(&(plane, _)) = candidate
            .candidate_planes
            .iter()
            .find(|(plane, drivable)| !reserved_planes.contains(plane) && drivable.contains(&crtc))
        else {
            continue;
        };
        return Ok(Assignment {
            connector: candidate.connector,
            connector_name: candidate.connector_name.clone(),
            encoder: candidate.encoder,
            crtc,
            plane,
        });
    }

    Err(candidate.connector_name.clone())
}

/// Greedy first-fit assignment of (CRTC, primary plane) pairs to connectors.
///
/// Walks `connectors` in input order. For each, picks the first
/// `candidate_crtc` not yet claimed, then the first `candidate_plane` that
/// can drive that CRTC and is not yet claimed. Returns the connector's name
/// as `Err` if no unclaimed (CRTC, plane) pair exists.
///
// TODO(phase-6.10.x): real-hardware shared encoder pools (Intel/AMD) need
// bipartite matching here — current scope is virtio-gpu where assignments
// are always disjoint.
fn assign_outputs(connectors: &[ConnectorCandidate]) -> Result<Vec<Assignment>, String> {
    let mut claimed_crtcs: HashSet<crtc::Handle> = HashSet::new();
    let mut claimed_planes: HashSet<plane::Handle> = HashSet::new();
    let mut out = Vec::with_capacity(connectors.len());

    for cand in connectors {
        let Some(&crtc) = cand
            .candidate_crtcs
            .iter()
            .find(|c| !claimed_crtcs.contains(c))
        else {
            return Err(cand.connector_name.clone());
        };
        let Some(&(plane, _)) = cand
            .candidate_planes
            .iter()
            .find(|(p, drivable)| !claimed_planes.contains(p) && drivable.contains(&crtc))
        else {
            return Err(cand.connector_name.clone());
        };
        claimed_crtcs.insert(crtc);
        claimed_planes.insert(plane);
        out.push(Assignment {
            connector: cand.connector,
            connector_name: cand.connector_name.clone(),
            encoder: cand.encoder,
            crtc,
            plane,
        });
    }

    Ok(out)
}

/// Enumerate every connected connector with usable modes and assign each
/// one a CRTC and primary plane. Greedy first-fit; see `assign_outputs`.
///
/// # Errors
/// - underlying DRM ioctls fail (resource handles, properties, etc.)
/// - a connector has no usable encoder, no candidate CRTC, or no usable
///   modes
/// - greedy assignment cannot place every connector (returns the stranded
///   connector's name in the error message)
///
/// A device with no connected connectors returns an empty output list. This
/// keeps an opened KMS card usable as provider/topology state while startup
/// remains headless until RANDR or hotplug enables an output.
///
/// # Panics
/// Panics only on internal invariant violations: a connector tracked in
/// `connector_infos` must always be present when its assignment is finalized,
/// and the picked mode must always be one of the connector's local modes.
pub fn discover_outputs(device: &Device) -> io::Result<Vec<Output>> {
    let resources = device.resource_handles()?;

    // Pre-collect primary planes with their possible-CRTC sets.
    // TODO(phase-6.10.x): on real hardware (Intel/AMD) primary planes are
    // shared across CRTCs and the greedy first-fit below can strand a
    // connector even though a valid assignment exists. virtio-gpu pairs
    // each plane 1:1 with a CRTC so greedy is correct for current scope.
    let primary_planes = primary_plane_candidates(device, &resources)?;

    // Build candidates for every connected connector with usable modes.
    let mut candidates: Vec<ConnectorCandidate> = Vec::new();
    let mut connector_infos: HashMap<connector::Handle, connector::Info> = HashMap::new();
    for &handle in resources.connectors() {
        let info = device.get_connector(handle, true)?;
        if info.state() != connector::State::Connected || info.modes().is_empty() {
            continue;
        }
        candidates.push(connector_candidate(
            device,
            &resources,
            &primary_planes,
            handle,
            &info,
        )?);
        connector_infos.insert(handle, info);
    }

    if candidates.is_empty() {
        return Ok(Vec::new());
    }

    let assignments = assign_outputs(&candidates).map_err(|name| {
        io::Error::other(format!(
            "connector {name} could not be placed (no unclaimed CRTC/plane)",
        ))
    })?;

    let mut outputs = Vec::with_capacity(assignments.len());
    for asg in assignments {
        let connector_info = connector_infos
            .remove(&asg.connector)
            .expect("connector_info recorded for every candidate");
        outputs.push(finalize_output(device, asg, &connector_info)?);
    }

    Ok(outputs)
}

/// Discover one connected connector while preserving the live routes of all
/// other outputs on this DRM device.
///
/// RANDR can enable an output independently. Running [`discover_outputs`] and
/// then keeping only the requested row is unsafe: its hypothetical whole-card
/// assignment may move a survivor to another CRTC/plane, allowing the target
/// row to steal the survivor's actual live objects. This targeted path seeds
/// the allocator with those live routes and never assigns unrelated Off
/// connectors.
pub fn discover_output_for_connector(
    device: &Device,
    connector_name: &str,
    reserved_routes: &[(encoder::Handle, crtc::Handle, plane::Handle)],
) -> io::Result<Output> {
    let resources = device.resource_handles()?;
    let primary_planes = primary_plane_candidates(device, &resources)?;

    for &handle in resources.connectors() {
        let info = device.get_connector(handle, false)?;
        let name = xorg_output_name(info.interface(), info.interface_id());
        if name != connector_name {
            continue;
        }
        if info.state() != connector::State::Connected || info.modes().is_empty() {
            return Err(io::Error::other(format!(
                "connector {connector_name} is disconnected or has no usable modes"
            )));
        }
        let candidate = connector_candidate(device, &resources, &primary_planes, handle, &info)?;
        let assignment = assign_output_avoiding(&candidate, reserved_routes).map_err(|name| {
            io::Error::other(format!(
                "connector {name} could not be placed without moving a live encoder/CRTC/plane"
            ))
        })?;
        return finalize_output(device, assignment, &info);
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("connector {connector_name} was not found"),
    ))
}

fn exact_assignment_from_candidate(
    candidate: &ConnectorCandidate,
    encoder: encoder::Handle,
    crtc: crtc::Handle,
    plane: plane::Handle,
) -> io::Result<Assignment> {
    if candidate.encoder != encoder {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "connector {} exact encoder {encoder:?} is no longer the selected encoder {:?}",
                candidate.connector_name, candidate.encoder
            ),
        ));
    }
    if !candidate.candidate_crtcs.contains(&crtc) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "connector {} exact CRTC {crtc:?} is not drivable by encoder {encoder:?}",
                candidate.connector_name
            ),
        ));
    }
    let plane_drives_crtc = candidate
        .candidate_planes
        .iter()
        .any(|(candidate_plane, drivable)| *candidate_plane == plane && drivable.contains(&crtc));
    if !plane_drives_crtc {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "connector {} exact primary plane {plane:?} cannot drive CRTC {crtc:?}",
                candidate.connector_name
            ),
        ));
    }
    Ok(Assignment {
        connector: candidate.connector,
        connector_name: candidate.connector_name.clone(),
        encoder,
        crtc,
        plane,
    })
}

fn requested_mode_index(modes: &[Mode], requested: ModeSpec) -> Option<usize> {
    modes.iter().position(|mode| {
        mode.width == requested.width
            && mode.height == requested.height
            && mode.vrefresh == requested.vrefresh
    })
}

/// Reconstruct one exact parent-selected connector/encoder/CRTC/primary-plane
/// assignment on an inherited KMS device.
///
/// Every object is rediscovered and revalidated against the child's current
/// DRM resources. The returned [`Output`] uses the exact requested nominal
/// mode rather than the connector's default/preferred mode. This performs no
/// atomic commit; disposable qualification later issues only `TEST_ONLY`.
pub(crate) fn output_for_exact_probe_assignment(
    device: &Device,
    connector: connector::Handle,
    encoder: encoder::Handle,
    crtc: crtc::Handle,
    plane: plane::Handle,
    requested_mode: ModeSpec,
) -> io::Result<Output> {
    let resources = device.resource_handles()?;
    if !resources.connectors().contains(&connector) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("exact probe connector {connector:?} is not present on the inherited device"),
        ));
    }
    let connector_info = device.get_connector(connector, true)?;
    let connector_name =
        xorg_output_name(connector_info.interface(), connector_info.interface_id());
    if connector_info.state() != connector::State::Connected || connector_info.modes().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            format!(
                "exact probe connector {connector_name}/{connector:?} is disconnected or has no usable modes"
            ),
        ));
    }
    if !resources.encoders().contains(&encoder)
        || (!connector_info.encoders().contains(&encoder)
            && connector_info.current_encoder() != Some(encoder))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "exact probe encoder {encoder:?} is no longer available to connector {connector_name}"
            ),
        ));
    }

    let primary_planes = primary_plane_candidates(device, &resources)?;
    let encoder_info = device.get_encoder(encoder)?;
    let mut candidate_crtcs = resources.filter_crtcs(encoder_info.possible_crtcs());
    if let Some(current) = encoder_info.crtc() {
        if let Some(index) = candidate_crtcs
            .iter()
            .position(|candidate| *candidate == current)
        {
            candidate_crtcs.swap(0, index);
        } else {
            candidate_crtcs.insert(0, current);
        }
    }
    let candidate_crtc_set: HashSet<_> = candidate_crtcs.iter().copied().collect();
    let candidate_planes = primary_planes
        .into_iter()
        .filter(|(_, drivable)| {
            drivable
                .iter()
                .any(|crtc| candidate_crtc_set.contains(crtc))
        })
        .collect();
    let candidate = ConnectorCandidate {
        connector,
        connector_name,
        encoder,
        candidate_crtcs,
        candidate_planes,
    };
    let assignment = exact_assignment_from_candidate(&candidate, encoder, crtc, plane)?;
    finalize_output_with_mode(device, assignment, &connector_info, Some(requested_mode))
}

fn finalize_output(
    device: &Device,
    asg: Assignment,
    connector_info: &connector::Info,
) -> io::Result<Output> {
    finalize_output_with_mode(device, asg, connector_info, None)
}

fn finalize_output_with_mode(
    device: &Device,
    asg: Assignment,
    connector_info: &connector::Info,
    requested_mode: Option<ModeSpec>,
) -> io::Result<Output> {
    let local_modes: Vec<Mode> = connector_info.modes().iter().map(local_mode_from).collect();
    let picked_idx = if let Some(requested) = requested_mode {
        requested_mode_index(&local_modes, requested).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "connector {} does not advertise requested mode {}x{}@{}",
                    asg.connector_name, requested.width, requested.height, requested.vrefresh
                ),
            )
        })?
    } else {
        let picked = pick_mode(&local_modes).ok_or_else(|| {
            io::Error::other(format!(
                "connector {} reports no usable modes",
                asg.connector_name
            ))
        })?;
        local_modes
            .iter()
            .position(|mode| {
                mode.name == picked.name
                    && mode.width == picked.width
                    && mode.height == picked.height
                    && mode.vrefresh == picked.vrefresh
            })
            .expect("picked mode is from local_modes")
    };
    let picked = local_modes[picked_idx].clone();
    let drm_mode = connector_info.modes()[picked_idx];

    let plane_props_map = PropMap::for_object(device, asg.plane)?;
    let plane_fb_id_prop = plane_props_map.id("FB_ID")?;
    let plane_crtc_id_prop = plane_props_map.id("CRTC_ID")?;
    let plane_src_x_prop = plane_props_map.id("SRC_X")?;
    let plane_src_y_prop = plane_props_map.id("SRC_Y")?;
    let plane_src_w_prop = plane_props_map.id("SRC_W")?;
    let plane_src_h_prop = plane_props_map.id("SRC_H")?;
    let plane_crtc_x_prop = plane_props_map.id("CRTC_X")?;
    let plane_crtc_y_prop = plane_props_map.id("CRTC_Y")?;
    let plane_crtc_w_prop = plane_props_map.id("CRTC_W")?;
    let plane_crtc_h_prop = plane_props_map.id("CRTC_H")?;
    let plane_in_fence_fd_prop = plane_props_map.id("IN_FENCE_FD").ok();
    let crtc_out_fence_ptr_prop = PropMap::for_object(device, asg.crtc)
        .and_then(|props| props.id("OUT_FENCE_PTR"))
        .ok();
    let scanout_modifiers = plane_scanout_modifiers(device, asg.plane)?;

    log::info!(
        "yserver: connector={} crtc={:?} plane={:?} mode={} ({}x{}@{}{})",
        asg.connector_name,
        asg.crtc,
        asg.plane,
        picked.name,
        picked.width,
        picked.height,
        picked.vrefresh,
        if picked.preferred { ", preferred" } else { "" }
    );

    let (mm_width, mm_height) = connector_info.size().unwrap_or((0, 0));
    let edid = connector_edid_blob(device, asg.connector);
    let connector_type = randr_connector_type_name(&asg.connector_name);

    // Full advertised mode list, sorted preferred-first (matching Xorg
    // GetOutputInfo's nPreferred prefix). `local_modes` is kept in kernel
    // order above for the `picked_idx` -> DRM-mode mapping; we only
    // reorder this owned copy now that `drm_mode` is resolved.
    let mut modes = local_modes;
    modes.sort_by_key(|m| !m.preferred);
    let modes = collapse_duplicate_modes(modes);

    Ok(Output {
        connector: asg.connector,
        connector_name: asg.connector_name,
        encoder: asg.encoder,
        crtc: asg.crtc,
        plane: asg.plane,
        mode: drm_mode,
        picked,
        plane_fb_id_prop,
        plane_crtc_id_prop,
        plane_src_x_prop,
        plane_src_y_prop,
        plane_src_w_prop,
        plane_src_h_prop,
        plane_crtc_x_prop,
        plane_crtc_y_prop,
        plane_crtc_w_prop,
        plane_crtc_h_prop,
        plane_in_fence_fd_prop,
        crtc_out_fence_ptr_prop,
        scanout_modifiers,
        mm_width,
        mm_height,
        edid,
        connector_type,
        modes,
    })
}

/// Name a connector exactly like Xorg's modesetting driver:
/// `output_names[connector_type]-connector_type_id`
/// (`hw/xfree86/drivers/modesetting/drmmode_display.c`). drm-rs's own
/// `Display`/`as_str` diverges — it renders `HDMIA` as `"HDMI-A"`,
/// giving `"HDMI-A-1"`, whereas Xorg (and therefore every X client and
/// every stored `monitors.xml` identity key) uses `"HDMI-1"`. A stored
/// GNOME/MATE monitor config keyed on `HDMI-1` never matches yserver's
/// `HDMI-A-1`, so the daemon discards the whole config and blanks the
/// desktop. Match Xorg's names verbatim.
fn xorg_output_name(interface: connector::Interface, interface_id: u32) -> String {
    use connector::Interface;
    let base = match interface {
        Interface::VGA => "VGA",
        Interface::DVII => "DVI-I",
        Interface::DVID => "DVI-D",
        Interface::DVIA => "DVI-A",
        Interface::Composite => "Composite",
        Interface::SVideo => "SVIDEO",
        Interface::LVDS => "LVDS",
        Interface::Component => "Component",
        Interface::NinePinDIN => "DIN",
        Interface::DisplayPort => "DP",
        Interface::HDMIA => "HDMI",
        Interface::HDMIB => "HDMI-B",
        Interface::TV => "TV",
        Interface::EmbeddedDisplayPort => "eDP",
        Interface::Virtual => "Virtual",
        Interface::DSI => "DSI",
        Interface::DPI => "DPI",
        // Beyond Xorg's table (newer/non-display connector types) and
        // the `#[non_exhaustive]` catch-all.
        _ => "Unknown",
    };
    format!("{base}-{interface_id}")
}

/// Read the connector's raw `EDID` property blob (empty if absent).
fn connector_edid_blob(device: &Device, connector: connector::Handle) -> Vec<u8> {
    let Ok(props) = device.get_properties(connector) else {
        return Vec::new();
    };
    for (prop_handle, raw_value) in &props {
        let Ok(info) = device.get_property(*prop_handle) else {
            continue;
        };
        if info.name().to_bytes() != b"EDID" {
            continue;
        }
        if *raw_value == 0 {
            return Vec::new();
        }
        return device.get_property_blob(*raw_value).unwrap_or_default();
    }
    Vec::new()
}

/// Map a DRM connector name (e.g. `"HDMI-A-1"`, `"DP-2"`, `"eDP-1"`) to
/// the RANDR `ConnectorType` property value name (randrproto §
/// "ConnectorType"). Best-effort; `"unknown"` when unrecognised.
fn randr_connector_type_name(connector_name: &str) -> String {
    let base = connector_name.trim();
    let ty = if base.starts_with("HDMI") {
        "HDMI"
    } else if base.starts_with("DP") || base.starts_with("DisplayPort") {
        "DisplayPort"
    } else if base.starts_with("eDP") || base.starts_with("LVDS") {
        "Panel"
    } else if base.starts_with("DVI-I") {
        "DVI-I"
    } else if base.starts_with("DVI-D") {
        "DVI-D"
    } else if base.starts_with("DVI-A") {
        "DVI-A"
    } else if base.starts_with("DVI") {
        "DVI"
    } else if base.starts_with("VGA") {
        "VGA"
    } else if base.starts_with("TV") || base.starts_with("Composite") || base.starts_with("SVIDEO")
    {
        "TV"
    } else {
        "unknown"
    };
    ty.to_string()
}

fn plane_scanout_modifiers(device: &Device, plane: plane::Handle) -> io::Result<Vec<u64>> {
    let props = device.get_properties(plane)?;
    for (prop_handle, raw_value) in &props {
        let info = device.get_property(*prop_handle)?;
        if info.name().to_bytes() != b"IN_FORMATS" {
            continue;
        }
        if *raw_value == 0 {
            return Ok(Vec::new());
        }
        let blob = device.get_property_blob(*raw_value)?;
        return Ok(parse_in_formats_modifiers(
            &blob,
            DrmFourcc::Xrgb8888 as u32,
        ));
    }
    Ok(Vec::new())
}

fn parse_in_formats_modifiers(blob: &[u8], wanted_format: u32) -> Vec<u64> {
    const HEADER_LEN: usize = 24;
    const MODIFIER_LEN: usize = 24;

    if blob.len() < HEADER_LEN {
        return Vec::new();
    }

    let read_u32 = |offset: usize| -> Option<u32> {
        let bytes: [u8; 4] = blob.get(offset..offset + 4)?.try_into().ok()?;
        Some(u32::from_ne_bytes(bytes))
    };
    let read_u64 = |offset: usize| -> Option<u64> {
        let bytes: [u8; 8] = blob.get(offset..offset + 8)?.try_into().ok()?;
        Some(u64::from_ne_bytes(bytes))
    };

    let Some(count_formats) = read_u32(8).map(|n| n as usize) else {
        return Vec::new();
    };
    let Some(formats_offset) = read_u32(12).map(|n| n as usize) else {
        return Vec::new();
    };
    let Some(count_modifiers) = read_u32(16).map(|n| n as usize) else {
        return Vec::new();
    };
    let Some(modifiers_offset) = read_u32(20).map(|n| n as usize) else {
        return Vec::new();
    };

    let Some(formats_end) = formats_offset.checked_add(count_formats.saturating_mul(4)) else {
        return Vec::new();
    };
    let Some(modifiers_end) =
        modifiers_offset.checked_add(count_modifiers.saturating_mul(MODIFIER_LEN))
    else {
        return Vec::new();
    };
    if formats_end > blob.len() || modifiers_end > blob.len() {
        return Vec::new();
    }

    let mut formats = Vec::with_capacity(count_formats);
    for i in 0..count_formats {
        let Some(format) = read_u32(formats_offset + i * 4) else {
            return Vec::new();
        };
        formats.push(format);
    }

    let mut modifiers = Vec::new();
    for i in 0..count_modifiers {
        let base = modifiers_offset + i * MODIFIER_LEN;
        let Some(format_bits) = read_u64(base) else {
            return Vec::new();
        };
        let Some(offset) = read_u32(base + 8) else {
            return Vec::new();
        };
        let Some(modifier) = read_u64(base + 16) else {
            return Vec::new();
        };
        let offset = offset as usize;
        for bit in 0..64 {
            if (format_bits & (1_u64 << bit)) == 0 {
                continue;
            }
            let idx = offset + bit;
            if formats.get(idx).copied() == Some(wanted_format) && !modifiers.contains(&modifier) {
                modifiers.push(modifier);
            }
        }
    }
    modifiers
}

pub fn discover_output(device: &Device) -> io::Result<Output> {
    let outs = discover_outputs(device)?;
    outs.into_iter().next().ok_or_else(|| {
        io::Error::other(
            "no connected output — vng with --graphics required for modeset path; \
             headless mode does not exercise this",
        )
    })
}

pub fn dump_properties(device: &Device, output: &Output) -> io::Result<()> {
    log::debug!("=== connector {} properties ===", output.connector_name);
    dump_object_properties(device, output.connector)?;
    log::debug!("=== crtc {:?} properties ===", output.crtc);
    dump_object_properties(device, output.crtc)?;
    log::debug!("=== plane {:?} properties ===", output.plane);
    dump_object_properties(device, output.plane)?;
    Ok(())
}

fn dump_object_properties<H>(device: &Device, handle: H) -> io::Result<()>
where
    H: drm::control::ResourceHandle,
{
    let props = device.get_properties(handle)?;
    for (prop_handle, raw_value) in &props {
        let info = device.get_property(*prop_handle)?;
        log::debug!(
            "  {} = 0x{:x} ({:?})",
            info.name().to_string_lossy(),
            raw_value,
            info.value_type()
        );
    }
    Ok(())
}

pub(crate) struct PropMap {
    handles: HashMap<String, property::Info>,
}

impl PropMap {
    pub(crate) fn for_object<H>(device: &Device, handle: H) -> io::Result<Self>
    where
        H: drm::control::ResourceHandle,
    {
        let props = device.get_properties(handle)?;
        Ok(Self {
            handles: props.as_hashmap(device)?,
        })
    }

    pub(crate) fn id(&self, name: &str) -> io::Result<property::Handle> {
        self.handles
            .get(name)
            .map(|info| info.handle())
            .ok_or_else(|| io::Error::other(format!("property {name:?} not exposed")))
    }
}

pub fn disable_output(device: &Device, output: &Output) -> io::Result<()> {
    let connector_props = PropMap::for_object(device, output.connector)?;
    let crtc_props = PropMap::for_object(device, output.crtc)?;

    let mut req = AtomicModeReq::new();
    req.add_raw_property(output.plane.into(), output.plane_fb_id_prop, 0);
    req.add_raw_property(output.plane.into(), output.plane_crtc_id_prop, 0);
    req.add_raw_property(output.crtc.into(), crtc_props.id("ACTIVE")?, 0);
    req.add_raw_property(output.crtc.into(), crtc_props.id("MODE_ID")?, 0);
    req.add_raw_property(output.connector.into(), connector_props.id("CRTC_ID")?, 0);

    device
        .atomic_commit(AtomicCommitFlags::ALLOW_MODESET, req)
        .map_err(|err| {
            io::Error::new(
                err.kind(),
                format!("disable_output atomic commit rejected: {err}"),
            )
        })
}

pub fn commit_modeset(
    device: &Device,
    output: &Output,
    fb_id: framebuffer::Handle,
) -> io::Result<()> {
    finish_best_effort_modeset(modeset_with_flags(
        device,
        output,
        fb_id,
        AtomicCommitFlags::ALLOW_MODESET,
        "atomic modeset commit",
    )?)
}

/// Validate a complete connector/CRTC/primary-plane modeset without changing
/// the hardware state.
pub(crate) fn test_modeset(
    device: &Device,
    output: &Output,
    fb_id: framebuffer::Handle,
) -> io::Result<()> {
    finish_best_effort_modeset(modeset_with_flags(
        device,
        output,
        fb_id,
        AtomicCommitFlags::ALLOW_MODESET | AtomicCommitFlags::TEST_ONLY,
        "atomic modeset TEST_ONLY",
    )?)
}

/// Failure from a helper-only TEST_ONLY transaction. An ordinary rejection is
/// safe once its mode blob was removed. A blob cleanup failure is terminal:
/// the helper shares the parent's DRM file description, so process exit cannot
/// reclaim the leaked blob while the server keeps that fd open.
#[derive(Debug)]
pub(crate) struct StrictTestModesetError {
    source: io::Error,
    blob_cleanup_failed: bool,
}

impl StrictTestModesetError {
    #[must_use]
    pub(crate) fn blob_cleanup_failed(&self) -> bool {
        self.blob_cleanup_failed
    }

    pub(crate) fn into_io_error(self) -> io::Error {
        self.source
    }
}

impl std::fmt::Display for StrictTestModesetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.source.fmt(f)
    }
}

impl std::error::Error for StrictTestModesetError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// TEST_ONLY variant for disposable helpers. Unlike the live path, mode-blob
/// cleanup is part of the authoritative result and overrides atomic success or
/// rejection when it fails.
pub(crate) fn test_modeset_strict(
    device: &Device,
    output: &Output,
    fb_id: framebuffer::Handle,
) -> Result<(), StrictTestModesetError> {
    let attempt = modeset_with_flags(
        device,
        output,
        fb_id,
        AtomicCommitFlags::ALLOW_MODESET | AtomicCommitFlags::TEST_ONLY,
        "atomic modeset TEST_ONLY",
    )
    .map_err(|source| StrictTestModesetError {
        source,
        blob_cleanup_failed: false,
    })?;
    finish_strict_test_modeset(attempt)
}

struct ModesetWithBlobResult {
    atomic: io::Result<()>,
    blob_cleanup: io::Result<()>,
}

fn modeset_with_flags(
    device: &Device,
    output: &Output,
    fb_id: framebuffer::Handle,
    flags: AtomicCommitFlags,
    operation: &str,
) -> io::Result<ModesetWithBlobResult> {
    let connector_props = PropMap::for_object(device, output.connector)?;
    let crtc_props = PropMap::for_object(device, output.crtc)?;
    let plane_props = PropMap::for_object(device, output.plane)?;

    // Resolve every fallible property lookup before creating the mode blob.
    // Probe helpers share the parent's DRM file description, so an early `?`
    // after blob creation would otherwise leave that object attached to the
    // still-live parent fd even after the child exits.
    let connector_crtc_id_prop = connector_props.id("CRTC_ID")?;
    let crtc_mode_id_prop = crtc_props.id("MODE_ID")?;
    let crtc_active_prop = crtc_props.id("ACTIVE")?;
    let plane_fb_id_prop = plane_props.id("FB_ID")?;
    let plane_crtc_id_prop = plane_props.id("CRTC_ID")?;
    let plane_src_x_prop = plane_props.id("SRC_X")?;
    let plane_src_y_prop = plane_props.id("SRC_Y")?;
    let plane_src_w_prop = plane_props.id("SRC_W")?;
    let plane_src_h_prop = plane_props.id("SRC_H")?;
    let plane_crtc_x_prop = plane_props.id("CRTC_X")?;
    let plane_crtc_y_prop = plane_props.id("CRTC_Y")?;
    let plane_crtc_w_prop = plane_props.id("CRTC_W")?;
    let plane_crtc_h_prop = plane_props.id("CRTC_H")?;

    let mode_blob = device.create_property_blob(&output.mode)?;
    let mode_blob_raw: u64 = mode_blob.into();

    let crtc_id_raw: u32 = output.crtc.into();
    let plane_crtc_raw: u32 = output.crtc.into();
    let fb_id_raw: u32 = fb_id.into();
    let (mode_w, mode_h) = output.mode.size();
    let src_w = u64::from(mode_w) << 16;
    let src_h = u64::from(mode_h) << 16;

    let mut req = AtomicModeReq::new();
    req.add_raw_property(
        output.connector.into(),
        connector_crtc_id_prop,
        u64::from(crtc_id_raw),
    );
    req.add_raw_property(output.crtc.into(), crtc_mode_id_prop, mode_blob_raw);
    req.add_raw_property(output.crtc.into(), crtc_active_prop, 1);
    req.add_raw_property(output.plane.into(), plane_fb_id_prop, u64::from(fb_id_raw));
    req.add_raw_property(
        output.plane.into(),
        plane_crtc_id_prop,
        u64::from(plane_crtc_raw),
    );
    req.add_raw_property(output.plane.into(), plane_src_x_prop, 0);
    req.add_raw_property(output.plane.into(), plane_src_y_prop, 0);
    req.add_raw_property(output.plane.into(), plane_src_w_prop, src_w);
    req.add_raw_property(output.plane.into(), plane_src_h_prop, src_h);
    req.add_raw_property(output.plane.into(), plane_crtc_x_prop, 0);
    req.add_raw_property(output.plane.into(), plane_crtc_y_prop, 0);
    req.add_raw_property(output.plane.into(), plane_crtc_w_prop, u64::from(mode_w));
    req.add_raw_property(output.plane.into(), plane_crtc_h_prop, u64::from(mode_h));

    let atomic = device.atomic_commit(flags, req).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!(
                "{operation} rejected (mode {}, {}x{}): {err}",
                output.picked.name, output.picked.width, output.picked.height
            ),
        )
    });
    let blob_cleanup = device.destroy_property_blob(mode_blob_raw).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("{operation} could not destroy mode blob {mode_blob_raw}: {err}"),
        )
    });
    Ok(ModesetWithBlobResult {
        atomic,
        blob_cleanup,
    })
}

fn finish_best_effort_modeset(result: ModesetWithBlobResult) -> io::Result<()> {
    if let Err(error) = result.blob_cleanup {
        // A committed live modeset cannot be reported as a pre-commit failure:
        // callers have already crossed the KMS ownership boundary. Preserve
        // the established result semantics and keep cleanup best-effort.
        log::warn!("{error}");
    }
    result.atomic
}

fn finish_strict_test_modeset(result: ModesetWithBlobResult) -> Result<(), StrictTestModesetError> {
    match (result.atomic, result.blob_cleanup) {
        (atomic, Err(cleanup)) => {
            let source = match atomic {
                Ok(()) => cleanup,
                Err(atomic) => io::Error::new(
                    cleanup.kind(),
                    format!("{cleanup}; atomic TEST_ONLY also failed: {atomic}"),
                ),
            };
            Err(StrictTestModesetError {
                source,
                blob_cleanup_failed: true,
            })
        }
        (Err(source), Ok(())) => Err(StrictTestModesetError {
            source,
            blob_cleanup_failed: false,
        }),
        (Ok(()), Ok(())) => Ok(()),
    }
}

/// One primary-plane assignment in an M1 direct-scanout dry run. Source
/// coordinates are integer pixels in the imported root-sized framebuffer;
/// the ioctl encoder converts them to DRM's unsigned 16.16 representation.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DirectScanoutPlaneState<'a> {
    pub(crate) output: &'a Output,
    pub(crate) src_x: u32,
    pub(crate) src_y: u32,
    pub(crate) src_w: u32,
    pub(crate) src_h: u32,
}

/// One primary-plane assignment used to leave a shared direct-scanout
/// framebuffer without disabling either CRTC. Each output gets its retained
/// compositor framebuffer in the same atomic transaction.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ComposedScanoutPlaneState<'a> {
    pub(crate) output: &'a Output,
    pub(crate) fb: framebuffer::Handle,
}

/// Successfully imported framebuffer retained after an accepted M1 probe.
/// It is never installed on hardware. Owning the DRM device makes teardown
/// reliable during backend shutdown regardless of struct-field drop order.
pub(crate) struct DirectScanoutProbeFramebuffer {
    device: Rc<Device>,
    fb: framebuffer::Handle,
    gem: DrmBufferHandle,
}

impl DirectScanoutProbeFramebuffer {
    pub(crate) fn handle(&self) -> framebuffer::Handle {
        self.fb
    }
}

impl Drop for DirectScanoutProbeFramebuffer {
    fn drop(&mut self) {
        if let Err(error) = self.device.destroy_framebuffer(self.fb) {
            log::warn!("scanout_m1: rm_fb during probe-cache teardown failed: {error}");
        }
        if let Err(error) = self.device.close_buffer(self.gem) {
            log::warn!("scanout_m1: GEM close during probe-cache teardown failed: {error}");
        }
    }
}

pub(crate) enum DirectScanoutTestResult {
    Accepted(DirectScanoutProbeFramebuffer),
    Rejected(io::Error),
}

struct DirectScanoutProbeBuffer {
    gem: DrmBufferHandle,
    width: u32,
    height: u32,
    fourcc: DrmFourcc,
    modifier: Option<u64>,
    pitch: u32,
    offset: u32,
}

impl PlanarBuffer for DirectScanoutProbeBuffer {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn format(&self) -> DrmFourcc {
        self.fourcc
    }

    fn modifier(&self) -> Option<DrmModifier> {
        self.modifier.map(DrmModifier::from)
    }

    fn pitches(&self) -> [u32; 4] {
        [self.pitch, 0, 0, 0]
    }

    fn handles(&self) -> [Option<DrmBufferHandle>; 4] {
        [Some(self.gem), None, None, None]
    }

    fn offsets(&self) -> [u32; 4] {
        [self.offset, 0, 0, 0]
    }
}

fn should_retry_direct_scanout_addfb_legacy(modifier: u64, error: &io::Error) -> bool {
    modifier == u64::from(DrmModifier::Linear) && error.kind() == io::ErrorKind::InvalidInput
}

/// Import one client dma-buf and test the exact all-output primary-plane
/// transaction. `TEST_ONLY` is the sole commit flag, so this cannot change
/// live scanout or generate page-flip events.
#[allow(clippy::too_many_arguments)]
pub(crate) fn probe_direct_scanout_test_only(
    device: Rc<Device>,
    dma_buf: BorrowedFd<'_>,
    width: u32,
    height: u32,
    fourcc_code: u32,
    modifier: u64,
    offset: u64,
    pitch: u32,
    planes: &[DirectScanoutPlaneState<'_>],
) -> io::Result<DirectScanoutTestResult> {
    if planes.is_empty() {
        return Err(io::Error::other("scanout M1: empty plane transaction"));
    }
    let fourcc = DrmFourcc::try_from(fourcc_code).map_err(|_| {
        io::Error::other(format!(
            "scanout M1: unknown DRM fourcc 0x{fourcc_code:08x}"
        ))
    })?;
    let offset = u32::try_from(offset)
        .map_err(|_| io::Error::other("scanout M1: plane offset exceeds u32"))?;
    let gem = device
        .prime_fd_to_buffer(dma_buf)
        .map_err(|error| io::Error::other(format!("scanout M1 PRIME import: {error}")))?;
    let buffer = DirectScanoutProbeBuffer {
        gem,
        width,
        height,
        fourcc,
        modifier: Some(modifier),
        pitch,
        offset,
    };
    let fb = match device.add_planar_framebuffer(&buffer, FbCmd2Flags::MODIFIERS) {
        Ok(fb) => fb,
        Err(explicit_error)
            if should_retry_direct_scanout_addfb_legacy(modifier, &explicit_error) =>
        {
            // AMDGPU can reject an explicitly tagged LINEAR PRIME import while
            // accepting the same BO through legacy ADDFB2, where the driver
            // obtains its layout from the imported BO metadata. Restrict this
            // compatibility probe to LINEAR + EINVAL: silently dropping a
            // non-linear modifier would make the M1 result ambiguous.
            let legacy_buffer = DirectScanoutProbeBuffer {
                modifier: None,
                ..buffer
            };
            match device.add_planar_framebuffer(&legacy_buffer, FbCmd2Flags::empty()) {
                Ok(fb) => {
                    log::info!(
                        "scanout_m1: legacy add_fb2 accepted linear buffer after explicit-modifier \
                         EINVAL width={width} height={height} pitch={pitch} offset={offset}"
                    );
                    fb
                }
                Err(legacy_error) => {
                    let _ = device.close_buffer(gem);
                    return Err(io::Error::other(format!(
                        "scanout M1 add_fb2 rejected linear buffer with explicit modifier \
                         ({explicit_error}) and legacy metadata ({legacy_error})"
                    )));
                }
            }
        }
        Err(error) => {
            let _ = device.close_buffer(gem);
            return Err(io::Error::other(format!(
                "scanout M1 add_fb2 with modifier 0x{modifier:x}: {error}"
            )));
        }
    };

    let mut request = AtomicModeReq::new();
    for state in planes {
        let output = state.output;
        let src_x = u64::from(state.src_x) << 16;
        let src_y = u64::from(state.src_y) << 16;
        let src_w = u64::from(state.src_w) << 16;
        let src_h = u64::from(state.src_h) << 16;
        request.add_raw_property(
            output.plane.into(),
            output.plane_fb_id_prop,
            u64::from(u32::from(fb)),
        );
        request.add_raw_property(
            output.plane.into(),
            output.plane_crtc_id_prop,
            u64::from(u32::from(output.crtc)),
        );
        request.add_raw_property(output.plane.into(), output.plane_src_x_prop, src_x);
        request.add_raw_property(output.plane.into(), output.plane_src_y_prop, src_y);
        request.add_raw_property(output.plane.into(), output.plane_src_w_prop, src_w);
        request.add_raw_property(output.plane.into(), output.plane_src_h_prop, src_h);
        request.add_raw_property(output.plane.into(), output.plane_crtc_x_prop, 0);
        request.add_raw_property(output.plane.into(), output.plane_crtc_y_prop, 0);
        request.add_raw_property(
            output.plane.into(),
            output.plane_crtc_w_prop,
            u64::from(state.src_w),
        );
        request.add_raw_property(
            output.plane.into(),
            output.plane_crtc_h_prop,
            u64::from(state.src_h),
        );
    }

    match device.atomic_commit(AtomicCommitFlags::TEST_ONLY, request) {
        Ok(()) => Ok(DirectScanoutTestResult::Accepted(
            DirectScanoutProbeFramebuffer { device, fb, gem },
        )),
        Err(error) => {
            let _ = device.destroy_framebuffer(fb);
            let _ = device.close_buffer(gem);
            Ok(DirectScanoutTestResult::Rejected(io::Error::new(
                error.kind(),
                format!("scanout M1 atomic TEST_ONLY rejected: {error}"),
            )))
        }
    }
}

/// Install an M1-proven client framebuffer on every affected primary plane.
/// The single atomic request is the ownership boundary for M2: before success
/// the caller retains its Copy fallback; after success it must retain the
/// client source until every emitted CRTC page-flip event has retired.
pub(crate) fn submit_direct_scanout(
    device: &Device,
    fb: framebuffer::Handle,
    planes: &[DirectScanoutPlaneState<'_>],
) -> io::Result<()> {
    if planes.is_empty() {
        return Err(io::Error::other("scanout M2: empty plane transaction"));
    }
    let mut request = AtomicModeReq::new();
    for state in planes {
        let output = state.output;
        request.add_raw_property(
            output.plane.into(),
            output.plane_fb_id_prop,
            u64::from(u32::from(fb)),
        );
        request.add_raw_property(
            output.plane.into(),
            output.plane_crtc_id_prop,
            u64::from(u32::from(output.crtc)),
        );
        request.add_raw_property(
            output.plane.into(),
            output.plane_src_x_prop,
            u64::from(state.src_x) << 16,
        );
        request.add_raw_property(
            output.plane.into(),
            output.plane_src_y_prop,
            u64::from(state.src_y) << 16,
        );
        request.add_raw_property(
            output.plane.into(),
            output.plane_src_w_prop,
            u64::from(state.src_w) << 16,
        );
        request.add_raw_property(
            output.plane.into(),
            output.plane_src_h_prop,
            u64::from(state.src_h) << 16,
        );
        request.add_raw_property(output.plane.into(), output.plane_crtc_x_prop, 0);
        request.add_raw_property(output.plane.into(), output.plane_crtc_y_prop, 0);
        request.add_raw_property(
            output.plane.into(),
            output.plane_crtc_w_prop,
            u64::from(state.src_w),
        );
        request.add_raw_property(
            output.plane.into(),
            output.plane_crtc_h_prop,
            u64::from(state.src_h),
        );
    }
    device.atomic_commit(
        AtomicCommitFlags::PAGE_FLIP_EVENT | AtomicCommitFlags::NONBLOCK,
        request,
    )
}

/// Replace every primary plane in a direct-scanout output set atomically.
/// Keeping the CRTCs active avoids the visible blackout and cursor-plane
/// teardown caused by a disable/modeset cycle. The caller retains the direct
/// source until the page-flip event from every CRTC has arrived.
pub(crate) fn submit_composed_scanout(
    device: &Device,
    planes: &[ComposedScanoutPlaneState<'_>],
) -> io::Result<()> {
    if planes.is_empty() {
        return Err(io::Error::other("scanout M2: empty composed transaction"));
    }
    let mut request = AtomicModeReq::new();
    for state in planes {
        let output = state.output;
        request.add_raw_property(
            output.plane.into(),
            output.plane_fb_id_prop,
            u64::from(u32::from(state.fb)),
        );
        request.add_raw_property(
            output.plane.into(),
            output.plane_crtc_id_prop,
            u64::from(u32::from(output.crtc)),
        );
        request.add_raw_property(output.plane.into(), output.plane_src_x_prop, 0);
        request.add_raw_property(output.plane.into(), output.plane_src_y_prop, 0);
        request.add_raw_property(
            output.plane.into(),
            output.plane_src_w_prop,
            u64::from(output.mode.size().0) << 16,
        );
        request.add_raw_property(
            output.plane.into(),
            output.plane_src_h_prop,
            u64::from(output.mode.size().1) << 16,
        );
        request.add_raw_property(output.plane.into(), output.plane_crtc_x_prop, 0);
        request.add_raw_property(output.plane.into(), output.plane_crtc_y_prop, 0);
        request.add_raw_property(
            output.plane.into(),
            output.plane_crtc_w_prop,
            u64::from(output.mode.size().0),
        );
        request.add_raw_property(
            output.plane.into(),
            output.plane_crtc_h_prop,
            u64::from(output.mode.size().1),
        );
    }
    device.atomic_commit(
        AtomicCommitFlags::PAGE_FLIP_EVENT | AtomicCommitFlags::NONBLOCK,
        request,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_scanout_addfb_legacy_retry_is_linear_einval_only() {
        let invalid = io::Error::from(io::ErrorKind::InvalidInput);
        let unsupported = io::Error::from(io::ErrorKind::Unsupported);

        assert!(should_retry_direct_scanout_addfb_legacy(
            u64::from(DrmModifier::Linear),
            &invalid
        ));
        assert!(!should_retry_direct_scanout_addfb_legacy(
            u64::from(DrmModifier::Linear),
            &unsupported
        ));
        assert!(!should_retry_direct_scanout_addfb_legacy(
            u64::from(DrmModifier::I915_x_tiled),
            &invalid
        ));
    }

    #[test]
    fn xorg_output_name_matches_modesetting_driver() {
        use ::drm::control::connector::Interface;
        // The bug: HDMI-A must render as "HDMI-1" (Xorg), not "HDMI-A-1"
        // (drm-rs). A stored monitors.xml keyed on HDMI-1 depends on this.
        assert_eq!(xorg_output_name(Interface::HDMIA, 1), "HDMI-1");
        assert_eq!(xorg_output_name(Interface::DisplayPort, 1), "DP-1");
        assert_eq!(xorg_output_name(Interface::DVII, 2), "DVI-I-2");
        assert_eq!(xorg_output_name(Interface::EmbeddedDisplayPort, 1), "eDP-1");
        assert_eq!(xorg_output_name(Interface::VGA, 1), "VGA-1");
    }

    #[test]
    fn randr_connector_type_name_maps_drm_connector_names() {
        assert_eq!(randr_connector_type_name("HDMI-A-1"), "HDMI");
        assert_eq!(randr_connector_type_name("HDMI-B-2"), "HDMI");
        assert_eq!(randr_connector_type_name("DP-1"), "DisplayPort");
        assert_eq!(randr_connector_type_name("DisplayPort-0"), "DisplayPort");
        assert_eq!(randr_connector_type_name("eDP-1"), "Panel");
        assert_eq!(randr_connector_type_name("LVDS-1"), "Panel");
        assert_eq!(randr_connector_type_name("DVI-I-1"), "DVI-I");
        assert_eq!(randr_connector_type_name("DVI-D-1"), "DVI-D");
        assert_eq!(randr_connector_type_name("VGA-1"), "VGA");
        assert_eq!(randr_connector_type_name("Virtual-1"), "unknown");
    }

    fn mode(name: &str, w: u16, h: u16, refresh: u32, preferred: bool) -> Mode {
        Mode {
            name: name.into(),
            width: w,
            height: h,
            vrefresh: refresh,
            preferred,
            ..Default::default()
        }
    }

    #[test]
    fn picks_preferred_when_present() {
        let modes = vec![
            mode("800x600", 800, 600, 60, false),
            mode("1024x768", 1024, 768, 60, true),
            mode("1920x1080", 1920, 1080, 60, false),
        ];
        let picked = pick_mode(&modes).unwrap();
        assert_eq!(picked.name, "1024x768");
    }

    #[test]
    fn falls_back_to_1024x768_60_when_no_preferred() {
        let modes = vec![
            mode("800x600", 800, 600, 60, false),
            mode("1024x768", 1024, 768, 60, false),
            mode("1920x1080", 1920, 1080, 60, false),
        ];
        let picked = pick_mode(&modes).unwrap();
        assert_eq!(picked.name, "1024x768");
    }

    #[test]
    fn falls_back_to_first_when_no_preferred_and_no_1024x768() {
        let modes = vec![
            mode("800x600", 800, 600, 60, false),
            mode("1920x1080", 1920, 1080, 60, false),
        ];
        let picked = pick_mode(&modes).unwrap();
        assert_eq!(picked.name, "800x600");
    }

    #[test]
    fn empty_list_returns_none() {
        assert!(pick_mode(&[]).is_none());
    }

    #[test]
    fn collapse_duplicate_modes_dedups_nominal_modes_keeping_first() {
        // The apply path can select only w×h@refresh today, so even timings
        // with distinct blanking must collapse within one connector. Input is
        // preferred-first and first-occurrence-wins retains that association.
        let modes = vec![
            mode("3440x1440", 3440, 1440, 165, true),
            mode("1920x1080-edid", 1920, 1080, 60, true),
            mode("1920x1080-cea", 1920, 1080, 60, false),
            Mode {
                name: "1920x1080-alt".into(),
                clock_khz: 148_500,
                hsync_start: 2008,
                hsync_end: 2052,
                htotal: 2200,
                vsync_start: 1084,
                vsync_end: 1089,
                vtotal: 1125,
                flags: 0x5,
                ..mode("ignored", 1920, 1080, 60, false)
            },
        ];
        let deduped = collapse_duplicate_modes(modes);

        assert_eq!(deduped.len(), 2, "two nominal modes remain");
        assert_eq!(deduped[0].name, "3440x1440", "order preserved");
        assert_eq!(deduped[1].name, "1920x1080-edid", "first occurrence wins");
        assert!(deduped[1].preferred, "the preferred instance survives");
    }

    use drm::control::from_u32;

    fn ch(n: u32) -> connector::Handle {
        from_u32(n).expect("non-zero raw handle")
    }
    fn eh(n: u32) -> encoder::Handle {
        from_u32(n).expect("non-zero raw handle")
    }
    fn rh(n: u32) -> crtc::Handle {
        from_u32(n).expect("non-zero raw handle")
    }
    fn ph(n: u32) -> plane::Handle {
        from_u32(n).expect("non-zero raw handle")
    }

    fn cand(
        idx: u32,
        name: &str,
        crtcs: Vec<crtc::Handle>,
        planes: Vec<(plane::Handle, &[crtc::Handle])>,
    ) -> ConnectorCandidate {
        ConnectorCandidate {
            connector: ch(idx),
            connector_name: name.into(),
            encoder: eh(idx),
            candidate_crtcs: crtcs,
            candidate_planes: planes
                .into_iter()
                .map(|(p, cs)| (p, cs.iter().copied().collect()))
                .collect(),
        }
    }

    #[test]
    fn exact_probe_assignment_accepts_only_the_requested_drivable_tuple() {
        let crtc = rh(10);
        let other_crtc = rh(11);
        let plane = ph(20);
        let candidate = cand(1, "HDMI-1", vec![crtc, other_crtc], vec![(plane, &[crtc])]);

        let exact = exact_assignment_from_candidate(&candidate, candidate.encoder, crtc, plane)
            .expect("the exact tuple is valid");
        assert_eq!(exact.connector, candidate.connector);
        assert_eq!(exact.encoder, candidate.encoder);
        assert_eq!(exact.crtc, crtc);
        assert_eq!(exact.plane, plane);

        assert!(
            exact_assignment_from_candidate(&candidate, eh(99), crtc, plane).is_err(),
            "a different encoder must not be substituted"
        );
        assert!(
            exact_assignment_from_candidate(&candidate, candidate.encoder, rh(99), plane).is_err(),
            "an encoder-incompatible CRTC must be rejected"
        );
        assert!(
            exact_assignment_from_candidate(&candidate, candidate.encoder, other_crtc, plane)
                .is_err(),
            "the exact plane must advertise the exact CRTC"
        );
        assert!(
            exact_assignment_from_candidate(&candidate, candidate.encoder, crtc, ph(99)).is_err(),
            "a different primary plane must not be substituted"
        );
    }

    #[test]
    fn exact_probe_mode_selection_includes_refresh_rate() {
        let modes = vec![
            mode("2560x1440-60", 2560, 1440, 60, false),
            mode("2560x1440-165", 2560, 1440, 165, true),
        ];
        assert_eq!(
            requested_mode_index(
                &modes,
                ModeSpec {
                    width: 2560,
                    height: 1440,
                    vrefresh: 165,
                }
            ),
            Some(1)
        );
        assert_eq!(
            requested_mode_index(
                &modes,
                ModeSpec {
                    width: 2560,
                    height: 1440,
                    vrefresh: 144,
                }
            ),
            None
        );
    }

    #[test]
    fn strict_test_only_blob_cleanup_wins_the_atomic_result_matrix() {
        let accepted_and_clean = finish_strict_test_modeset(ModesetWithBlobResult {
            atomic: Ok(()),
            blob_cleanup: Ok(()),
        });
        assert!(accepted_and_clean.is_ok());

        let rejected_and_clean = finish_strict_test_modeset(ModesetWithBlobResult {
            atomic: Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "atomic rejection",
            )),
            blob_cleanup: Ok(()),
        })
        .expect_err("atomic rejection remains ordinary after clean blob teardown");
        assert!(!rejected_and_clean.blob_cleanup_failed());
        assert_eq!(
            rejected_and_clean.into_io_error().kind(),
            io::ErrorKind::InvalidData
        );

        for atomic in [
            Ok(()),
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "atomic rejection",
            )),
        ] {
            let cleanup_failed = finish_strict_test_modeset(ModesetWithBlobResult {
                atomic,
                blob_cleanup: Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "blob cleanup failure",
                )),
            })
            .expect_err("blob cleanup failure must override either atomic result");
            assert!(cleanup_failed.blob_cleanup_failed());
            assert_eq!(
                cleanup_failed.into_io_error().kind(),
                io::ErrorKind::PermissionDenied
            );
        }
    }

    #[test]
    fn assigns_two_connectors_with_disjoint_crtcs_in_input_order() {
        let c0 = rh(10);
        let c1 = rh(11);
        let p0 = ph(20);
        let p1 = ph(21);
        let cands = vec![
            cand(1, "HDMI-1", vec![c0], vec![(p0, &[c0])]),
            cand(2, "HDMI-2", vec![c1], vec![(p1, &[c1])]),
        ];
        let asg = assign_outputs(&cands).expect("assignment succeeds");
        assert_eq!(asg.len(), 2);
        assert_eq!(asg[0].connector_name, "HDMI-1");
        assert_eq!(asg[0].crtc, c0);
        assert_eq!(asg[0].plane, p0);
        assert_eq!(asg[1].connector_name, "HDMI-2");
        assert_eq!(asg[1].crtc, c1);
        assert_eq!(asg[1].plane, p1);
    }

    #[test]
    fn targeted_assignment_avoids_a_live_survivor_route() {
        let live_crtc = rh(10);
        let free_crtc = rh(11);
        let live_plane = ph(20);
        let free_plane = ph(21);
        let candidate = cand(
            1,
            "HDMI-target",
            vec![live_crtc, free_crtc],
            vec![(live_plane, &[live_crtc]), (free_plane, &[free_crtc])],
        );

        let assignment = assign_output_avoiding(&candidate, &[(eh(99), live_crtc, live_plane)])
            .expect("a free route remains");
        assert_eq!(assignment.crtc, free_crtc);
        assert_eq!(assignment.plane, free_plane);
    }

    #[test]
    fn targeted_assignment_tries_a_later_crtc_when_the_first_plane_is_reserved() {
        let first_crtc = rh(10);
        let second_crtc = rh(11);
        let first_plane = ph(20);
        let second_plane = ph(21);
        let candidate = cand(
            1,
            "DP-target",
            vec![first_crtc, second_crtc],
            vec![(first_plane, &[first_crtc]), (second_plane, &[second_crtc])],
        );

        let assignment = assign_output_avoiding(&candidate, &[(eh(99), rh(99), first_plane)])
            .expect("the allocator must try the later compatible pair");
        assert_eq!(assignment.crtc, second_crtc);
        assert_eq!(assignment.plane, second_plane);
    }

    #[test]
    fn targeted_assignment_errors_when_every_compatible_route_is_reserved() {
        let crtc = rh(10);
        let plane = ph(20);
        let candidate = cand(1, "DP-target", vec![crtc], vec![(plane, &[crtc])]);

        let crtc_error = assign_output_avoiding(&candidate, &[(eh(99), crtc, ph(99))])
            .expect_err("reserving only the CRTC must make the route unavailable");
        assert_eq!(crtc_error, "DP-target");
        let plane_error = assign_output_avoiding(&candidate, &[(eh(99), rh(99), plane)])
            .expect_err("reserving only the plane must make the route unavailable");
        assert_eq!(plane_error, "DP-target");

        let encoder_error =
            assign_output_avoiding(&candidate, &[(candidate.encoder, rh(99), ph(99))])
                .expect_err("reserving the connector's encoder must make the route unavailable");
        assert_eq!(encoder_error, "DP-target");
    }

    #[test]
    fn errors_when_connector_has_no_candidate_crtcs() {
        let cands = vec![cand(1, "HDMI-stranded", vec![], vec![])];
        let err = assign_outputs(&cands).expect_err("must error");
        assert_eq!(err, "HDMI-stranded");
    }

    #[test]
    fn errors_on_second_connector_when_one_crtc_shared() {
        let c0 = rh(10);
        let p0 = ph(20);
        let p1 = ph(21);
        let cands = vec![
            cand(1, "HDMI-A", vec![c0], vec![(p0, &[c0])]),
            cand(2, "HDMI-B", vec![c0], vec![(p1, &[c0])]),
        ];
        let err = assign_outputs(&cands).expect_err("must error");
        assert_eq!(err, "HDMI-B");
    }

    #[test]
    fn errors_when_no_plane_can_drive_candidate_crtcs() {
        let c0 = rh(10);
        let c_other = rh(99);
        let p0 = ph(20);
        // plane only drives c_other, which is not a candidate.
        let cands = vec![cand(1, "HDMI-NoPlane", vec![c0], vec![(p0, &[c_other])])];
        let err = assign_outputs(&cands).expect_err("must error");
        assert_eq!(err, "HDMI-NoPlane");
    }

    #[test]
    fn parses_in_formats_modifiers_for_xrgb8888() {
        let mut blob = Vec::new();
        let formats = [0x1111_1111, DrmFourcc::Xrgb8888 as u32];
        let formats_offset = 24_u32;
        let modifiers_offset = 32_u32;
        blob.extend_from_slice(&1_u32.to_ne_bytes()); // version
        blob.extend_from_slice(&0_u32.to_ne_bytes()); // flags
        blob.extend_from_slice(&(formats.len() as u32).to_ne_bytes());
        blob.extend_from_slice(&formats_offset.to_ne_bytes());
        blob.extend_from_slice(&2_u32.to_ne_bytes()); // count_modifiers
        blob.extend_from_slice(&modifiers_offset.to_ne_bytes());
        for format in formats {
            blob.extend_from_slice(&format.to_ne_bytes());
        }

        // Modifier 0 applies to format index 0 only; modifier 1 applies
        // to format index 1 (XRGB8888).
        blob.extend_from_slice(&1_u64.to_ne_bytes()); // formats bitset
        blob.extend_from_slice(&0_u32.to_ne_bytes()); // offset
        blob.extend_from_slice(&0_u32.to_ne_bytes()); // pad
        blob.extend_from_slice(&0xaaaa_u64.to_ne_bytes());
        blob.extend_from_slice(&(1_u64 << 1).to_ne_bytes());
        blob.extend_from_slice(&0_u32.to_ne_bytes());
        blob.extend_from_slice(&0_u32.to_ne_bytes());
        blob.extend_from_slice(&0xbbbb_u64.to_ne_bytes());

        let modifiers = parse_in_formats_modifiers(&blob, DrmFourcc::Xrgb8888 as u32);
        assert_eq!(modifiers, vec![0xbbbb]);
    }
}
