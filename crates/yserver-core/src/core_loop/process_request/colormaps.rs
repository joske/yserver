use super::*;

/// StoreColors (89): always BadAccess on TrueColor (read-only) colormaps.
pub(super) fn handle_store_colors(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
) -> io::Result<RequestOutcome> {
    debug!("client {} #{} StoreColors", client_id.0, sequence.0);
    emit_x11_error(state, client_id, sequence, x11::error::BAD_ACCESS, 0, 89)
}

/// StoreNamedColor (90): same — TrueColor → BadAccess.
pub(super) fn handle_store_named_color(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
) -> io::Result<RequestOutcome> {
    debug!("client {} #{} StoreNamedColor", client_id.0, sequence.0);
    emit_x11_error(state, client_id, sequence, x11::error::BAD_ACCESS, 0, 90)
}

/// AllocColorCells (86): always BadAlloc on TrueColor visuals.
/// X11 spec: cells/planes can only be allocated from
/// DirectColor/PseudoColor/GrayScale colormaps. Our default
/// colormaps are TrueColor → always BadAlloc.
pub(super) fn handle_alloc_color_cells(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    _body: &[u8],
) -> io::Result<RequestOutcome> {
    debug!("client {} #{} AllocColorCells", client_id.0, sequence.0);
    emit_x11_error(state, client_id, sequence, x11::error::BAD_ALLOC, 0, 86)
}

/// AllocColorPlanes (87): same — TrueColor → BadAlloc.
pub(super) fn handle_alloc_color_planes(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    _body: &[u8],
) -> io::Result<RequestOutcome> {
    debug!("client {} #{} AllocColorPlanes", client_id.0, sequence.0);
    emit_x11_error(state, client_id, sequence, x11::error::BAD_ALLOC, 0, 87)
}

pub(super) fn handle_create_colormap(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    if body.len() < 12 {
        return Ok(RequestOutcome::Handled);
    }
    let mid = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
    if state.resources.xid_in_use(mid) || xid_out_of_client_range(state, client_id, mid.0) {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_ID_CHOICE,
            mid.0,
            78,
        );
    }
    let visual = ResourceId(u32::from_le_bytes([body[8], body[9], body[10], body[11]]));
    state.resources.create_colormap(client_id, mid, visual);
    debug!(
        "client {} #{} CreateColormap 0x{:x}",
        client_id.0, sequence.0, mid.0
    );
    Ok(RequestOutcome::Handled)
}

/// CopyColormapAndFree (80): allocate new colormap, BadIDChoice on duplicate.
pub(super) fn handle_copy_colormap_and_free(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    if body.len() < 8 {
        return Ok(RequestOutcome::Handled);
    }
    let mid = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
    if state.resources.xid_in_use(mid) || xid_out_of_client_range(state, client_id, mid.0) {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_ID_CHOICE,
            mid.0,
            80,
        );
    }
    // Source colormap's visual; default to ROOT_VISUAL if unknown.
    let src_id = ResourceId(u32::from_le_bytes([body[4], body[5], body[6], body[7]]));
    let visual = state
        .resources
        .colormap(src_id)
        .map(|c| c.visual)
        .unwrap_or(crate::resources::ROOT_VISUAL);
    state.resources.create_colormap(client_id, mid, visual);
    debug!(
        "client {} #{} CopyColormapAndFree 0x{:x}",
        client_id.0, sequence.0, mid.0
    );
    Ok(RequestOutcome::Handled)
}

/// ListInstalledColormaps (83): reply with 0 colormaps.
/// Emit `ColormapNotify(window, cmap, new=false, installed=…)` to every
/// window whose `attributes.colormap == cmap`, for clients subscribed
/// with the ColormapChange event mask bit (`1 << 23`).
fn emit_colormap_notify_for(state: &mut ServerState, cmap: ResourceId, installed: bool) {
    let windows = state.resources.windows_with_colormap(cmap);
    for w in windows {
        let _dropped = crate::core_loop::fanout::emit_window_event_to_state(
            state,
            w,
            0x0080_0000,
            |buf, seq, order| {
                x11::encode_colormap_notify_event(buf, seq, order, w, cmap, false, installed);
            },
        );
    }
}

/// Restore the default colormap to the installed list if a prior
/// uninstall / free emptied it. X11 spec: "Initially, the default
/// colormap for a screen is installed" plus the server may implicitly
/// install required colormaps; ROOT_COLORMAP is the lone required
/// entry on our TrueColor setup.
fn ensure_default_colormap_installed(state: &mut ServerState) {
    if !state.installed_colormaps.is_empty() {
        return;
    }
    let root = crate::resources::ROOT_COLORMAP;
    state.installed_colormaps.push(root);
    emit_colormap_notify_for(state, root, true);
}

