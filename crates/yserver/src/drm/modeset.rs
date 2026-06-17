use std::{
    collections::{HashMap, HashSet},
    io,
};

use drm::{
    buffer::DrmFourcc,
    control::{
        AtomicCommitFlags, Device as ControlDevice, Mode as DrmMode, ModeTypeFlags, PlaneType,
        atomic::AtomicModeReq, connector, crtc, encoder, framebuffer, plane, property,
    },
};

use crate::drm::Device;

#[derive(Debug, Clone)]
pub struct Mode {
    pub name: String,
    pub width: u16,
    pub height: u16,
    pub vrefresh: u32,
    pub preferred: bool,
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

fn parse_mode_spec(spec: &str) -> Option<(u16, u16)> {
    let (w, h) = spec.split_once('x')?;
    let w: u16 = w.trim().parse().ok()?;
    let h: u16 = h.trim().parse().ok()?;
    Some((w, h))
}

fn local_mode_from(m: &DrmMode) -> Mode {
    let (w, h) = m.size();
    Mode {
        name: m.name().to_string_lossy().into_owned(),
        width: w,
        height: h,
        vrefresh: m.vrefresh(),
        preferred: m.mode_type().contains(ModeTypeFlags::PREFERRED),
    }
}

#[derive(Debug)]
pub struct Output {
    pub connector: connector::Handle,
    pub connector_name: String,
    pub crtc: crtc::Handle,
    pub plane: plane::Handle,
    pub mode: DrmMode,
    pub picked: Mode,
    pub plane_fb_id_prop: property::Handle,
    pub plane_crtc_id_prop: property::Handle,
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
    /// The connector's full local mode list, preferred-first, as
    /// reported by the kernel/EDID. `picked` is the boot default and
    /// is always present in this list. Used by RANDR to advertise the
    /// selectable mode set (`GetOutputInfo` / `GetScreenResources`) and
    /// by `apply_crtc_config` to resolve a client-requested mode.
    pub modes: Vec<Mode>,
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
    // Step 3 will surface the bound encoder on `Output`; keep it on the
    // assignment so that change is local to `discover_outputs`.
    #[allow(dead_code)]
    pub encoder: encoder::Handle,
    pub crtc: crtc::Handle,
    pub plane: plane::Handle,
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
/// - no connector is connected at all (typical when running without
///   `vng --graphics`)
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
    let mut primary_planes: Vec<(plane::Handle, HashSet<crtc::Handle>)> = Vec::new();
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
        let drivable: HashSet<crtc::Handle> = resources
            .filter_crtcs(info.possible_crtcs())
            .into_iter()
            .collect();
        primary_planes.push((handle, drivable));
    }

    // Build candidates for every connected connector with usable modes.
    let mut candidates: Vec<ConnectorCandidate> = Vec::new();
    let mut connector_infos: HashMap<connector::Handle, connector::Info> = HashMap::new();
    for &handle in resources.connectors() {
        let info = device.get_connector(handle, false)?;
        if info.state() != connector::State::Connected || info.modes().is_empty() {
            continue;
        }
        let connector_name = format!("{info}");
        let encoder_handle = info
            .current_encoder()
            .or_else(|| info.encoders().first().copied())
            .ok_or_else(|| {
                io::Error::other(format!("connector {connector_name} has no usable encoder"))
            })?;
        let encoder_info = device.get_encoder(encoder_handle)?;
        let mut candidate_crtcs: Vec<crtc::Handle> =
            resources.filter_crtcs(encoder_info.possible_crtcs());
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
        let candidate_planes: Vec<(plane::Handle, HashSet<crtc::Handle>)> = primary_planes
            .iter()
            .filter(|(_, drivable)| drivable.iter().any(|c| candidate_crtc_set.contains(c)))
            .cloned()
            .collect();

        candidates.push(ConnectorCandidate {
            connector: handle,
            connector_name,
            encoder: encoder_handle,
            candidate_crtcs,
            candidate_planes,
        });
        connector_infos.insert(handle, info);
    }

