use super::*;

fn rewrite_reply_sequence(reply: &mut [u8], sequence: SequenceNumber) {
    if reply.len() >= 4 {
        let bytes = sequence.0.to_le_bytes();
        reply[2] = bytes[0];
        reply[3] = bytes[1];
    }
}

/// GetFontPath (52): reply with 0 paths.
pub(super) fn handle_get_font_path(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
    sequence: SequenceNumber,
) -> io::Result<RequestOutcome> {
    debug!("client {} #{} GetFontPath", client_id.0, sequence.0);
    let paths = backend.font_path();
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let byte_order = client.byte_order;
    // Layout: reply(1) pad(1) seq(2) length(4) npaths u16(2) pad(22)
    // then LISTofSTR (1 length byte + bytes each), padded to 4.
    let mut buf = x11::fixed_reply(byte_order, sequence, 0, 0);
    let mut tmp = Vec::with_capacity(2);
    x11::write_u16(
        byte_order,
        &mut tmp,
        u16::try_from(paths.len()).unwrap_or(0),
    );
    buf.extend_from_slice(&tmp);
    buf.resize(32, 0);
    for p in &paths {
        let bytes = p.as_bytes();
        let len = bytes.len().min(255);
        buf.push(u8::try_from(len).unwrap_or(255));
        buf.extend_from_slice(&bytes[..len]);
    }
    while !buf.len().is_multiple_of(4) {
        buf.push(0);
    }
    let units = u32::try_from((buf.len() - 32) / 4).unwrap_or(0);
    let len_bytes = match byte_order {
        yserver_protocol::x11::ClientByteOrder::LittleEndian => units.to_le_bytes(),
        yserver_protocol::x11::ClientByteOrder::BigEndian => units.to_be_bytes(),
    };
    buf[4..8].copy_from_slice(&len_bytes);
    Ok(write_to_client(client, client_id, &buf))
}

/// SetFontPath (51): parse the STR list, hand it to the backend for
/// validation + install. Invalid element → BadValue (old path kept,
/// errorValue = offending element index, matching Xorg's
/// SetFontPathElements bad-element counter).
pub(super) fn handle_set_font_path(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    if body.len() < 4 {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_LENGTH,
            0,
            header.opcode,
        );
    }
    let npaths = u16::from_le_bytes([body[0], body[1]]);
    let mut paths: Vec<String> = Vec::with_capacity(usize::from(npaths));
    let mut off = 4usize;
    for _ in 0..npaths {
        let Some(&len) = body.get(off) else {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_LENGTH,
                0,
                header.opcode,
            );
        };
        off += 1;
        let end = off + usize::from(len);
        let Some(bytes) = body.get(off..end) else {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_LENGTH,
                0,
                header.opcode,
            );
        };
        paths.push(String::from_utf8_lossy(bytes).into_owned());
        off = end;
    }
    debug!(
        "client {} #{} SetFontPath {:?}",
        client_id.0, sequence.0, paths
    );
    match backend.set_font_path(origin, &paths) {
        Ok(()) => Ok(RequestOutcome::Handled),
        Err(bad_index) => emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_VALUE,
            u32::try_from(bad_index).unwrap_or(0),
            header.opcode,
        ),
    }
}

pub(super) fn handle_list_fonts(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    if let Some(request) = x11::list_fonts_request(body) {
        debug!(
            "client {} #{} ListFonts max={} pattern={:?}",
            client_id.0, sequence.0, request.max_names, request.pattern
        );
        if let Ok(mut reply) = backend.list_fonts_proxy(origin, request.max_names, &request.pattern)
        {
            let names_returned = u16::from_le_bytes([reply[8], reply[9]]);
            debug!(
                "client {} #{} ListFonts → {} names",
                client_id.0, sequence.0, names_returned
            );
            rewrite_reply_sequence(&mut reply, sequence);
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            let _byte_order = client.byte_order;
            return Ok(write_to_client(client, client_id, &reply));
        }
    } else {
        debug!(
            "client {} #{} ListFonts (unparsed)",
            client_id.0, sequence.0
        );
    }
    Ok(RequestOutcome::Handled)
}

