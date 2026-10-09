use super::*;

/// Clip RENDER `FillRectangles` wire rects (8-byte `xRectangle`s) to a
/// `ClipByChildren` window's effective region — i.e. subtract the
/// window's mapped `InputOutput` children. Returns the surviving rects
/// re-encoded as wire bytes; an empty `Vec` means the paint is a no-op
/// (window fully covered by a child, as with a systray socket under its
/// XEMBED icon — Xorg's empty composite clip → nothing drawn, icon
/// preserved). For pixmaps / childless windows the rects pass through
/// unchanged.
fn clip_fill_rects_by_children(
    state: &ServerState,
    dst_drawable: ResourceId,
    rects: &[u8],
) -> Vec<u8> {
    use yserver_protocol::x11::xfixes;

    let child_rects = crate::core_loop::damage_fanout::mapped_child_clip_rects(state, dst_drawable);
    if child_rects.is_empty() {
        return rects.to_vec();
    }

    let requested: Vec<xfixes::RegionRect> = rects
        .chunks_exact(8)
        .map(|c| xfixes::RegionRect {
            x: i16::from_le_bytes([c[0], c[1]]),
            y: i16::from_le_bytes([c[2], c[3]]),
            width: u16::from_le_bytes([c[4], c[5]]),
            height: u16::from_le_bytes([c[6], c[7]]),
        })
        .collect();

    let clipped = crate::nested::subtract_regions(&requested, &child_rects);

    crate::core_loop::damage_fanout::log_clip_by_children_debug(
        state,
        dst_drawable,
        "fill",
        &requested,
        &clipped,
    );

    let mut out = Vec::with_capacity(clipped.len() * 8);
    for r in clipped {
        out.extend_from_slice(&r.x.to_le_bytes());
        out.extend_from_slice(&r.y.to_le_bytes());
        out.extend_from_slice(&r.width.to_le_bytes());
        out.extend_from_slice(&r.height.to_le_bytes());
    }
    out
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) fn handle_render_request(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    use crate::{
        nested::{ChangePictureAttr, change_picture_translate_xids},
        resources::{ARGB_VISUAL, GlyphSetState, PictureState},
    };
    use yserver_protocol::x11::ClientByteOrder;
    let byte_order = state
        .clients
        .get(&client_id.0)
        .map_or(ClientByteOrder::LittleEndian, |c| c.byte_order);
    let minor = header.data;
    // RenderErrBase + BadPicture for the first unknown Picture, in Xorg's per-request order.
    macro_rules! verify_pictures {
        ($($id:expr),+) => {
            if let Some(bad) = first_missing_picture(state, &[$($id),+]) {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    crate::nested::RENDER_FIRST_ERROR + 1,
                    bad.0,
                    u16::from(minor),
                    header.opcode,
                );
            }
        };
    }
    match minor {
        0 => {
            let (major, minor_ver) = backend.render_query_version(origin).unwrap_or((0, 11));
            let mut buf: Vec<u8> = Vec::with_capacity(32);
            x11::write_render_query_version_reply(
                &mut buf, byte_order, sequence, major, minor_ver,
            )?;
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            let _byte_order = client.byte_order;
            return Ok(write_to_client(client, client_id, &buf));
        }
        1 => {
            let mut buf: Vec<u8> = Vec::with_capacity(256);
            x11::write_render_query_pict_formats_reply(
                &mut buf,
                byte_order,
                sequence,
                crate::resources::ROOT_VISUAL,
                ARGB_VISUAL,
                crate::resources::GLMARK_VISUAL,
            )?;
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            let _byte_order = client.byte_order;
            return Ok(write_to_client(client, client_id, &buf));
        }
        2 => {
            let mut buf: Vec<u8> = Vec::with_capacity(32);
            x11::write_render_query_pict_index_values_reply(&mut buf, byte_order, sequence)?;
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            let _byte_order = client.byte_order;
            return Ok(write_to_client(client, client_id, &buf));
        }
        4 => {
            let Some(req) = x11::render_create_picture_request(body) else {
                return Ok(RequestOutcome::Handled);
            };
            // Xorg dixLookupDrawable (render.c:575): BadDrawable if unknown, BadMatch for
            // an InputOnly window (dix/dixutils.c:208-213).
            let input_only = match state.resources.window(req.drawable) {
                Some(w) => w.class == crate::resources::WindowClass::InputOnly,
                None if state.resources.pixmap(req.drawable).is_some() => false,
                None => {
                    return emit_x11_error_with_minor(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_DRAWABLE,
                        req.drawable.0,
                        u16::from(minor),
                        header.opcode,
                    );
                }
            };
            if input_only {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_MATCH,
                    req.drawable.0,
                    u16::from(minor),
                    header.opcode,
                );
            }
            let damage_drawable = render_picture_damage_drawable(state, req.drawable);
            let drawable_origin = state
                .resources
                .window(req.drawable)
                .map(|w| (w.x, w.y))
                .unwrap_or((0, 0));
            // A window Picture names the window itself, never its redirect backing: the
            // backend resolves the window's current storage or backing at each use.
            let picture_window = state.resources.window(req.drawable).map(|_| req.drawable);
            let host_drawable_handle = match state.resources.window(req.drawable) {
                Some(w) => w.host_xid.map(crate::backend::AnyHandle::Window),
                None => state
                    .resources
                    .host_drawable_target(req.drawable)
                    .map(|t| t.host_handle()),
            };
            let host_pic = host_drawable_handle.and_then(|host_drawable| {
                backend
                    .render_create_picture(
                        origin,
                        host_drawable,
                        req.format,
                        req.value_mask,
                        &req.values,
                    )
                    .ok()
                    .flatten()
            });
            // Core's registry is authoritative: no X error was sent, so the Picture exists.
            match host_pic {
                Some(hp) => backend.set_picture_drawable_origin(hp.as_raw(), drawable_origin),
                None => debug!(
                    "client {} #{} RENDER::CreatePicture 0x{:x}: backend could not back it; ops on it are no-ops",
                    client_id.0, sequence.0, req.picture.0
                ),
            }
            state.resources.create_picture(
                req.picture,
                PictureState {
                    client: client_id,
                    host_picture_xid: host_pic,
                    host_owned_pixmap: None,
                    kind: crate::resources::PictureKind::Drawable,
                    drawable: Some(damage_drawable),
                    window: picture_window,
                },
            );
        }
        5 => {
            if body.len() < 8 {
                return Ok(RequestOutcome::Handled);
            }
            let pic_id = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
            verify_pictures!(pic_id);
            let value_mask = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
            let translated = change_picture_translate_xids(value_mask, &body[8..], |attr, xid| {
                let resource = ResourceId(xid);
                match attr {
                    ChangePictureAttr::ClipMask => state
                        .resources
                        .pixmap(resource)
                        .and_then(|p| p.host_xid)
                        .map(|h| h.as_raw()),
                    ChangePictureAttr::AlphaMap => state
                        .resources
                        .picture(resource)
                        .and_then(|p| p.host_picture_xid)
                        .map(|h| h.as_raw()),
                }
            });
            let Some(translated_values) = translated else {
                return Ok(RequestOutcome::Handled);
            };
            let mut patched = Vec::with_capacity(8 + translated_values.len());
            patched.extend_from_slice(&body[..8]);
            patched.extend_from_slice(&translated_values);
            let host_pic = state
                .resources
                .picture(pic_id)
                .and_then(|p| p.host_picture_xid)
                .map(|h| h.as_raw());
            if let Some(hp) = host_pic {
                let _ = backend.render_change_picture(origin, hp, &patched);
            }
        }
        6 => {
            if body.len() < 8 {
                return Ok(RequestOutcome::Handled);
            }
            let pic_id = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
            verify_pictures!(pic_id);
            let host_pic = state
                .resources
                .picture(pic_id)
                .and_then(|p| p.host_picture_xid)
                .map(|h| h.as_raw());
            if let Some(hp) = host_pic {
                let _ = backend.render_set_picture_clip_rectangles(origin, hp, body);
            }
        }
        7 => {
            let Some(pic_id) = x11::render_free_resource_id(body) else {
                return Ok(RequestOutcome::Handled);
            };
            verify_pictures!(pic_id);
            let st = state.resources.free_picture(pic_id);
            if let Some(st) = st {
                if let Some(hp) = st.host_picture_xid {
                    let _ = backend.render_free_picture(origin, hp.as_raw());
                }
                if let Some(pix) = st.host_owned_pixmap {
                    let _ = backend.free_pixmap(origin, pix.as_raw());
                }
            }
        }
        8 => {
            let Some(req) = x11::render_composite_request(body) else {
                return Ok(RequestOutcome::Handled);
            };
            verify_pictures!(req.dst);
            if dst_picture_is_sourceless(state, req.dst) {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_DRAWABLE,
                    req.dst.0,
                    u16::from(minor),
                    header.opcode,
                );
            }
            verify_pictures!(req.src);
            if req.mask.0 != 0 {
                verify_pictures!(req.mask);
            }
            if dst_picture_window_unviewable(state, req.dst) {
                return Ok(RequestOutcome::Handled);
            }
            let host_src = state
                .resources
                .picture(req.src)
                .and_then(|p| p.host_picture_xid)
                .map(|h| h.as_raw());
            let host_mask = if req.mask.0 == 0 {
                Some(0)
            } else {
                state
                    .resources
                    .picture(req.mask)
                    .and_then(|p| p.host_picture_xid)
                    .map(|h| h.as_raw())
            };
            let host_dst = state
                .resources
                .picture(req.dst)
                .and_then(|p| p.host_picture_xid)
                .map(|h| h.as_raw());
            if let (Some(host_src), Some(host_mask), Some(host_dst)) =
                (host_src, host_mask, host_dst)
            {
                let painted = backend
                    .render_composite(
                        origin, req.op, host_src, host_mask, host_dst, req.src_x, req.src_y,
                        req.mask_x, req.mask_y, req.dst_x, req.dst_y, req.width, req.height,
                    )
                    .unwrap_or_default();
                if let Some(dst_drawable) =
                    state.resources.picture(req.dst).and_then(|p| p.drawable)
                {
                    for r in &painted {
                        let _dropped = accumulate_damage_to_state(
                            state,
                            dst_drawable,
                            r.x,
                            r.y,
                            r.width,
                            r.height,
                        );
                    }
                }
            }
        }
        10..=13 => {
            // Trapezoids (10), Triangles (11), TriStrip (12), TriFan (13).
            // All four share the same fixed prefix; the variable body
            // layout differs and is handled by the backend.
            if body.len() < 20 {
                return Ok(RequestOutcome::Handled);
            }
            let op = body[0];
            let src = ResourceId(u32::from_le_bytes([body[4], body[5], body[6], body[7]]));
            let dst = ResourceId(u32::from_le_bytes([body[8], body[9], body[10], body[11]]));
            let ynest_mask_format = u32::from_le_bytes([body[12], body[13], body[14], body[15]]);
            let src_x = i16::from_le_bytes([body[16], body[17]]);
            let src_y = i16::from_le_bytes([body[18], body[19]]);
            let primitives = &body[20..];
            verify_pictures!(src, dst);
            if dst_picture_is_sourceless(state, dst) {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_DRAWABLE,
                    dst.0,
                    u16::from(minor),
                    header.opcode,
                );
            }
            if dst_picture_window_unviewable(state, dst) {
                return Ok(RequestOutcome::Handled);
            }
            let host_src = state
                .resources
                .picture(src)
                .and_then(|p| p.host_picture_xid)
                .map(|h| h.as_raw());
            let host_dst = state
                .resources
                .picture(dst)
                .and_then(|p| p.host_picture_xid)
                .map(|h| h.as_raw());
            let host_mask_format = if ynest_mask_format == 0 {
                Some(0u32)
            } else {
                backend.render_format_for_ynest_id(ynest_mask_format)
            };
            if let (Some(host_src), Some(host_dst), Some(host_mask_fmt)) =
                (host_src, host_dst, host_mask_format)
            {
                let painted = if minor == 10 {
                    backend
                        .render_trapezoids(
                            origin,
                            op,
                            host_src,
                            host_dst,
                            host_mask_fmt,
                            src_x,
                            src_y,
                            primitives,
                            0,
                            0,
                        )
                        .unwrap_or_default()
                } else {
                    backend
                        .render_triangles_op(
                            origin,
                            minor,
                            op,
                            host_src,
                            host_dst,
                            host_mask_fmt,
                            src_x,
                            src_y,
                            primitives,
                            0,
                            0,
                        )
                        .unwrap_or_default()
                };
                if let Some(dst_drawable) = state.resources.picture(dst).and_then(|p| p.drawable) {
                    for r in &painted {
                        let _dropped = accumulate_damage_to_state(
                            state,
                            dst_drawable,
                            r.x,
                            r.y,
                            r.width,
                            r.height,
                        );
                    }
                }
            }
        }
        17 => {
            let Some((gs_id, fmt)) = x11::render_create_glyphset_request(body) else {
                return Ok(RequestOutcome::Handled);
            };
            let host_gs = backend.render_create_glyphset(origin, fmt).ok().flatten();
            if let Some(host_gs) = host_gs {
                state.resources.create_glyphset(
                    gs_id,
                    GlyphSetState {
                        client: client_id,
                        host_glyphset_xid: host_gs,
                    },
                );
            }
        }
        18 => {
            let Some((new_glyphset, existing)) = x11::render_reference_glyphset_request(body)
            else {
                return Ok(RequestOutcome::Handled);
            };
            let _ = state
                .resources
                .reference_glyphset(client_id, new_glyphset, existing);
        }
        19 => {
            let Some(gs_id) = x11::render_free_resource_id(body) else {
                return Ok(RequestOutcome::Handled);
            };
            let st = state.resources.free_glyphset(gs_id);
            if let Some(st) = st {
                let _ = backend.render_free_glyphset(origin, st.host_glyphset_xid.as_raw());
            }
        }
        20 => {
            let Some((gs_id, tail)) = x11::render_add_glyphs_request(body) else {
                return Ok(RequestOutcome::Handled);
            };
            let host_gs = state
                .resources
                .glyphset(gs_id)
                .map(|g| g.host_glyphset_xid.as_raw());
            if let Some(host_gs) = host_gs {
                let _ = backend.render_add_glyphs(origin, host_gs, &tail);
            }
        }
        22 => {
            let Some((gs_id, glyph_ids)) = x11::render_free_glyphs_request(body) else {
                return Ok(RequestOutcome::Handled);
            };
            let host_gs = state
                .resources
                .glyphset(gs_id)
                .map(|g| g.host_glyphset_xid.as_raw());
            if let Some(host_gs) = host_gs {
                let _ = backend.render_free_glyphs(origin, host_gs, &glyph_ids);
            }
        }
        23..=25 => {
            let Some(req) = x11::render_composite_glyphs_request(body) else {
                // Silently dropping this draws nothing and reports no
                // error, which looks exactly like "the text is invisible"
                // (#137). Say so.
                debug!(
                    "client {} #{} RENDER::CompositeGlyphs{} -> DROPPED (body not parsed, {} bytes)",
                    client_id.0,
                    sequence.0,
                    8u16 << (u16::from(minor) - 23),
                    body.len(),
                );
                return Ok(RequestOutcome::Handled);
            };
            verify_pictures!(req.src, req.dst);
            if dst_picture_is_sourceless(state, req.dst) {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_DRAWABLE,
                    req.dst.0,
                    u16::from(minor),
                    header.opcode,
                );
            }
            if dst_picture_window_unviewable(state, req.dst) {
                return Ok(RequestOutcome::Handled);
            }
            let host_src = state
                .resources
                .picture(req.src)
                .and_then(|p| p.host_picture_xid)
                .map(|h| h.as_raw());
            let host_dst = state
                .resources
                .picture(req.dst)
                .and_then(|p| p.host_picture_xid)
                .map(|h| h.as_raw());
            let host_gs = state
                .resources
                .glyphset(req.glyphset)
                .map(|g| g.host_glyphset_xid.as_raw());
            if host_src.is_none() || host_dst.is_none() || host_gs.is_none() {
                debug!(
                    "client {} #{} RENDER::CompositeGlyphs src=0x{:x}{} dst=0x{:x}{} glyphset=0x{:x}{} \
                     -> DROPPED (unresolved resource)",
                    client_id.0,
                    sequence.0,
                    req.src.0,
                    if host_src.is_none() { " MISSING" } else { "" },
                    req.dst.0,
                    if host_dst.is_none() { " MISSING" } else { "" },
                    req.glyphset.0,
                    if host_gs.is_none() { " MISSING" } else { "" },
                );
            }
            if let (Some(host_src), Some(host_dst), Some(host_gs)) = (host_src, host_dst, host_gs) {
                let mask_fmt = if req.mask_format == 0 {
                    0
                } else {
                    backend
                        .render_format_for_ynest_id(req.mask_format)
                        .unwrap_or(0)
                };
                let painted = backend
                    .render_composite_glyphs(
                        origin, minor, req.op, host_src, host_dst, mask_fmt, host_gs, req.src_x,
                        req.src_y, &req.items, 0, 0,
                    )
                    .unwrap_or_default();
                debug!(
                    "client {} #{} RENDER::CompositeGlyphs{} op={} src=0x{:x} dst=0x{:x} gs=0x{:x} \
                     mask_format={}->{} items={} src_xy=({},{}) -> painted {} rect(s)",
                    client_id.0,
                    sequence.0,
                    8u16 << (u16::from(minor) - 23),
                    req.op,
                    req.src.0,
                    req.dst.0,
                    req.glyphset.0,
                    req.mask_format,
                    mask_fmt,
                    req.items.len(),
                    req.src_x,
                    req.src_y,
                    painted.len(),
                );
                if let Some(dst_drawable) =
                    state.resources.picture(req.dst).and_then(|p| p.drawable)
                {
                    for r in &painted {
                        let _dropped = accumulate_damage_to_state(
                            state,
                            dst_drawable,
                            r.x,
                            r.y,
                            r.width,
                            r.height,
                        );
                    }
                }
            }
        }
        26 => {
            let Some(req) = x11::render_fill_rectangles_request(body) else {
                return Ok(RequestOutcome::Handled);
            };
            verify_pictures!(req.dst);
            if dst_picture_is_sourceless(state, req.dst) {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_DRAWABLE,
                    req.dst.0,
                    u16::from(minor),
                    header.opcode,
                );
            }
            if dst_picture_window_unviewable(state, req.dst) {
                return Ok(RequestOutcome::Handled);
            }
            let host_dst = state
                .resources
                .picture(req.dst)
                .and_then(|p| p.host_picture_xid)
                .map(|h| h.as_raw());
            if let Some(host_dst) = host_dst {
                // ClipByChildren: a FillRectangles op=Clear on a window
                // fully covered by a mapped child (mate-panel systray
                // socket under its XEMBED icon) is a no-op in Xorg —
                // empty composite clip → nothing painted, no damage. We
                // historically painted + damaged the full drawable, which
                // both wiped the embedded icon's backing AND drove the
                // per-frame tray recomposite loop. Clip the paint rects to
                // window-minus-children; if the result is empty the whole
                // op is skipped. Pixmaps / childless windows pass through
                // unchanged.
                // IncludeInferiors keeps the children in the clip.
                let dst_drawable = state.resources.picture(req.dst).and_then(|p| p.drawable);
                let include_inferiors = backend.picture_includes_inferiors(host_dst);
                let painted_rects = match dst_drawable {
                    Some(d) if !include_inferiors => {
                        clip_fill_rects_by_children(state, d, &req.rects)
                    }
                    _ => req.rects.clone(),
                };
                if !painted_rects.is_empty() {
                    let _ = backend.render_fill_rectangles(
                        origin,
                        host_dst,
                        req.op,
                        req.color,
                        &painted_rects,
                        0,
                        0,
                    );
                    if let Some(d) = dst_drawable {
                        let _dropped = if include_inferiors {
                            accumulate_damage_full_to_state(state, d)
                        } else {
                            accumulate_damage_clip_by_children_to_state(state, d)
                        };
                    }
                }
            }
        }
        27 => {
            if body.len() < 12 {
                return Ok(RequestOutcome::Handled);
            }
            let cursor_id = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
            let src_pic_id = ResourceId(u32::from_le_bytes([body[4], body[5], body[6], body[7]]));
            verify_pictures!(src_pic_id);
            let x = u16::from_le_bytes([body[8], body[9]]);
            let y = u16::from_le_bytes([body[10], body[11]]);
            let host_src = state
                .resources
                .picture(src_pic_id)
                .and_then(|p| p.host_picture_xid);
            if let Some(host_src) = host_src
                && let Some(cursor_handle) = backend
                    .render_create_cursor(origin, host_src, x, y)
                    .ok()
                    .flatten()
            {
                state.resources.create_glyph_cursor(client_id, cursor_id);
                state
                    .resources
                    .set_cursor_host_xid(cursor_id, cursor_handle);
            }
        }
        28 => {
            if body.len() < 40 {
                return Ok(RequestOutcome::Handled);
            }
            let pic_id = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
            verify_pictures!(pic_id);
            let host_pic = state
                .resources
                .picture(pic_id)
                .and_then(|p| p.host_picture_xid)
                .map(|h| h.as_raw());
            if let Some(hp) = host_pic {
                let _ = backend.render_set_picture_transform(origin, hp, body);
            }
        }
        29 => {
            let mut buf: Vec<u8> = Vec::with_capacity(64);
            x11::write_render_query_filters_reply(&mut buf, byte_order, sequence)?;
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            let _byte_order = client.byte_order;
            return Ok(write_to_client(client, client_id, &buf));
        }
        30 => {
            if body.len() < 8 {
                return Ok(RequestOutcome::Handled);
            }
            let pic_id = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
            verify_pictures!(pic_id);
            let host_pic = state
                .resources
                .picture(pic_id)
                .and_then(|p| p.host_picture_xid)
                .map(|h| h.as_raw());
            if let Some(hp) = host_pic {
                let _ = backend.render_set_picture_filter(origin, hp, body);
            }
        }
        33 => {
            let Some((pic_id, color)) = x11::render_create_solid_fill_request(body) else {
                return Ok(RequestOutcome::Handled);
            };
            let host_pic = backend
                .render_create_solid_fill(origin, color)
                .ok()
                .flatten();
            state.resources.create_picture(
                pic_id,
                PictureState {
                    client: client_id,
                    host_picture_xid: host_pic,
                    host_owned_pixmap: None,
                    kind: crate::resources::PictureKind::Sourceless,
                    drawable: None,
                    window: None,
                },
            );
        }
        34 => {
            if body.len() < 24 {
                return Ok(RequestOutcome::Handled);
            }
            let pic_id = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
            let host_pic = backend
                .render_create_linear_gradient(origin, body)
                .ok()
                .flatten();
            state.resources.create_picture(
                pic_id,
                PictureState {
                    client: client_id,
                    host_picture_xid: host_pic,
                    host_owned_pixmap: None,
                    kind: crate::resources::PictureKind::Sourceless,
                    drawable: None,
                    window: None,
                },
            );
        }
        35 => {
            if body.len() < 32 {
                return Ok(RequestOutcome::Handled);
            }
            let pic_id = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
            let host_pic = backend
                .render_create_radial_gradient(origin, body)
                .ok()
                .flatten();
            state.resources.create_picture(
                pic_id,
                PictureState {
                    client: client_id,
                    host_picture_xid: host_pic,
                    host_owned_pixmap: None,
                    kind: crate::resources::PictureKind::Sourceless,
                    drawable: None,
                    window: None,
                },
            );
        }
        31 => {
            // RENDER::CreateAnimCursor — body: cid(4), [cursor(4),
            // delay(4)]*N. Port of Xorg
            // `render/render.c::ProcRenderCreateAnimCursor:1783`:
            // validate cid is a fresh client-range xid, validate every
            // sub-cursor exists, register the cid as a real Cursor
            // resource so downstream CWA(CWCursor=this) /
            // XFixesSetCursorName / DefineCursor see a live xid.
            //
            // Validation: odd pair bytes → BadLength; zero frames →
            // BadValue; nested animated sub-cursor → BadMatch (Xorg
            // animcur.c:316).
            //
            // Animation is delegated to `backend.create_anim_cursor`;
            // backends returning `Ok(None)` (default / ynest) degenerate
            // to frame 0's handle. Previously this arm was a stub that
            // silently dropped the xid — combined with the BadCursor gate
            // in handle_change_window_attributes, that wedged caja /
            // thunar on every app launch via marco's busy-cursor path.
            if body.len() < 4 {
                return Ok(RequestOutcome::Handled);
            }
            let cursor_id = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
            let validation_failed = {
                let handle = state.clients.get(&client_id.0).expect("client registered");
                let owned = crate::server::IdAllocator::validate_owned(
                    cursor_id.0,
                    handle.resource_id_base,
                    handle.resource_id_mask,
                );
                !owned || state.resources.xid_in_use(cursor_id)
            };
            if validation_failed {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_ID_CHOICE,
                    cursor_id.0,
                    u16::from(minor),
                    header.opcode,
                );
            }
            let pairs = &body[4..];
            // Xorg fidelity (render.c:1796,1801): odd request length
            // → BadLength; zero frames → BadValue.
            if !pairs.len().is_multiple_of(8) {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_LENGTH,
                    0,
                    u16::from(minor),
                    header.opcode,
                );
            }
            if pairs.is_empty() {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    0,
                    u16::from(minor),
                    header.opcode,
                );
            }
            let mut first_host: Option<u32> = None;
            let mut frames: Vec<(crate::backend::CursorHandle, u32)> =
                Vec::with_capacity(pairs.len() / 8);
            for chunk in pairs.chunks_exact(8) {
                let sub = ResourceId(u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
                let delay = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
                if !state.resources.cursor_exists(sub) {
                    return emit_x11_error_with_minor(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_CURSOR,
                        sub.0,
                        u16::from(minor),
                        header.opcode,
                    );
                }
                // Xorg refuses nested animated cursors (animcur.c:316).
                // bad_value stays 0: AnimCursorCreate returns BadMatch
                // without setting client->errorValue.
                if state.resources.cursor_is_anim(sub) {
                    return emit_x11_error_with_minor(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_MATCH,
                        0,
                        u16::from(minor),
                        header.opcode,
                    );
                }
                if let Some(host_raw) = state.resources.cursor_host_xid(sub) {
                    if first_host.is_none() {
                        first_host = Some(host_raw);
                    }
                    if let Some(h) = crate::backend::CursorHandle::from_raw(host_raw) {
                        frames.push((h, delay));
                    }
                }
            }
            // Backend-side animation. `Ok(None)` (default impl /
            // ynest) → static degeneration to frame 0's handle. A
            // backend Err is "can't happen" after the validation
            // above — log and degenerate rather than swallowing
            // silently (spec "Error handling").
            let anim_handle = if frames.is_empty() {
                log::warn!(
                    "client {} RENDER::CreateAnimCursor: no sub-cursor has a host handle; \
                     cursor 0x{:x} registered without backend animation",
                    client_id.0,
                    cursor_id.0,
                );
                None
            } else {
                match backend.create_anim_cursor(origin, &frames) {
                    Ok(handle) => handle,
                    Err(e) => {
                        log::warn!(
                            "client {} RENDER::CreateAnimCursor backend failure ({e}); \
                             degenerating to first frame",
                            client_id.0,
                        );
                        None
                    }
                }
            };
            state.resources.create_glyph_cursor(client_id, cursor_id);
            state.resources.set_cursor_anim(cursor_id);
            if let Some(handle) = anim_handle {
                state.resources.set_cursor_host_xid(cursor_id, handle);
                log::debug!(
                    "client {} RENDER::CreateAnimCursor cursor=0x{:x} animated \
                     ({} frames, backend handle 0x{:x})",
                    client_id.0,
                    cursor_id.0,
                    frames.len(),
                    handle.as_raw(),
                );
            } else if let Some(host_raw) = first_host
                && let Some(handle) = crate::backend::CursorHandle::from_raw(host_raw)
            {
                state.resources.set_cursor_host_xid(cursor_id, handle);
                log::debug!(
                    "client {} RENDER::CreateAnimCursor cursor=0x{:x} (static \
                     degeneration to first sub-cursor host_xid=0x{host_raw:x})",
                    client_id.0,
                    cursor_id.0,
                );
            }
        }
        36 => {
            // CreateConicalGradient: registered so the client can name it; rendering with it
            // is not implemented, so ops that use it stay no-ops.
            if body.len() < 4 {
                return Ok(RequestOutcome::Handled);
            }
            let pic_id = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
            state.resources.create_picture(
                pic_id,
                PictureState {
                    client: client_id,
                    host_picture_xid: None,
                    host_owned_pixmap: None,
                    kind: crate::resources::PictureKind::Sourceless,
                    drawable: None,
                    window: None,
                },
            );
        }
        32 => {
            // AddTraps — backend dispatch is a stub, but match the
            // damage pattern of the other paint vectors so a future
            // real impl doesn't silently miss compositor wakeups.
            // Wasted compositor recomposite on a no-op paint is
            // harmless; the alternative (no damage call) is a
            // silent gap on a real impl. Body: pic(4) x_off(2)
            // y_off(2) then variable trapezoid list.
            if body.len() >= 4 {
                let pic = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
                verify_pictures!(pic);
                if let Some(dst_drawable) = state.resources.picture(pic).and_then(|p| p.drawable) {
                    let _dropped = accumulate_damage_full_to_state(state, dst_drawable);
                }
            }
        }
        other if other > RENDER_LAST_REQUEST => {
            return emit_x11_error_with_minor(
                state,
                client_id,
                sequence,
                x11::error::BAD_REQUEST,
                0,
                u16::from(other),
                header.opcode,
            );
        }
        _ => {
            debug!(
                "client {} #{} RENDER::known unsupported minor={}",
                client_id.0, sequence.0, minor
            );
        }
    }
    Ok(RequestOutcome::Handled)
}