/// InstallColormap (81): mark a colormap as installed and emit
/// `ColormapNotify(Installed)` on every window that uses it. On a
/// TrueColor server this is bookkeeping only — the hardware colormap
/// never actually changes — but clients (and xts) still observe the
/// notify chain.
pub(super) fn handle_install_colormap(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    if body.len() < 4 {
        return Ok(RequestOutcome::Handled);
    }
    let cmap = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
    debug!(
        "client {} #{} InstallColormap 0x{:x}",
        client_id.0, sequence.0, cmap.0
    );
    if state.resources.colormap(cmap).is_none() {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_COLORMAP,
            cmap.0,
            81,
        );
    }
    if state.installed_colormaps.contains(&cmap) {
        return Ok(RequestOutcome::Handled);
    }
    // Capacity = `max_installed_maps = 1` (matches the SETUP advertise).
    // Evict oldest to make room — its own `ColormapNotify(Uninstalled)`
    // fans out before the new install's notify.
    if !state.installed_colormaps.is_empty() {
        let evicted = state.installed_colormaps.remove(0);
        emit_colormap_notify_for(state, evicted, false);
    }
    state.installed_colormaps.push(cmap);
    emit_colormap_notify_for(state, cmap, true);
    Ok(RequestOutcome::Handled)
}

/// FreeColormap (79): destroy the colormap resource. Uninstalls it
/// first if installed (emitting `ColormapNotify(Uninstalled)`). The
/// default colormap is not freeable — BadColor per spec.
pub(super) fn handle_free_colormap(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    if body.len() < 4 {
        return Ok(RequestOutcome::Handled);
    }
    let cmap = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
    debug!(
        "client {} #{} FreeColormap 0x{:x}",
        client_id.0, sequence.0, cmap.0
    );
    if cmap == crate::resources::ROOT_COLORMAP || state.resources.colormap(cmap).is_none() {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_COLORMAP,
            cmap.0,
            79,
        );
    }
    let was_installed = state.installed_colormaps.contains(&cmap);
    state.installed_colormaps.retain(|c| *c != cmap);
    if was_installed {
        emit_colormap_notify_for(state, cmap, false);
    }
    state.resources.free_colormap(cmap);
    ensure_default_colormap_installed(state);
    Ok(RequestOutcome::Handled)
}

/// UninstallColormap (82): symmetric to `InstallColormap`. Uninstalling
/// the default colormap is allowed but the server implicitly re-installs
/// it later when focus changes; we model the bookkeeping faithfully.
pub(super) fn handle_uninstall_colormap(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    if body.len() < 4 {
        return Ok(RequestOutcome::Handled);
    }
    let cmap = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
    debug!(
        "client {} #{} UninstallColormap 0x{:x}",
        client_id.0, sequence.0, cmap.0
    );
    if state.resources.colormap(cmap).is_none() {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_COLORMAP,
            cmap.0,
            82,
        );
    }
    let before = state.installed_colormaps.len();
    state.installed_colormaps.retain(|c| *c != cmap);
    if state.installed_colormaps.len() == before {
        return Ok(RequestOutcome::Handled);
    }
    emit_colormap_notify_for(state, cmap, false);
    ensure_default_colormap_installed(state);
    Ok(RequestOutcome::Handled)
}

pub(super) fn handle_list_installed_colormaps(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    // Body's window xid scopes the reply to the window's screen.
    // yserver has a single screen so the list is server-global, but
    // we still validate the xid — xts probes BadWindow on a freed xid.
    let window = if body.len() >= 4 {
        ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]))
    } else {
        ResourceId(0)
    };
    debug!(
        "client {} #{} ListInstalledColormaps 0x{:x}",
        client_id.0, sequence.0, window.0
    );
    if state.resources.window(window).is_none() {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_WINDOW,
            window.0,
            83,
        );
    }
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let byte_order = client.byte_order;
    let installed = state.installed_colormaps.clone();
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    // Layout: reply(1) pad(1) seq(2) length=N(4) nColormaps u16(2)
    //         pad(22)  colormaps u32[N]
    // Length field counts u32 units past the 32-byte fixed header.
    let length_units = u32::try_from(installed.len()).unwrap_or(0);
    let mut buf = x11::fixed_reply(byte_order, sequence, 0, length_units);
    let mut tmp = Vec::with_capacity(2);
    x11::write_u16(
        byte_order,
        &mut tmp,
        u16::try_from(installed.len()).unwrap_or(0),
    );
    buf.extend_from_slice(&tmp);
    buf.resize(32, 0);
    for cmap in &installed {
        let mut entry = Vec::with_capacity(4);
        x11::write_u32(byte_order, &mut entry, cmap.0);
        buf.extend_from_slice(&entry);
    }
    Ok(write_to_client(client, client_id, &buf))
}