pub(super) fn handle_list_fonts_with_info(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    if let Some(request) = x11::list_fonts_request(body) {
        debug!(
            "client {} #{} ListFontsWithInfo max={} pattern={:?}",
            client_id.0, sequence.0, request.max_names, request.pattern
        );
        // Thread the atom interner so the backend can attach the FONT
        // property (value = atom of the resolved XLFD) — XCreateFontSet
        // resolves non-XLFD base names exclusively through it. Same
        // disjoint-borrow pattern as handle_xkb_request.
        let replies = {
            let atoms = &mut state.atoms;
            let mut intern = |name: &str| atoms.intern(name, false).0;
            backend.list_fonts_with_info_proxy(
                origin,
                request.max_names,
                &request.pattern,
                &mut intern,
            )
        };
        if let Ok(replies) = replies {
            debug!(
                "client {} #{} ListFontsWithInfo → {} replies (incl. terminator)",
                client_id.0,
                sequence.0,
                replies.len()
            );
            for mut reply in replies {
                rewrite_reply_sequence(&mut reply, sequence);
                let Some(client) = state.clients.get_mut(&client_id.0) else {
                    return Ok(RequestOutcome::Handled);
                };
                let _byte_order = client.byte_order;
                let outcome = write_to_client(client, client_id, &reply);
                if matches!(outcome, RequestOutcome::Disconnect(_)) {
                    return Ok(outcome);
                }
            }
        }
    } else {
        debug!(
            "client {} #{} ListFontsWithInfo (unparsed)",
            client_id.0, sequence.0
        );
    }
    Ok(RequestOutcome::Handled)
}