/// True if the picture exists and was created from a SolidFill /
/// LinearGradient / RadialGradient / ConicalGradient — i.e. has no
/// underlying drawable, so cannot be a Composite/Trapezoids/Triangles/
/// FillRectangles/CompositeGlyphs destination. Unknown picture ids
/// return false (caller falls through to existing missing-id handling).
fn dst_picture_is_sourceless(state: &ServerState, dst: ResourceId) -> bool {
    state
        .resources
        .picture(dst)
        .is_some_and(|p| matches!(p.kind, crate::resources::PictureKind::Sourceless))
}

pub(super) fn render_picture_damage_drawable(
    state: &ServerState,
    drawable: ResourceId,
) -> ResourceId {
    state
        .resources
        .composite_named_pixmap_owner_window(drawable)
        .unwrap_or(drawable)
}

/// Xorg VERIFY_PICTURE (`render/picturestr.h:363`, error value set at
/// `render/render.c:252`): the first of `ids` that names no Picture.
fn first_missing_picture(state: &ServerState, ids: &[ResourceId]) -> Option<ResourceId> {
    ids.iter()
        .copied()
        .find(|id| state.resources.picture(*id).is_none())
}

/// A RENDER destination Picture on an unviewable window: Xorg's composite clip is the
/// window's clipList/borderClip (`render/mipict.c:114-118`), emptied when it stops being
/// viewable (`mi/mivaltree.c:690-695`, `mi/miwindow.c:738-745`), so nothing is drawn.
fn dst_picture_window_unviewable(state: &ServerState, pic: ResourceId) -> bool {
    state
        .resources
        .picture(pic)
        .and_then(|p| p.window)
        .is_some_and(|w| window_unviewable(state, w))
}