    if candidates.is_empty() {
        return Err(io::Error::other(
            "no connected output — vng with --graphics required for modeset path; \
             headless mode does not exercise this",
        ));
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

fn finalize_output(
    device: &Device,
    asg: Assignment,
    connector_info: &connector::Info,
) -> io::Result<Output> {
    let local_modes: Vec<Mode> = connector_info.modes().iter().map(local_mode_from).collect();
    let picked = pick_mode(&local_modes)
        .ok_or_else(|| {
            io::Error::other(format!(
                "connector {} reports no usable modes",
                asg.connector_name
            ))
        })?
        .clone();
    let picked_idx = local_modes
        .iter()
        .position(|m| {
            m.name == picked.name
                && m.width == picked.width
                && m.height == picked.height
                && m.vrefresh == picked.vrefresh
        })
        .expect("picked mode is from local_modes");
    let drm_mode = connector_info.modes()[picked_idx];

    let plane_props_map = PropMap::for_object(device, asg.plane)?;
    let plane_fb_id_prop = plane_props_map.id("FB_ID")?;
    let plane_crtc_id_prop = plane_props_map.id("CRTC_ID")?;
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

    // Full advertised mode list, sorted preferred-first (matching Xorg
    // GetOutputInfo's nPreferred prefix). `local_modes` is kept in kernel
    // order above for the `picked_idx` -> DRM-mode mapping; we only
    // reorder this owned copy now that `drm_mode` is resolved.
    let mut modes = local_modes;
    modes.sort_by_key(|m| !m.preferred);

    Ok(Output {
        connector: asg.connector,
        connector_name: asg.connector_name,
        crtc: asg.crtc,
        plane: asg.plane,
        mode: drm_mode,
        picked,
        plane_fb_id_prop,
        plane_crtc_id_prop,
        plane_in_fence_fd_prop,
        crtc_out_fence_ptr_prop,
        scanout_modifiers,
        mm_width,
        mm_height,
        modes,
    })
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
    let connector_props = PropMap::for_object(device, output.connector)?;
    let crtc_props = PropMap::for_object(device, output.crtc)?;
    let plane_props = PropMap::for_object(device, output.plane)?;

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
        connector_props.id("CRTC_ID")?,
        u64::from(crtc_id_raw),
    );
    req.add_raw_property(output.crtc.into(), crtc_props.id("MODE_ID")?, mode_blob_raw);
    req.add_raw_property(output.crtc.into(), crtc_props.id("ACTIVE")?, 1);
    req.add_raw_property(
        output.plane.into(),
        plane_props.id("FB_ID")?,
        u64::from(fb_id_raw),
    );
    req.add_raw_property(
        output.plane.into(),
        plane_props.id("CRTC_ID")?,
        u64::from(plane_crtc_raw),
    );
    req.add_raw_property(output.plane.into(), plane_props.id("SRC_X")?, 0);
    req.add_raw_property(output.plane.into(), plane_props.id("SRC_Y")?, 0);
    req.add_raw_property(output.plane.into(), plane_props.id("SRC_W")?, src_w);
    req.add_raw_property(output.plane.into(), plane_props.id("SRC_H")?, src_h);
    req.add_raw_property(output.plane.into(), plane_props.id("CRTC_X")?, 0);
    req.add_raw_property(output.plane.into(), plane_props.id("CRTC_Y")?, 0);
    req.add_raw_property(
        output.plane.into(),
        plane_props.id("CRTC_W")?,
        u64::from(mode_w),
    );
    req.add_raw_property(
        output.plane.into(),
        plane_props.id("CRTC_H")?,
        u64::from(mode_h),
    );

    let result = device.atomic_commit(AtomicCommitFlags::ALLOW_MODESET, req);
    let _ = device.destroy_property_blob(mode_blob_raw);
    result.map_err(|err| {
        io::Error::new(
            err.kind(),
            format!(
                "atomic modeset commit rejected (mode {}, {}x{}): {err}",
                output.picked.name, output.picked.width, output.picked.height
            ),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(name: &str, w: u16, h: u16, refresh: u32, preferred: bool) -> Mode {
        Mode {
            name: name.into(),
            width: w,
            height: h,
            vrefresh: refresh,
            preferred,
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