pub(super) fn handle_open_font(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    let Some(request) = x11::open_font_request(body) else {
        debug!(
            "client {} #{} OpenFont (parse failed)",
            client_id.0, sequence.0
        );
        return Ok(RequestOutcome::Handled);
    };
    debug!(
        "client {} #{} OpenFont {:?}",
        client_id.0, sequence.0, request.name
    );
    let new_id = request.font.0;
    let validation_failed = {
        let handle = state.clients.get(&client_id.0).expect("client registered");
        !crate::server::IdAllocator::validate_owned(
            new_id,
            handle.resource_id_base,
            handle.resource_id_mask,
        ) || state.xid_occupied(request.font.0)
    };
    if validation_failed {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_ID_CHOICE,
            new_id,
            45,
        );
    }
    let host_result = match backend.open_font(origin, &request.name) {
        Ok(pair) => Some(pair),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            // Font-path resolution exhausted with no match → BadName
            // (Xorg dix OpenFont semantics). KMS signals this with
            // ErrorKind::NotFound; host-proxy failures use other
            // kinds and keep the sentinel path below.
            debug!(
                "client {} OpenFont {:?}: no match on font path → BadName",
                client_id.0, request.name
            );
            return emit_x11_error(state, client_id, sequence, x11::error::BAD_NAME, 0, 45);
        }
        Err(err) => {
            log::warn!(
                "client {} OpenFont {:?} failed on host: {err}",
                client_id.0,
                request.name
            );
            None
        }
    };
    // Even when the host font open failed, install the font ID locally
    // so subsequent OpenFont calls on the same XID raise BadIDChoice
    // (X11 spec: BadIDChoice fires on duplicate IDs regardless of
    // whether the resource is actually backed). Use a sentinel host
    // handle that drawing paths will recognise as "no glyph data".
    let (host_handle, mut metrics) = host_result.unwrap_or_else(|| {
        (
            crate::backend::FontHandle::from_raw(0xffff_ffff).expect("nonzero sentinel"),
            x11::FontMetrics {
                min_bounds: x11::CharInfo::default(),
                max_bounds: x11::CharInfo::default(),
                min_char_or_byte2: 0,
                max_char_or_byte2: 0,
                default_char: 0,
                draw_direction: 0,
                min_byte1: 0,
                max_byte1: 0,
                all_chars_exist: false,
                font_ascent: 0,
                font_descent: 0,
                properties: Vec::new(),
                named_properties: Vec::new(),
                char_infos: Vec::new(),
            },
        )
    });
    {
        // Properties read from the font file (BDF/PCF) arrive as
        // named (String, value) pairs — convert to wire (atom, CARD32)
        // pairs here where the atom table lives. String values are
        // themselves atoms (BDF ATOM-typed properties).
        if !metrics.named_properties.is_empty() {
            let named = std::mem::take(&mut metrics.named_properties);
            let mut out: Vec<u8> = Vec::with_capacity(named.len() * 8 + 8);
            let mut has_font_prop = false;
            for (name, value) in &named {
                if name == "FONT" {
                    has_font_prop = true;
                }
                let name_atom = state.atoms.intern(name, false).0;
                let v: u32 = match value {
                    x11::FontPropValue::Card(c) => *c,
                    #[allow(clippy::cast_sign_loss)]
                    x11::FontPropValue::Int(i) => *i as u32,
                    x11::FontPropValue::Str(s) => state.atoms.intern(s, false).0,
                };
                out.extend_from_slice(&name_atom.to_le_bytes());
                out.extend_from_slice(&v.to_le_bytes());
            }
            // libX11's XCreateFontSet resolves non-XLFD base names
            // exclusively through the FONT property (omGeneric.c) —
            // always present on Xorg fonts; append if the file's
            // property list lacked it.
            if !has_font_prop {
                let name_atom = state.atoms.intern("FONT", false).0;
                let value_atom = state.atoms.intern(&request.name, false).0;
                out.extend_from_slice(&name_atom.to_le_bytes());
                out.extend_from_slice(&value_atom.to_le_bytes());
            }
            metrics.properties = out;
        }
        // Backends that don't proxy to a real X server (KMS) can't
        // populate font properties from upstream and return an empty
        // properties vec. Synthesize the standard XLFD-derived
        // properties (FOUNDRY, FAMILY_NAME, WEIGHT_NAME, ..., FONT)
        // so apps that interrogate them — fvwm3 menu rendering,
        // Xt/Athena widgets — don't silently skip text drawing.
        if metrics.properties.is_empty() {
            metrics.properties = synthesize_font_properties(&mut state.atoms, &request.name);
            log::debug!(
                "OpenFont {:?}: synthesized {} property bytes ({} props), {} char_infos",
                request.name,
                metrics.properties.len(),
                metrics.properties.len() / 8,
                metrics.char_infos.len(),
            );
        }
        state
            .resources
            .install_font(client_id, request.font, request.name, host_handle, metrics);
    }
    Ok(RequestOutcome::Handled)
}