pub(super) fn handle_alloc_color(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    debug!("client {} #{} AllocColor", client_id.0, sequence.0);
    let color = x11::alloc_color_request(body).unwrap_or_default();
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let byte_order = client.byte_order;
    let mut buf: Vec<u8> = Vec::with_capacity(32);
    x11::write_alloc_color_reply(&mut buf, byte_order, sequence, color)?;
    Ok(write_to_client(client, client_id, &buf))
}

/// FreeColors (88): release a client's color allocations.
///
/// yserver currently exposes only fixed TrueColor visuals and does not retain
/// per-client references for the read-only colors returned by AllocColor /
/// AllocNamedColor. Validate the colormap and the Xorg-detectable BadValue
/// cases, then complete without a reply. Xorg additionally returns BadAccess
/// for unallocated or already-freed pixels; without allocation accounting,
/// yserver deliberately accepts those frees as a permissive no-op.
pub(super) fn handle_free_colors(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    let Some(prefix) = body.get(..8) else {
        return Ok(RequestOutcome::Handled);
    };
    let cmap = ResourceId(u32::from_le_bytes(
        prefix[..4].try_into().expect("four-byte colormap id"),
    ));
    let plane_mask = u32::from_le_bytes(prefix[4..8].try_into().expect("four-byte plane mask"));
    let mut pixels = body[8..]
        .chunks_exact(4)
        .map(|pixel| u32::from_le_bytes(pixel.try_into().expect("four-byte pixel")));
    let pixel_count = pixels.len();
    debug!(
        "client {} #{} FreeColors cmap=0x{:x} plane_mask=0x{:08x} pixels={}",
        client_id.0, sequence.0, cmap.0, plane_mask, pixel_count
    );
    let Some(colormap) = state.resources.colormap(cmap) else {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_COLORMAP,
            cmap.0,
            88,
        );
    };
    let Some(visual) = state.resources.visual(colormap.visual) else {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_COLORMAP,
            cmap.0,
            88,
        );
    };
    let valid_mask = visual.red_mask | visual.green_mask | visual.blue_mask | visual.alpha_mask;
    if pixel_count != 0 && plane_mask & !valid_mask != 0 {
        let first_pixel = pixels.clone().next().expect("non-empty pixel list");
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_VALUE,
            first_pixel | plane_mask,
            88,
        );
    }
    if let Some(invalid_pixel) = pixels.find(|pixel| pixel & !valid_mask != 0) {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_VALUE,
            invalid_pixel,
            88,
        );
    }
    Ok(RequestOutcome::Handled)
}

pub(super) fn handle_alloc_named_color(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    let name = x11::alloc_named_color_name(body);
    let Some(color) = x11::lookup_color_name(&name) else {
        // Unknown color name → BadName (Xorg), not a silent gray
        // fallback that hands the client the wrong pixel.
        debug!(
            "client {} #{} AllocNamedColor unknown name {:?} -> BadName",
            client_id.0, sequence.0, name
        );
        return emit_x11_error(state, client_id, sequence, x11::error::BAD_NAME, 0, 85);
    };
    debug!(
        "client {} #{} AllocNamedColor {:?}",
        client_id.0, sequence.0, name
    );
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let byte_order = client.byte_order;
    let mut buf: Vec<u8> = Vec::with_capacity(32);
    x11::write_alloc_named_color_reply(&mut buf, byte_order, sequence, color)?;
    Ok(write_to_client(client, client_id, &buf))
}

pub(super) fn handle_query_colors(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    let pixels = x11::query_colors_pixels(body);
    debug!(
        "client {} #{} QueryColors {} pixels",
        client_id.0,
        sequence.0,
        pixels.len()
    );
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let byte_order = client.byte_order;
    let mut buf: Vec<u8> = Vec::with_capacity(32 + pixels.len() * 8);
    x11::write_query_colors_reply(&mut buf, byte_order, sequence, &pixels)?;
    Ok(write_to_client(client, client_id, &buf))
}

pub(super) fn handle_lookup_color(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    let name = x11::alloc_named_color_name(body);
    let Some(color) = x11::lookup_color_name(&name) else {
        // Unknown color name → BadName (Xorg), not a silent gray fallback.
        debug!(
            "client {} #{} LookupColor unknown name {:?} -> BadName",
            client_id.0, sequence.0, name
        );
        return emit_x11_error(state, client_id, sequence, x11::error::BAD_NAME, 0, 92);
    };
    debug!(
        "client {} #{} LookupColor {:?}",
        client_id.0, sequence.0, name
    );
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let byte_order = client.byte_order;
    let mut buf: Vec<u8> = Vec::with_capacity(32);
    x11::write_lookup_color_reply(&mut buf, byte_order, sequence, color)?;
    Ok(write_to_client(client, client_id, &buf))
}