/// Build a `FontProp` byte sequence (8 bytes per property: name atom +
/// CARD32 value) from an XLFD font name. String-typed properties get
/// their value interned as an atom; integer-typed properties carry
/// the int directly per the XLFD spec.
///
/// XLFD components:
/// `-foundry-family-weight-slant-setwidth-addstyle-pixelsize-pointsize-resx-resy-spacing-avgwidth-charset-encoding`
fn synthesize_font_properties(atoms: &mut crate::server::AtomTable, xlfd: &str) -> Vec<u8> {
    let parts: Vec<&str> = xlfd.split('-').collect();
    // Real XLFDs start with '-' so split yields an empty first element.
    // Names that aren't proper XLFDs (e.g. "fixed", "cursor") get only a
    // FONT property pointing at the literal string — better than empty.
    let mut out: Vec<u8> = Vec::with_capacity(15 * 8);

    let put_string =
        |atoms: &mut crate::server::AtomTable, out: &mut Vec<u8>, name: &str, value: &str| {
            let name_atom = atoms.intern(name, false).0;
            let value_atom = atoms.intern(value, false).0;
            out.extend_from_slice(&name_atom.to_le_bytes());
            out.extend_from_slice(&value_atom.to_le_bytes());
        };
    let put_int =
        |atoms: &mut crate::server::AtomTable, out: &mut Vec<u8>, name: &str, value: i32| {
            let name_atom = atoms.intern(name, false).0;
            out.extend_from_slice(&name_atom.to_le_bytes());
            out.extend_from_slice(&(value as u32).to_le_bytes());
        };

    // FONT property always: the full XLFD as an atom value.
    put_string(atoms, &mut out, "FONT", xlfd);

    // XLFD-shaped name: -foundry-family-weight-slant-setwidth-addstyle-pixel-point-resx-resy-spacing-avgwidth-charset-encoding
    // parts[0] is the empty leading "" before the first '-'.
    if parts.len() >= 15 {
        put_string(atoms, &mut out, "FOUNDRY", parts[1]);
        put_string(atoms, &mut out, "FAMILY_NAME", parts[2]);
        put_string(atoms, &mut out, "WEIGHT_NAME", parts[3]);
        put_string(atoms, &mut out, "SLANT", parts[4]);
        put_string(atoms, &mut out, "SETWIDTH_NAME", parts[5]);
        put_string(atoms, &mut out, "ADD_STYLE_NAME", parts[6]);
        if let Ok(v) = parts[7].parse::<i32>() {
            put_int(atoms, &mut out, "PIXEL_SIZE", v);
        }
        if let Ok(v) = parts[8].parse::<i32>() {
            put_int(atoms, &mut out, "POINT_SIZE", v);
        }
        if let Ok(v) = parts[9].parse::<i32>() {
            put_int(atoms, &mut out, "RESOLUTION_X", v);
        }
        if let Ok(v) = parts[10].parse::<i32>() {
            put_int(atoms, &mut out, "RESOLUTION_Y", v);
        }
        put_string(atoms, &mut out, "SPACING", parts[11]);
        if let Ok(v) = parts[12].parse::<i32>() {
            put_int(atoms, &mut out, "AVERAGE_WIDTH", v);
        }
        put_string(atoms, &mut out, "CHARSET_REGISTRY", parts[13]);
        put_string(atoms, &mut out, "CHARSET_ENCODING", parts[14]);
    }
    out
}

pub(super) fn handle_close_font(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    if let Some(font) = x11::free_resource_id(body) {
        let removed = state.resources.close_font(font);
        if let Some(removed) = removed {
            let _ = backend.close_font(origin, removed.host_xid.as_raw());
        }
    }
    debug!("client {} #{} CloseFont", client_id.0, sequence.0);
    Ok(RequestOutcome::Handled)
}

pub(super) fn handle_query_font(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    debug!("client {} #{} QueryFont", client_id.0, sequence.0);
    let font_id = x11::drawable_request_id(body);
    let Some(metrics) = font_id
        .and_then(|id| state.resources.fontable(id))
        .map(|font| font.metrics.clone())
    else {
        // Unknown/invalid fontable → BadFont (Xorg), not a zeroed
        // metrics reply that misleads the client into thinking the
        // font loaded.
        let bad = font_id.map_or(0, |id| id.0);
        return emit_x11_error(state, client_id, sequence, x11::error::BAD_FONT, bad, 47);
    };
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let byte_order = client.byte_order;
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    x11::write_query_font_reply(&mut buf, byte_order, sequence, &metrics)?;
    Ok(write_to_client(client, client_id, &buf))
}

pub(super) fn handle_query_text_extents(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    debug!("client {} #{} QueryTextExtents", client_id.0, sequence.0);
    let req = x11::query_text_extents_request(header.data, body);
    let extents = req.as_ref().and_then(|req| {
        state
            .resources
            .fontable(req.fontable)
            .map(|font| font.metrics.text_extents(&req.chars))
    });
    let Some(extents) = extents else {
        // Unknown/invalid fontable → BadFont (Xorg), not zeroed extents.
        let bad = req.map_or(0, |r| r.fontable.0);
        return emit_x11_error(state, client_id, sequence, x11::error::BAD_FONT, bad, 48);
    };
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let byte_order = client.byte_order;
    let mut buf: Vec<u8> = Vec::with_capacity(32);
    x11::write_query_text_extents_reply(&mut buf, byte_order, sequence, extents)?;
    Ok(write_to_client(client, client_id, &buf))
}
