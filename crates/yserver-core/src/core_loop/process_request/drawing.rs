use super::*;

fn clear_extent(requested: u16, offset: i16, window_extent: u16) -> u16 {
    if requested != 0 {
        return requested;
    }
    if offset <= 0 {
        window_extent
    } else {
        window_extent.saturating_sub(offset as u16)
    }
}

pub(super) fn handle_put_image(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    let Some(request) = x11::put_image_request(header.data, body) else {
        return Ok(RequestOutcome::Handled);
    };
    if request.width == 0 || request.height == 0 {
        return Ok(RequestOutcome::Handled);
    }
    debug!(
        "client {} #{} PutImage drawable=0x{:x} {}x{} dst=({},{}) depth={} fmt={:?}",
        client_id.0,
        sequence.0,
        request.drawable.0,
        request.width,
        request.height,
        request.dst_x,
        request.dst_y,
        request.depth,
        request.format,
    );
    if let Err((code, bad_value)) = validate_drawable_and_gc(state, request.drawable, request.gc) {
        return emit_x11_error(state, client_id, sequence, code, bad_value, 72);
    }
    let draw_state = state.resources.resolve_draw_state(request.gc);
    let target = state.resources.host_drawable_target(request.drawable);
    if let Some(target) = target {
        let st = draw_state.unwrap_or_default();
        let (image_bytes, upload_depth) = match request.format {
            x11::ImageFormat::ZPixmap => {
                if request.left_pad != 0 || request.depth != target.depth() {
                    return Ok(RequestOutcome::Handled);
                }
                let Some(expected_len) =
                    zpixmap_expected_len(request.width, request.height, request.depth)
                else {
                    return Ok(RequestOutcome::Handled);
                };
                if request.data.len() < expected_len {
                    return Ok(RequestOutcome::Handled);
                }
                (request.data[..expected_len].to_vec(), request.depth)
            }
            x11::ImageFormat::XyBitmap => {
                // XYBitmap is a single depth-1 source plane interpreted through
                // the GC foreground/background pixels. This is the path used by
                // XCreateBitmapFromData() and XCreatePixmapFromBitmapData().
                if request.depth != 1 {
                    return Ok(RequestOutcome::Handled);
                }
                let Some(expected_len) =
                    xybitmap_expected_len(request.width, request.height, request.left_pad)
                else {
                    return Ok(RequestOutcome::Handled);
                };
                if request.data.len() < expected_len {
                    return Ok(RequestOutcome::Handled);
                }
                let Some(bytes) = xybitmap_to_target_zpixmap(
                    &request.data[..expected_len],
                    request.width,
                    request.height,
                    request.left_pad,
                    target.depth(),
                    st.foreground,
                    st.background,
                ) else {
                    return Ok(RequestOutcome::Handled);
                };
                (bytes, target.depth())
            }
            x11::ImageFormat::XyPixmap => {
                // Depth-1 XYPixmap is a single bitmap plane and can be
                // normalized through the same repack as XYBitmap. Higher
                // depths need multi-plane composition and remain unsupported
                // here for now.
                if request.depth != 1 || target.depth() != 1 {
                    return Ok(RequestOutcome::Handled);
                }
                let Some(expected_len) =
                    xybitmap_expected_len(request.width, request.height, request.left_pad)
                else {
                    return Ok(RequestOutcome::Handled);
                };
                if request.data.len() < expected_len {
                    return Ok(RequestOutcome::Handled);
                }
                let Some(bytes) = xybitmap_to_zpixmap(
                    &request.data[..expected_len],
                    request.width,
                    request.height,
                    request.left_pad,
                ) else {
                    return Ok(RequestOutcome::Handled);
                };
                (bytes, request.depth)
            }
            x11::ImageFormat::Unknown(_) => {
                return Ok(RequestOutcome::Handled);
            }
        };
        backend.apply_clip_state(origin, &st.clip)?;
        backend.apply_draw_state(origin, &st)?;
        backend.put_image(
            origin,
            target.host_xid(),
            upload_depth,
            request.width,
            request.height,
            request.dst_x,
            request.dst_y,
            &image_bytes,
        )?;
        let _dropped = accumulate_damage_to_state(
            state,
            request.drawable,
            request.dst_x,
            request.dst_y,
            request.width,
            request.height,
        );
    }
    Ok(RequestOutcome::Handled)
}

/// Bytes per scanline for X11 ZPixmap of `width` pixels at `depth`,
/// padded to the server's bitmap_scanline_pad of 32 bits.
pub(super) fn zpixmap_row_stride(width: u16, depth: u8) -> Option<usize> {
    match depth {
        24 | 32 => usize::from(width).checked_mul(4),
        8 => usize::from(width).div_ceil(4).checked_mul(4),
        4 => usize::from(width).div_ceil(8).checked_mul(4),
        1 => usize::from(width).div_ceil(32).checked_mul(4),
        _ => None,
    }
}

fn xybitmap_expected_len(width: u16, height: u16, left_pad: u8) -> Option<usize> {
    let bits_per_row = usize::from(width).checked_add(usize::from(left_pad))?;
    let stride_bytes = bits_per_row.div_ceil(32).checked_mul(4)?;
    stride_bytes.checked_mul(usize::from(height))
}

pub(super) fn xybitmap_to_zpixmap(
    data: &[u8],
    width: u16,
    height: u16,
    left_pad: u8,
) -> Option<Vec<u8>> {
    let bits_per_row = usize::from(width).checked_add(usize::from(left_pad))?;
    let src_row_stride = bits_per_row.div_ceil(32).checked_mul(4)?;
    if data.len() < src_row_stride.checked_mul(usize::from(height))? {
        return None;
    }
    let dst_row_stride = zpixmap_row_stride(width, 1)?;
    let mut out = vec![0u8; dst_row_stride.checked_mul(usize::from(height))?];
    let left_pad = usize::from(left_pad);
    let width = usize::from(width);
    for row in 0..usize::from(height) {
        let src_row = &data[row * src_row_stride..(row + 1) * src_row_stride];
        let dst_row = &mut out[row * dst_row_stride..(row + 1) * dst_row_stride];
        for x in 0..width {
            let src_bit = left_pad + x;
            let src_byte = src_row[src_bit / 8];
            if (src_byte >> (src_bit % 8)) & 1 != 0 {
                dst_row[x / 8] |= 1 << (x % 8);
            }
        }
    }
    Some(out)
}

fn depth_plane_mask(depth: u8) -> u32 {
    if depth >= 32 {
        u32::MAX
    } else {
        (1u32 << depth) - 1
    }
}

fn write_zpixmap_pixel(bytes: &mut [u8], width: usize, depth: u8, x: usize, y: usize, value: u32) {
    let stride = zpixmap_row_stride(width as u16, depth).expect("validated target depth");
    match depth {
        1 => {
            let byte = &mut bytes[y * stride + x / 8];
            let bit = 1u8 << (x % 8);
            if value & 1 != 0 {
                *byte |= bit;
            } else {
                *byte &= !bit;
            }
        }
        4 => {
            let byte = &mut bytes[y * stride + x / 2];
            let nibble = (value & 0x0f) as u8;
            if x.is_multiple_of(2) {
                *byte = (*byte & 0xf0) | nibble;
            } else {
                *byte = (*byte & 0x0f) | (nibble << 4);
            }
        }
        8 => bytes[y * stride + x] = value as u8,
        24 | 32 => {
            let off = y * stride + x * 4;
            bytes[off..off + 4].copy_from_slice(&value.to_le_bytes());
        }
        _ => {}
    }
}

pub(super) fn xybitmap_to_target_zpixmap(
    data: &[u8],
    width: u16,
    height: u16,
    left_pad: u8,
    target_depth: u8,
    fg: u32,
    bg: u32,
) -> Option<Vec<u8>> {
    let bits_per_row = usize::from(width).checked_add(usize::from(left_pad))?;
    let src_row_stride = bits_per_row.div_ceil(32).checked_mul(4)?;
    if data.len() < src_row_stride.checked_mul(usize::from(height))? {
        return None;
    }
    let dst_row_stride = zpixmap_row_stride(width, target_depth)?;
    let mut out = vec![0u8; dst_row_stride.checked_mul(usize::from(height))?];
    let left_pad = usize::from(left_pad);
    let width = usize::from(width);
    let fg = fg & depth_plane_mask(target_depth);
    let bg = bg & depth_plane_mask(target_depth);
    for row in 0..usize::from(height) {
        let src_row = &data[row * src_row_stride..(row + 1) * src_row_stride];
        for x in 0..width {
            let src_bit = left_pad + x;
            let src_byte = src_row[src_bit / 8];
            let pixel = if (src_byte >> (src_bit % 8)) & 1 != 0 {
                fg
            } else {
                bg
            };
            write_zpixmap_pixel(&mut out, width, target_depth, x, row, pixel);
        }
    }
    Some(out)
}

pub(super) fn handle_image_text8(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    if let Some((drawable_raw, gc_id, text_body)) = x11::image_text8_data(body) {
        let drawable = ResourceId(drawable_raw);
        if let Err((code, bad_value)) = validate_drawable_and_gc(state, drawable, ResourceId(gc_id))
        {
            return emit_x11_error(state, client_id, sequence, code, bad_value, 76);
        }
        let draw_state = state.resources.resolve_draw_state(ResourceId(gc_id));
        let target = state.resources.host_drawable_target(drawable);
        if let Some(target) = target {
            let st = draw_state.unwrap_or_default();
            backend.apply_clip_state(origin, &st.clip)?;
            backend.apply_draw_state(origin, &st)?;
            backend.image_text8(
                origin,
                target.host_xid(),
                st.foreground,
                st.background,
                header.data,
                text_body,
            )?;
        }
        let _dropped = accumulate_damage_full_to_state(state, drawable);
    }
    debug!("client {} #{} ImageText8", client_id.0, sequence.0);
    Ok(RequestOutcome::Handled)
}

pub(super) fn handle_image_text16(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    if let Some((drawable_raw, gc_id, text_body)) = x11::image_text8_data(body) {
        let drawable = ResourceId(drawable_raw);
        if let Err((code, bad_value)) = validate_drawable_and_gc(state, drawable, ResourceId(gc_id))
        {
            return emit_x11_error(state, client_id, sequence, code, bad_value, 77);
        }
        let draw_state = state.resources.resolve_draw_state(ResourceId(gc_id));
        let target = state.resources.host_drawable_target(drawable);
        if let Some(target) = target {
            let st = draw_state.unwrap_or_default();
            backend.apply_clip_state(origin, &st.clip)?;
            backend.apply_draw_state(origin, &st)?;
            backend.image_text16(
                origin,
                target.host_xid(),
                st.foreground,
                st.background,
                header.data,
                text_body,
            )?;
        }
        let _dropped = accumulate_damage_full_to_state(state, drawable);
    }
    debug!("client {} #{} ImageText16", client_id.0, sequence.0);
    Ok(RequestOutcome::Handled)
}

pub(super) fn handle_get_image(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    debug!("client {} #{} GetImage", client_id.0, sequence.0);
    let Some(req) = x11::get_image_request(header.data, body) else {
        return Ok(RequestOutcome::Handled);
    };
    // Validation per Xorg ProcGetImage: format ∈ {XYPixmap, ZPixmap}
    // → BadValue; drawable must exist → BadDrawable; the rect must
    // lie fully within the drawable (windows additionally must be
    // viewable) → BadMatch.
    if req.format != 1 && req.format != 2 {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_VALUE,
            u32::from(req.format),
            73,
        );
    }
    let rx0 = i32::from(req.x);
    let ry0 = i32::from(req.y);
    let rx1 = rx0 + i32::from(req.width);
    let ry1 = ry0 + i32::from(req.height);
    if let Some(w) = state.resources.window(req.drawable) {
        // Windows: must be viewable; the rect may include the BORDER
        // (xts XGetImage-7 reads (-1,-1)) but not extend past its
        // outside edges; and — ignoring overlaps — the rect must be
        // fully on the screen (XGetImage-15). Both → BadMatch.
        let bw = i32::from(w.border_width);
        let (ww, wh) = (i32::from(w.width), i32::from(w.height));
        let viewable = w.map_state == crate::resources::MapState::Viewable;
        let in_window = rx0 >= -bw && ry0 >= -bw && rx1 <= ww + bw && ry1 <= wh + bw;
        let (abs_x, abs_y) = state.resources.window_absolute_position(req.drawable);
        let (root_w, root_h) = state
            .resources
            .window(crate::resources::ROOT_WINDOW)
            .map_or((i32::MAX, i32::MAX), |r| {
                (i32::from(r.width), i32::from(r.height))
            });
        // The bound is the pixmap the window draws into: the screen, or
        // the backing of the nearest redirected window at or above it,
        // which a window partly off the screen still has whole
        // (`DoGetImage`, `dix/dispatch.c:2176-2210`).
        let (bx, by, bwidth, bheight) = match redirected_ancestor_or_self(state, req.drawable)
            .and_then(|r| state.resources.window(r).map(|w| (r, w)))
        {
            Some((r, rw)) => {
                let (rx, ry) = state.resources.window_absolute_position(r);
                let rbw = i32::from(rw.border_width);
                (
                    rx - rbw,
                    ry - rbw,
                    i32::from(rw.width) + 2 * rbw,
                    i32::from(rw.height) + 2 * rbw,
                )
            }
            None => (0, 0, root_w, root_h),
        };
        let on_screen = abs_x + rx0 >= bx
            && abs_y + ry0 >= by
            && abs_x + rx1 <= bx + bwidth
            && abs_y + ry1 <= by + bheight;
        if !viewable || !in_window || !on_screen {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_MATCH,
                req.drawable.0,
                73,
            );
        }
    } else if let Some(p) = state.resources.pixmap(req.drawable) {
        if rx0 < 0 || ry0 < 0 || rx1 > i32::from(p.width) || ry1 > i32::from(p.height) {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_MATCH,
                req.drawable.0,
                73,
            );
        }
    } else {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_DRAWABLE,
            req.drawable.0,
            73,
        );
    }
    let byte_order = state
        .clients
        .get(&client_id.0)
        .map_or(yserver_protocol::x11::ClientByteOrder::LittleEndian, |c| {
            c.byte_order
        });
    let host_xid = if req.drawable == ROOT_WINDOW {
        Some(backend.window_id())
    } else {
        state
            .resources
            .host_drawable_target(req.drawable)
            .map(|t| t.host_xid())
    };
    let host_reply = host_xid.and_then(|xid| {
        backend
            .get_image(
                origin,
                xid,
                req.format,
                req.x,
                req.y,
                req.width.max(1),
                req.height.max(1),
                req.plane_mask,
            )
            .ok()
            .flatten()
    });
    let buf: Vec<u8> = if let Some(bytes) = host_reply {
        patch_get_image_reply_header(bytes, byte_order, sequence, crate::resources::ROOT_VISUAL.0)
    } else {
        let mut buf: Vec<u8> = Vec::with_capacity(64);
        x11::write_get_image_reply(
            &mut buf,
            byte_order,
            sequence,
            &req,
            crate::resources::ROOT_VISUAL.0,
        )?;
        buf
    };
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let _byte_order = client.byte_order;
    Ok(write_to_client(client, client_id, &buf))
}

pub(super) fn patch_get_image_reply_header(
    mut bytes: Vec<u8>,
    byte_order: x11::ClientByteOrder,
    sequence: SequenceNumber,
    visual: u32,
) -> Vec<u8> {
    if bytes.len() < 32 {
        return bytes;
    }
    let seq = match byte_order {
        x11::ClientByteOrder::LittleEndian => sequence.0.to_le_bytes(),
        x11::ClientByteOrder::BigEndian => sequence.0.to_be_bytes(),
    };
    let len_words = u32::try_from((bytes.len() - 32) / 4).unwrap_or(u32::MAX);
    let len = match byte_order {
        x11::ClientByteOrder::LittleEndian => len_words.to_le_bytes(),
        x11::ClientByteOrder::BigEndian => len_words.to_be_bytes(),
    };
    let vis = match byte_order {
        x11::ClientByteOrder::LittleEndian => visual.to_le_bytes(),
        x11::ClientByteOrder::BigEndian => visual.to_be_bytes(),
    };
    bytes[2..4].copy_from_slice(&seq);
    bytes[4..8].copy_from_slice(&len);
    bytes[8..12].copy_from_slice(&vis);
    bytes
}

/// Rewrite PolyText8/16 embedded font-change items (len == 255):
/// the wire carries the CLIENT font XID, the backend's font table is
/// keyed by HOST xids. Returns the translated body plus the last
/// font's client id (committed to the GC per X11 §8), or
/// `Err(client_xid)` when an id doesn't resolve (→ BadFont).
fn translate_poly_text_fonts(
    state: &ServerState,
    body: &[u8],
    two_byte: bool,
) -> Result<(Vec<u8>, Option<ResourceId>), u32> {
    let mut out = body.to_vec();
    let mut last: Option<ResourceId> = None;
    let mut off = 12usize;
    while off + 2 <= out.len() {
        let len = out[off];
        if len == 255 {
            if off + 5 > out.len() {
                break;
            }
            let client_xid =
                u32::from_be_bytes([out[off + 1], out[off + 2], out[off + 3], out[off + 4]]);
            let Some(font) = state.resources.font(ResourceId(client_xid)) else {
                return Err(client_xid);
            };
            out[off + 1..off + 5].copy_from_slice(&font.host_xid.as_raw().to_be_bytes());
            last = Some(ResourceId(client_xid));
            off += 5;
        } else {
            let n = usize::from(len);
            off += 2 + if two_byte { 2 * n } else { n };
        }
    }
    Ok((out, last))
}

pub(super) fn handle_poly_text8(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    if let Some((drawable_raw, gc_id, text_body)) = x11::poly_text_data(body) {
        let drawable = ResourceId(drawable_raw);
        if let Err((code, bad_value)) = validate_drawable_and_gc(state, drawable, ResourceId(gc_id))
        {
            return emit_x11_error(state, client_id, sequence, code, bad_value, 74);
        }
        let (text_body, last_font) = match translate_poly_text_fonts(state, text_body, false) {
            Ok(pair) => pair,
            Err(bad_font) => {
                return emit_x11_error(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_FONT,
                    bad_font,
                    74,
                );
            }
        };
        let draw_state = state.resources.resolve_draw_state(ResourceId(gc_id));
        let target = state.resources.host_drawable_target(drawable);
        if let Some(target) = target {
            let st = draw_state.unwrap_or_default();
            backend.apply_clip_state(origin, &st.clip)?;
            backend.apply_draw_state(origin, &st)?;
            backend.poly_text8(origin, target.host_xid(), st.foreground, &text_body)?;
        }
        if let Some(font) = last_font {
            state.resources.set_gc_font(ResourceId(gc_id), font);
        }
        let _dropped = accumulate_damage_full_to_state(state, drawable);
    }
    debug!("client {} #{} PolyText8", client_id.0, sequence.0);
    Ok(RequestOutcome::Handled)
}

pub(super) fn handle_poly_text16(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    if let Some((drawable_raw, gc_id, text_body)) = x11::poly_text_data(body) {
        let drawable = ResourceId(drawable_raw);
        if let Err((code, bad_value)) = validate_drawable_and_gc(state, drawable, ResourceId(gc_id))
        {
            return emit_x11_error(state, client_id, sequence, code, bad_value, 75);
        }
        let (text_body, last_font) = match translate_poly_text_fonts(state, text_body, true) {
            Ok(pair) => pair,
            Err(bad_font) => {
                return emit_x11_error(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_FONT,
                    bad_font,
                    75,
                );
            }
        };
        let draw_state = state.resources.resolve_draw_state(ResourceId(gc_id));
        let target = state.resources.host_drawable_target(drawable);
        if let Some(target) = target {
            let st = draw_state.unwrap_or_default();
            backend.apply_clip_state(origin, &st.clip)?;
            backend.apply_draw_state(origin, &st)?;
            backend.poly_text16(origin, target.host_xid(), st.foreground, &text_body)?;
        }
        if let Some(font) = last_font {
            state.resources.set_gc_font(ResourceId(gc_id), font);
        }
        let _dropped = accumulate_damage_full_to_state(state, drawable);
    }
    debug!("client {} #{} PolyText16", client_id.0, sequence.0);
    Ok(RequestOutcome::Handled)
}

pub(super) fn handle_poly_point(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    if body.len() >= 8 {
        let drawable = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
        let gc_id = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
        let points = &body[8..];
        if let Err((code, bad_value)) = validate_drawable_and_gc(state, drawable, ResourceId(gc_id))
        {
            return emit_x11_error(state, client_id, sequence, code, bad_value, 64);
        }
        let draw_state = state.resources.resolve_draw_state(ResourceId(gc_id));
        let target = state.resources.host_drawable_target(drawable);
        if let Some(target) = target {
            let st = draw_state.unwrap_or_default();
            if let Err(err) = (|| -> io::Result<()> {
                backend.apply_clip_state(origin, &st.clip)?;
                backend.apply_draw_state(origin, &st)?;
                backend.poly_point(
                    origin,
                    target.host_xid(),
                    st.foreground,
                    header.data,
                    points,
                )
            })() {
                log::warn!(
                    "client {} #{} PolyPoint backend error: {err}",
                    client_id.0,
                    sequence.0
                );
            }
        }
        let _dropped = accumulate_damage_full_to_state(state, drawable);
    }
    debug!(
        "client {} #{} PolyPoint drawable=0x{:x}",
        client_id.0,
        sequence.0,
        x11::drawable_request_id(body).map_or(0, |d| d.0),
    );
    Ok(RequestOutcome::Handled)
}

pub(super) fn handle_poly_line(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    if let Some((gc_id, points)) = x11::poly_line_data(body)
        && let Some(drawable) = x11::drawable_request_id(body)
    {
        if let Err((code, bad_value)) = validate_drawable_and_gc(state, drawable, ResourceId(gc_id))
        {
            return emit_x11_error(state, client_id, sequence, code, bad_value, 65);
        }
        let draw_state = state.resources.resolve_draw_state(ResourceId(gc_id));
        let target = state.resources.host_drawable_target(drawable);
        if let Some(target) = target {
            let st = draw_state.unwrap_or_default();
            backend.apply_clip_state(origin, &st.clip)?;
            backend.apply_draw_state(origin, &st)?;
            let _ = backend.poly_line(
                origin,
                target.host_xid(),
                st.foreground,
                header.data,
                points,
            );
        }
        let _dropped = accumulate_damage_full_to_state(state, drawable);
    }
    debug!(
        "client {} #{} PolyLine drawable=0x{:x}",
        client_id.0,
        sequence.0,
        x11::drawable_request_id(body).map_or(0, |d| d.0),
    );
    Ok(RequestOutcome::Handled)
}

pub(super) fn handle_poly_segment(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    if let Some((gc_id, segments)) = x11::poly_segment_data(body)
        && let Some(drawable) = x11::drawable_request_id(body)
    {
        if let Err((code, bad_value)) = validate_drawable_and_gc(state, drawable, ResourceId(gc_id))
        {
            return emit_x11_error(state, client_id, sequence, code, bad_value, 66);
        }
        let draw_state = state.resources.resolve_draw_state(ResourceId(gc_id));
        let target = state.resources.host_drawable_target(drawable);
        if let Some(target) = target {
            let st = draw_state.unwrap_or_default();
            backend.apply_clip_state(origin, &st.clip)?;
            backend.apply_draw_state(origin, &st)?;
            let _ = backend.poly_segment(origin, target.host_xid(), st.foreground, segments);
        }
        let _dropped = accumulate_damage_full_to_state(state, drawable);
    }
    debug!(
        "client {} #{} PolySegment drawable=0x{:x}",
        client_id.0,
        sequence.0,
        x11::drawable_request_id(body).map_or(0, |d| d.0),
    );
    Ok(RequestOutcome::Handled)
}

pub(super) fn handle_poly_rectangle(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    if let Some((gc_id, rectangles)) = x11::poly_fill_rectangle_data(body)
        && let Some(drawable) = x11::drawable_request_id(body)
    {
        if let Err((code, bad_value)) = validate_drawable_and_gc(state, drawable, ResourceId(gc_id))
        {
            return emit_x11_error(state, client_id, sequence, code, bad_value, 67);
        }
        let draw_state = state.resources.resolve_draw_state(ResourceId(gc_id));
        let target = state.resources.host_drawable_target(drawable);
        if let Some(target) = target {
            let st = draw_state.unwrap_or_default();
            backend.apply_clip_state(origin, &st.clip)?;
            backend.apply_draw_state(origin, &st)?;
            let _ = backend.poly_rectangle(origin, target.host_xid(), st.foreground, rectangles);
        }
        let _dropped = accumulate_damage_full_to_state(state, drawable);
    }
    debug!(
        "client {} #{} PolyRectangle drawable=0x{:x}",
        client_id.0,
        sequence.0,
        x11::drawable_request_id(body).map_or(0, |d| d.0),
    );
    Ok(RequestOutcome::Handled)
}

pub(super) fn handle_poly_arc(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    if let Some((gc_id, arcs)) = x11::poly_arc_data(body)
        && let Some(drawable) = x11::drawable_request_id(body)
    {
        if let Err((code, bad_value)) = validate_drawable_and_gc(state, drawable, ResourceId(gc_id))
        {
            return emit_x11_error(state, client_id, sequence, code, bad_value, 68);
        }
        let draw_state = state.resources.resolve_draw_state(ResourceId(gc_id));
        let target = state.resources.host_drawable_target(drawable);
        if let Some(target) = target {
            let st = draw_state.unwrap_or_default();
            backend.apply_clip_state(origin, &st.clip)?;
            backend.apply_draw_state(origin, &st)?;
            let _ = backend.poly_arc(origin, target.host_xid(), st.foreground, arcs);
        }
        let _dropped = accumulate_damage_full_to_state(state, drawable);
    }
    debug!(
        "client {} #{} PolyArc drawable=0x{:x}",
        client_id.0,
        sequence.0,
        x11::drawable_request_id(body).map_or(0, |d| d.0),
    );
    Ok(RequestOutcome::Handled)
}

pub(super) fn handle_fill_poly(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    if body.len() >= 12 {
        let drawable = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
        let gc_id = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
        let coord_mode = body[9];
        let points = &body[12..];
        if let Err((code, bad_value)) = validate_drawable_and_gc(state, drawable, ResourceId(gc_id))
        {
            return emit_x11_error(state, client_id, sequence, code, bad_value, 69);
        }
        let draw_state = state.resources.resolve_draw_state(ResourceId(gc_id));
        let target = state.resources.host_drawable_target(drawable);
        if let Some(target) = target {
            let st = draw_state.unwrap_or_default();
            let needs_fill_reset = !matches!(st.fill, FillState::Solid);
            backend.apply_clip_state(origin, &st.clip)?;
            backend.apply_fill_state(origin, &st.fill)?;
            backend.apply_draw_state(origin, &st)?;
            let _ = backend.fill_poly(origin, target.host_xid(), st.foreground, coord_mode, points);
            if needs_fill_reset {
                let _ = backend.set_gc_fill_solid(origin);
            }
        }
        let _dropped = accumulate_damage_full_to_state(state, drawable);
    }
    debug!(
        "client {} #{} FillPoly drawable=0x{:x}",
        client_id.0,
        sequence.0,
        x11::drawable_request_id(body).map_or(0, |d| d.0),
    );
    Ok(RequestOutcome::Handled)
}

pub(super) fn handle_poly_fill_rectangle(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    if let Some((gc_id, rectangles)) = x11::poly_fill_rectangle_data(body)
        && let Some(drawable) = x11::drawable_request_id(body)
    {
        if let Err((code, bad_value)) = validate_drawable_and_gc(state, drawable, ResourceId(gc_id))
        {
            return emit_x11_error(state, client_id, sequence, code, bad_value, 70);
        }
        let draw_state = state.resources.resolve_draw_state(ResourceId(gc_id));
        let target = state.resources.host_drawable_target(drawable);
        if let Some(target) = target {
            let st = draw_state.unwrap_or_default();
            let needs_fill_reset = !matches!(st.fill, FillState::Solid);
            backend.apply_clip_state(origin, &st.clip)?;
            backend.apply_fill_state(origin, &st.fill)?;
            backend.apply_draw_state(origin, &st)?;
            let _ =
                backend.poly_fill_rectangle(origin, target.host_xid(), st.foreground, rectangles);
            if needs_fill_reset {
                let _ = backend.set_gc_fill_solid(origin);
            }
        }
        let _dropped = accumulate_damage_full_to_state(state, drawable);
    }
    debug!(
        "client {} #{} PolyFillRectangle drawable=0x{:x}",
        client_id.0,
        sequence.0,
        x11::drawable_request_id(body).map_or(0, |d| d.0),
    );
    Ok(RequestOutcome::Handled)
}

pub(super) fn handle_poly_fill_arc(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    if let Some((gc_id, arcs)) = x11::poly_fill_arc_data(body)
        && let Some(drawable) = x11::drawable_request_id(body)
    {
        if let Err((code, bad_value)) = validate_drawable_and_gc(state, drawable, ResourceId(gc_id))
        {
            return emit_x11_error(state, client_id, sequence, code, bad_value, 71);
        }
        let draw_state = state.resources.resolve_draw_state(ResourceId(gc_id));
        let target = state.resources.host_drawable_target(drawable);
        if let Some(target) = target {
            let st = draw_state.unwrap_or_default();
            let needs_fill_reset = !matches!(st.fill, FillState::Solid);
            backend.apply_clip_state(origin, &st.clip)?;
            backend.apply_fill_state(origin, &st.fill)?;
            backend.apply_draw_state(origin, &st)?;
            let _ = backend.poly_fill_arc(origin, target.host_xid(), st.foreground, arcs);
            if needs_fill_reset {
                let _ = backend.set_gc_fill_solid(origin);
            }
        }
        let _dropped = accumulate_damage_full_to_state(state, drawable);
    }
    debug!(
        "client {} #{} PolyFillArc drawable=0x{:x}",
        client_id.0,
        sequence.0,
        x11::drawable_request_id(body).map_or(0, |d| d.0),
    );
    Ok(RequestOutcome::Handled)
}

pub(super) fn handle_clear_area(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    let exposures = header.data != 0;
    if let Some(request) = x11::clear_area_request(body) {
        // Validation per Xorg ProcClearToBackground: exposures must
        // be a BOOL (0/1) → BadValue; the drawable must be a window
        // → BadWindow; InputOnly windows can't be cleared → BadMatch.
        if header.data > 1 {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                u32::from(header.data),
                61,
            );
        }
        let Some(window) = state.resources.window(request.window) else {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_WINDOW,
                request.window.0,
                61,
            );
        };
        if matches!(window.class, crate::resources::WindowClass::InputOnly) {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_MATCH,
                request.window.0,
                61,
            );
        }
        let extents = state
            .resources
            .window(request.window)
            .map(|w| (w.width, w.height));
        let resolved_bg = state.resources.window_resolved_background(request.window);
        let target = state.resources.host_drawable_target(request.window);
        if let Some((w_width, w_height)) = extents
            && let Some(target) = target
        {
            let width = clear_extent(request.width, request.x, w_width);
            let height = clear_extent(request.height, request.y, w_height);
            if width != 0 && height != 0 {
                // Background None (X11 §ClearArea): the contents are
                // left untouched — no paint, no damage. Expose events
                // (below) still fire when requested.
                if let Some(bg) = resolved_bg {
                    backend.clear_area(
                        origin,
                        target.host_xid(),
                        bg.background_pixel,
                        bg.background_pixmap_host_xid.map(|h| h.as_raw()),
                        request.x,
                        request.y,
                        width,
                        height,
                        bg.tile_origin_offset,
                    )?;
                    let _dropped = accumulate_damage_to_state(
                        state,
                        request.window,
                        request.x,
                        request.y,
                        width,
                        height,
                    );
                }
                // X11 spec: when `exposures` is True (header.data),
                // the server sends an Expose event for visible regions
                // of the cleared rectangle.
                if exposures {
                    let req = request;
                    let _dropped = emit_window_event_to_state(
                        state,
                        req.window,
                        0x0000_8000,
                        |buf, seq, order| {
                            x11::encode_expose_event(
                                buf,
                                seq,
                                order,
                                req.window,
                                req.x as u16,
                                req.y as u16,
                                width,
                                height,
                                0,
                            );
                        },
                    );
                }
            }
        }
    }
    debug!(
        "client {} #{} ClearArea drawable=0x{:x}",
        client_id.0,
        sequence.0,
        x11::clear_area_request(body).map_or(0, |r| r.window.0),
    );
    Ok(RequestOutcome::Handled)
}

/// Drawable size in its own coordinate space (windows: interior
/// w×h; pixmaps: w×h). None for unknown resources.
fn drawable_size(state: &ServerState, id: ResourceId) -> Option<(u16, u16)> {
    if let Some(w) = state.resources.window(id) {
        return Some((w.width, w.height));
    }
    state.resources.pixmap(id).map(|p| (p.width, p.height))
}

/// Source-availability split for CopyArea/CopyPlane (X11 §CopyArea;
/// Xorg miHandleExposures): the part of the requested source rect
/// outside the source drawable's bounds is not copied. Returns the
/// available source sub-rect (None when fully outside) and the
/// missing sub-rects translated to DESTINATION coordinates — each
/// becomes one GraphicsExpose; an empty list means NoExposure.
#[allow(clippy::type_complexity)]
pub(super) fn copy_area_source_split(
    state: &ServerState,
    src: ResourceId,
    src_x: i16,
    src_y: i16,
    dst_x: i16,
    dst_y: i16,
    width: u16,
    height: u16,
) -> (
    Option<(i16, i16, i16, i16, u16, u16)>,
    Vec<(i16, i16, u16, u16)>,
) {
    if window_unviewable(state, src) {
        // Xorg: an unrealized window's clipList is empty, so nothing copies (micopy.c:282).
        return (None, vec![(dst_x, dst_y, width, height)]);
    }
    let Some((sw, sh)) = drawable_size(state, src) else {
        // Unknown source geometry: keep the old conservative
        // behavior (copy as requested, no missing region).
        return (
            Some((src_x, src_y, dst_x, dst_y, width, height)),
            Vec::new(),
        );
    };
    let rx0 = i32::from(src_x);
    let ry0 = i32::from(src_y);
    let rx1 = rx0 + i32::from(width);
    let ry1 = ry0 + i32::from(height);
    let ax0 = rx0.max(0);
    let ay0 = ry0.max(0);
    let ax1 = rx1.min(i32::from(sw));
    let ay1 = ry1.min(i32::from(sh));
    // Translate a source-coordinate sub-rect into dst coordinates.
    let to_dst = |x0: i32, y0: i32, x1: i32, y1: i32| -> (i16, i16, u16, u16) {
        let dx = i32::from(dst_x) + (x0 - rx0);
        let dy = i32::from(dst_y) + (y0 - ry0);
        (
            i16::try_from(dx).unwrap_or(i16::MAX),
            i16::try_from(dy).unwrap_or(i16::MAX),
            u16::try_from(x1 - x0).unwrap_or(0),
            u16::try_from(y1 - y0).unwrap_or(0),
        )
    };
    if ax1 <= ax0 || ay1 <= ay0 {
        // Fully outside: nothing copies, the whole rect is exposed.
        return (None, vec![to_dst(rx0, ry0, rx1, ry1)]);
    }
    let mut missing: Vec<(i16, i16, u16, u16)> = Vec::new();
    // Band decomposition of requested − available: top, bottom,
    // left-of-middle, right-of-middle.
    if ay0 > ry0 {
        missing.push(to_dst(rx0, ry0, rx1, ay0));
    }
    if ay1 < ry1 {
        missing.push(to_dst(rx0, ay1, rx1, ry1));
    }
    if ax0 > rx0 {
        missing.push(to_dst(rx0, ay0, ax0, ay1));
    }
    if ax1 < rx1 {
        missing.push(to_dst(ax1, ay0, rx1, ay1));
    }
    let avail = (
        i16::try_from(ax0).unwrap_or(0),
        i16::try_from(ay0).unwrap_or(0),
        i16::try_from(i32::from(dst_x) + (ax0 - rx0)).unwrap_or(i16::MAX),
        i16::try_from(i32::from(dst_y) + (ay0 - ry0)).unwrap_or(i16::MAX),
        u16::try_from(ax1 - ax0).unwrap_or(0),
        u16::try_from(ay1 - ay0).unwrap_or(0),
    );
    (Some(avail), missing)
}

/// The region a CopyArea / CopyPlane exposes on its destination, in its
/// coordinates: Xorg's `miHandleExposures` (`mi/miexpose.c:120-305`),
/// step for step. The part of the source rect outside what the source
/// shows — its clip list under ClipByChildren, `NotClippedByChildren`
/// under IncludeInferiors, its extent for a pixmap — moved over the
/// destination and cut to what of it shows likewise, and to the GC's
/// client clip (which Xorg applies without the clip origin). Past
/// `RECTLIMIT` rects onto a window it is sent as its extents, unless the
/// source's shape does not hold the source rect. `None` when nothing is
/// exposed or there is nothing to do: Xorg's NULL return, NoExpose.
/// The flag says whether the region was reduced to its extents.
#[allow(clippy::too_many_arguments)]
fn copy_exposed_region(
    state: &ServerState,
    src: ResourceId,
    dst: ResourceId,
    draw_state: &crate::backend::DrawState,
    graphics_exposures: bool,
    (src_x, src_y): (i16, i16),
    (width, height): (u16, u16),
    (dst_x, dst_y): (i16, i16),
) -> Option<(Vec<x11::xfixes::RegionRect>, bool)> {
    use crate::core_loop::clip_list;
    let dst_is_window = state.resources.window(dst).is_some();
    if !graphics_exposures && !dst_is_window {
        return None;
    }
    let include_inferiors = matches!(
        draw_state.subwindow_mode,
        crate::backend::SubwindowMode::IncludeInferiors
    );
    let src_box = x11::xfixes::RegionRect {
        x: src_x,
        y: src_y,
        width,
        height,
    };
    let window_clip = |w: ResourceId| {
        if include_inferiors {
            clip_list::not_clipped_by_children(state, w)
        } else {
            clip_list::clip_list(state, w)
        }
    };
    let src_clip = if state.resources.window(src).is_some() {
        let clip = window_clip(src);
        if clip_list::contains(&clip, src_box) {
            return None;
        }
        clip
    } else {
        let (w, h) = drawable_size(state, src)?;
        let whole = x11::xfixes::RegionRect {
            x: 0,
            y: 0,
            width: w,
            height: h,
        };
        if clip_list::contains(&[whole], src_box) {
            return None;
        }
        vec![whole]
    };
    let dst_clip = if dst == src {
        src_clip.clone()
    } else if dst_is_window {
        window_clip(dst)
    } else {
        let (w, h) = drawable_size(state, dst)?;
        vec![x11::xfixes::RegionRect {
            x: 0,
            y: 0,
            width: w,
            height: h,
        }]
    };
    let hidden = clip_list::subtract(&[src_box], &src_clip);
    let moved = clip_list::translate(
        hidden,
        i32::from(dst_x) - i32::from(src_x),
        i32::from(dst_y) - i32::from(src_y),
    );
    let mut exposed = clip_list::intersect(&moved, &dst_clip);
    if let crate::backend::ClipState::Rectangles { rects, .. } = &draw_state.clip {
        let client: Vec<x11::xfixes::RegionRect> = rects
            .rectangles
            .chunks_exact(8)
            .map(|c| x11::xfixes::RegionRect {
                x: i16::from_le_bytes([c[0], c[1]]),
                y: i16::from_le_bytes([c[2], c[3]]),
                width: u16::from_le_bytes([c[4], c[5]]),
                height: u16::from_le_bytes([c[6], c[7]]),
            })
            .collect();
        exposed = clip_list::intersect(&exposed, &client);
    }
    let mut extents = graphics_exposures && exposed.len() > clip_list::RECTLIMIT && dst_is_window;
    if extents
        && let Some(shape) = state
            .shape_windows
            .get(&src)
            .and_then(|s| s.clip.as_ref().or(s.bounding.as_ref()))
        && !clip_list::contains(shape, src_box)
    {
        extents = false;
    }
    if exposed.is_empty() {
        return None;
    }
    if extents {
        exposed = vec![crate::nested::region_extents(&exposed)];
    }
    Some((exposed, extents))
}

/// [`copy_exposed_region`]'s side effects: the destination window's
/// background over the region unless it is None (`miHandleExposures`
/// paints it, cut to the clip list when reduced to extents), and the
/// GraphicsExpose events, or one NoExpose, when the GC asks for them.
#[allow(clippy::too_many_arguments)]
fn finish_copy_exposures(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    dst: ResourceId,
    dst_host: u32,
    exposed: Option<(Vec<x11::xfixes::RegionRect>, bool)>,
    graphics_exposures: bool,
    major_opcode: u8,
) -> io::Result<()> {
    let rects = exposed.as_ref().map_or(&[][..], |(r, _)| r.as_slice());
    if let Some(window) = state.resources.window(dst)
        && !window.background_none
        && !rects.is_empty()
    {
        let paint = match &exposed {
            Some((r, true)) => crate::core_loop::clip_list::intersect(
                r,
                &crate::core_loop::clip_list::clip_list(state, dst),
            ),
            _ => rects.to_vec(),
        };
        for r in &paint {
            backend.paint_window_background_rect(origin, dst_host, r.x, r.y, r.width, r.height)?;
            let _dropped = accumulate_damage_to_state(state, dst, r.x, r.y, r.width, r.height);
        }
    }
    if graphics_exposures {
        let events: Vec<(i16, i16, u16, u16)> = rects
            .iter()
            .map(|r| (r.x, r.y, r.width, r.height))
            .collect();
        emit_copy_exposures(state, client_id, dst, &events, major_opcode);
    }
    Ok(())
}

/// Emit the CopyArea/CopyPlane graphics-exposures contract events to
/// the requesting client: one GraphicsExpose per missing dst-coord
/// sub-rect (count = number still to follow), or one NoExposure when
/// the source was fully available. Only called when the GC has
/// graphics-exposures=True; the events go to the REQUESTOR
/// unconditionally (not gated by any event mask).
fn emit_copy_exposures(
    state: &mut ServerState,
    client_id: ClientId,
    dst: ResourceId,
    missing: &[(i16, i16, u16, u16)],
    major_opcode: u8,
) {
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return;
    };
    let seq = SequenceNumber(
        client
            .last_sequence
            .load(std::sync::atomic::Ordering::Relaxed),
    );
    if missing.is_empty() {
        let mut buf = Vec::with_capacity(32);
        x11::encode_no_exposure_event(&mut buf, seq, client.byte_order, dst, 0, major_opcode);
        let _ = write_to_client(client, client_id, &buf);
        return;
    }
    let total = missing.len();
    for (i, (x, y, w, h)) in missing.iter().enumerate() {
        let mut buf = Vec::with_capacity(32);
        x11::encode_graphics_expose_event(
            &mut buf,
            seq,
            client.byte_order,
            dst,
            u16::try_from(*x).unwrap_or(0),
            u16::try_from(*y).unwrap_or(0),
            *w,
            *h,
            0,
            u16::try_from(total - 1 - i).unwrap_or(0),
            major_opcode,
        );
        let _ = write_to_client(client, client_id, &buf);
    }
}

pub(super) fn handle_copy_area(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    let Some(request) = x11::copy_area_request(body) else {
        return Ok(RequestOutcome::Handled);
    };
    if request.width == 0 || request.height == 0 {
        return Ok(RequestOutcome::Handled);
    }
    debug!(
        "client {} #{} CopyArea src=0x{:x} dst=0x{:x} gc=0x{:x} src=({},{}) dst=({},{}) {}x{}",
        client_id.0,
        sequence.0,
        request.src.0,
        request.dst.0,
        request.gc.0,
        request.src_x,
        request.src_y,
        request.dst_x,
        request.dst_y,
        request.width,
        request.height
    );
    // Validate src + dst + gc together. Order mirrors Xorg's
    // VALIDATE_DRAWABLE_AND_GC: src-side first (BadDrawable / BadMatch
    // inputonly), then GC (BadGC), then dst-side, then cross-depth
    // (BadMatch). Note: each call covers BadGC; the second is a
    // redundant lookup but the cost is tiny and the deduplicated form
    // would obscure the per-side error ordering Xorg follows.
    if let Err((code, bad_value)) = validate_drawable_and_gc(state, request.src, request.gc) {
        return emit_x11_error(state, client_id, sequence, code, bad_value, 62);
    }
    if let Err((code, bad_value)) = validate_drawable_and_gc(state, request.dst, request.gc) {
        return emit_x11_error(state, client_id, sequence, code, bad_value, 62);
    }
    let draw_state = state.resources.resolve_draw_state(request.gc);
    let src = state.resources.host_drawable_target(request.src);
    let dst = state.resources.host_drawable_target(request.dst);
    if let (Some(src), Some(dst)) = (src.as_ref(), dst.as_ref()) {
        // Keep window identity so the backend applies its content offset
        // and clip before resolving a redirect backing. Named pixmaps still
        // address the entire backing, including its border.
        let src_host = state
            .resources
            .window(request.src)
            .and_then(|w| w.host_xid)
            .map_or_else(|| src.host_xid(), |h| h.as_raw());
        let dst_host = state
            .resources
            .window(request.dst)
            .and_then(|w| w.host_xid)
            .map_or_else(|| dst.host_xid(), |h| h.as_raw());
        if src.depth() != dst.depth() {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_MATCH,
                request.dst.0,
                62,
            );
        }
        let st = draw_state.unwrap_or_default();
        backend.apply_clip_state(origin, &st.clip)?;
        backend.apply_draw_state(origin, &st)?;
        // Clamp the copy to the AVAILABLE part of the source drawable
        // (X11 §CopyArea): out-of-bounds source regions are never
        // copied; they become the GraphicsExpose region below.
        let (avail, exposed) = if window_unviewable(state, request.dst) {
            // Xorg miDoCopy returns before copying or exposing: NoExpose (micopy.c:157).
            (None, None)
        } else {
            let graphics_exposures = state
                .resources
                .gc(request.gc)
                .is_some_and(|g| g.graphics_exposures);
            (
                copy_area_source_split(
                    state,
                    request.src,
                    request.src_x,
                    request.src_y,
                    request.dst_x,
                    request.dst_y,
                    request.width,
                    request.height,
                )
                .0,
                copy_exposed_region(
                    state,
                    request.src,
                    request.dst,
                    &st,
                    graphics_exposures,
                    (request.src_x, request.src_y),
                    (request.width, request.height),
                    (request.dst_x, request.dst_y),
                ),
            )
        };
        let request = match avail {
            Some((sx, sy, dx, dy, w, h)) => x11::CopyAreaRequest {
                src_x: sx,
                src_y: sy,
                dst_x: dx,
                dst_y: dy,
                width: w,
                height: h,
                ..request
            },
            None => x11::CopyAreaRequest {
                width: 0,
                height: 0,
                ..request
            },
        };
        // Stage 4d Manual-redirect fix (codex 2026-05-18): when the
        // destination is a window, the X11 spec requires copy_area to
        // honour (a) the GC's clip-mask if rectangular and (b) the
        // `subwindow-mode=ClipByChildren` default by subtracting every
        // mapped child window's geometry from the destination rect.
        //
        // Split in window-local coordinates before the backend translates
        // each piece to backing space.
        // ClipState::Pixmap (mask-pixmap clip) is out of scope for this
        // fix and passes through untouched.
        let copy_sub_rects = if request.width == 0 || request.height == 0 {
            Vec::new()
        } else {
            copy_area_effective_dst_rects(state, request.dst, &st, &request)
        };
        if copy_sub_rects.is_empty() {
            debug!(
                "client {} #{} CopyArea fully clipped, no backend call",
                client_id.0, sequence.0,
            );
        }
        for sub in &copy_sub_rects {
            // src coords shift by the same delta the dst sub-rect
            // shifted from the original dst_xy.
            let sub_src_x = request
                .src_x
                .saturating_add(sub.x.saturating_sub(request.dst_x));
            let sub_src_y = request
                .src_y
                .saturating_add(sub.y.saturating_sub(request.dst_y));
            backend.copy_area(
                origin, src_host, dst_host, sub_src_x, sub_src_y, sub.x, sub.y, sub.width,
                sub.height,
            )?;
        }
        let dst_id = request.dst;
        // Damage per actually-copied sub-rect rather than the original
        // full rect — avoids over-damaging the children's areas a
        // ClipByChildren split just excluded. Skipped naturally when
        // the slice is empty (fully-clipped copy).
        for sub in &copy_sub_rects {
            let _dropped =
                accumulate_damage_to_state(state, dst_id, sub.x, sub.y, sub.width, sub.height);
        }
        // Graphics-exposures contract (still fires when the copy was
        // fully clipped — codex 2026-05-18 follow-up): the exposed
        // destination gets its background, and GraphicsExpose per rect
        // or one NoExposure go to the requestor unconditionally (not
        // mask-gated).
        let graphics_exposures = state
            .resources
            .gc(request.gc)
            .is_some_and(|g| g.graphics_exposures);
        finish_copy_exposures(
            state,
            backend,
            origin,
            client_id,
            request.dst,
            dst_host,
            exposed,
            graphics_exposures,
            62,
        )?;
    }
    Ok(RequestOutcome::Handled)
}

/// Compute the surviving destination sub-rectangles for an X11
/// CopyArea request after applying:
///
/// 1. GC rectangular clip-mask (`ClipState::Rectangles`). Empty or
///    completely-disjoint clip → empty Vec (spec-correct no-op).
///    `ClipState::Pixmap` is currently NOT honoured here (out of
///    scope for the Stage 4d fix — see the inline TODO below).
/// 2. `subwindow-mode == ClipByChildren` (X11 default): subtract
///    every mapped child window's geometry from the destination
///    rectangle. v2 collapses a redirected subtree into a single
///    backing pixmap, so this is the layer that prevents marco's
///    decoration copies into a frame window from clobbering the
///    reparented client child area.
///
/// Pixmap destinations skip clipping entirely (no children to
/// subtract; rectangular clip already applies via the same path).
///
/// Returns rectangles in the **destination window's** coordinate
/// space (matching the wire `dst_x` / `dst_y` units).
pub(super) fn copy_area_effective_dst_rects(
    state: &crate::server::ServerState,
    dst_id: ResourceId,
    draw_state: &crate::backend::DrawState,
    req: &yserver_protocol::x11::CopyAreaRequest,
) -> Vec<CopyAreaSubRect> {
    let mut current: Vec<CopyAreaSubRect> = vec![CopyAreaSubRect {
        x: req.dst_x,
        y: req.dst_y,
        width: req.width,
        height: req.height,
    }];
    let mut gc_clip_rects: Vec<CopyAreaSubRect> = Vec::new();
    // Step 1: GC rectangular clip-mask. `ClipState::None` is
    // "no clip" (pass through); `ClipState::Pixmap` is the
    // depth-1 mask form — TODO: rasterise to rect-band; for now
    // pass through (mirrors v1's `intersect_with_current_clip`
    // pixmap-clip skip).
    if let crate::backend::ClipState::Rectangles { origin, rects } = &draw_state.clip {
        gc_clip_rects = Vec::with_capacity(rects.rectangles.len() / 8);
        for chunk in rects.rectangles.chunks_exact(8) {
            let cx = i16::from_le_bytes([chunk[0], chunk[1]]).saturating_add(origin.0);
            let cy = i16::from_le_bytes([chunk[2], chunk[3]]).saturating_add(origin.1);
            let cw = u16::from_le_bytes([chunk[4], chunk[5]]);
            let ch = u16::from_le_bytes([chunk[6], chunk[7]]);
            if cw > 0 && ch > 0 {
                gc_clip_rects.push(CopyAreaSubRect {
                    x: cx,
                    y: cy,
                    width: cw,
                    height: ch,
                });
            }
        }
        current = current
            .into_iter()
            .flat_map(|r| {
                gc_clip_rects
                    .iter()
                    .filter_map(move |c| intersect_sub_rects(r, *c))
            })
            .collect();
        if current.is_empty() {
            log::debug!(
                target: "yserver_core::core_loop",
                "copy_area clip empty after GC clip dst=0x{:x} req=({},{}) {}x{} subwindow_mode={:?} gc_clip_rects={:?}",
                dst_id.0,
                req.dst_x,
                req.dst_y,
                req.width,
                req.height,
                draw_state.subwindow_mode,
                gc_clip_rects,
            );
            return current;
        }
    }
    // Step 2: ClipByChildren. Only meaningful when:
    // - dst is a window (pixmaps have no children),
    // - subwindow-mode is ClipByChildren (default),
    // - the window has mapped children.
    let Some(dst_window) = state.resources.window(dst_id) else {
        return current;
    };
    if !matches!(
        draw_state.subwindow_mode,
        crate::backend::SubwindowMode::ClipByChildren
    ) {
        return current;
    }
    let child_rects: Vec<CopyAreaSubRect> = dst_window
        .children
        .iter()
        .filter_map(|cid| {
            let c = state.resources.window(*cid)?;
            // Manually-redirected children don't claim the parent's
            // pixmap real estate — the redirecting compositor (which
            // may BE the parent's own client; see
            // notification-area-applet for a live example) puts the
            // children's pixels there itself. Subtracting them
            // strips the compositor's own composite-target rect to
            // empty, blocking the icons from reaching the parent's
            // backing. Automatic-redirected children are still
            // subtracted: under Automatic mode the X server
            // auto-composites them into the parent's pixmap, so the
            // parent's own paint must avoid those rects to stay
            // out of the auto-composite's way.
            let is_manual = matches!(
                effective_redirect_mode_for_window(state, *cid),
                Some(crate::server::CompositeRedirectMode::Manual)
            );
            (c.class == crate::resources::WindowClass::InputOutput
                && c.map_state == crate::resources::MapState::Viewable
                && c.width > 0
                && c.height > 0
                && !is_manual)
                .then(|| {
                    let rect = x11::xfixes::RegionRect {
                        x: c.x,
                        y: c.y,
                        width: c.width,
                        height: c.height,
                    };
                    // A shaped child takes only its bounding shape out
                    // (Xorg subtracts its `borderSize`): GDK clips a
                    // native window inside a client-side one with it, and
                    // the parent's button bar outside it stays drawable.
                    crate::nested::intersect_regions(
                        &[rect],
                        &current_bounding_in_parent(state, *cid),
                    )
                })
        })
        .flatten()
        .map(|r| CopyAreaSubRect {
            x: r.x,
            y: r.y,
            width: r.width,
            height: r.height,
        })
        .collect();
    if child_rects.is_empty() {
        return current;
    }
    for child in &child_rects {
        let mut next = Vec::new();
        for r in current {
            next.extend(subtract_sub_rect(r, *child));
        }
        current = next;
        if current.is_empty() {
            let redirect_mode = effective_redirect_mode_for_window(state, dst_id);
            log::debug!(
                target: "yserver_core::core_loop",
                "copy_area clip empty after ClipByChildren dst=0x{:x} req=({},{}) {}x{} subwindow_mode={:?} \
                 redirect_mode={:?} gc_clip_rects={:?} child_rects={:?}",
                dst_id.0,
                req.dst_x,
                req.dst_y,
                req.width,
                req.height,
                draw_state.subwindow_mode,
                redirect_mode,
                gc_clip_rects,
                child_rects,
            );
            return current;
        }
    }
    current
}

/// Rect ∩ rect. Returns `None` for disjoint or zero-area input.
fn intersect_sub_rects(a: CopyAreaSubRect, b: CopyAreaSubRect) -> Option<CopyAreaSubRect> {
    let ax0 = i32::from(a.x);
    let ay0 = i32::from(a.y);
    let ax1 = ax0 + i32::from(a.width);
    let ay1 = ay0 + i32::from(a.height);
    let bx0 = i32::from(b.x);
    let by0 = i32::from(b.y);
    let bx1 = bx0 + i32::from(b.width);
    let by1 = by0 + i32::from(b.height);
    let ix0 = ax0.max(bx0);
    let iy0 = ay0.max(by0);
    let ix1 = ax1.min(bx1);
    let iy1 = ay1.min(by1);
    if ix0 >= ix1 || iy0 >= iy1 {
        return None;
    }
    Some(CopyAreaSubRect {
        x: i16::try_from(ix0).ok()?,
        y: i16::try_from(iy0).ok()?,
        width: u16::try_from(ix1 - ix0).ok()?,
        height: u16::try_from(iy1 - iy0).ok()?,
    })
}

/// `outer \ inner` → up to 4 disjoint sub-rectangles (top strip,
/// bottom strip, left middle, right middle). If `inner` doesn't
/// intersect `outer`, returns `[outer]` unchanged.
fn subtract_sub_rect(outer: CopyAreaSubRect, inner: CopyAreaSubRect) -> Vec<CopyAreaSubRect> {
    let ox0 = i32::from(outer.x);
    let oy0 = i32::from(outer.y);
    let ox1 = ox0 + i32::from(outer.width);
    let oy1 = oy0 + i32::from(outer.height);
    let ix0 = i32::from(inner.x).max(ox0);
    let iy0 = i32::from(inner.y).max(oy0);
    let ix1 = (i32::from(inner.x) + i32::from(inner.width)).min(ox1);
    let iy1 = (i32::from(inner.y) + i32::from(inner.height)).min(oy1);
    if ix0 >= ix1 || iy0 >= iy1 {
        return vec![outer];
    }
    let mk = |x: i32, y: i32, w: i32, h: i32| -> Option<CopyAreaSubRect> {
        Some(CopyAreaSubRect {
            x: i16::try_from(x).ok()?,
            y: i16::try_from(y).ok()?,
            width: u16::try_from(w).ok()?,
            height: u16::try_from(h).ok()?,
        })
    };
    let mut out = Vec::with_capacity(4);
    // Top strip
    if oy0 < iy0
        && let Some(r) = mk(ox0, oy0, ox1 - ox0, iy0 - oy0)
    {
        out.push(r);
    }
    // Bottom strip
    if iy1 < oy1
        && let Some(r) = mk(ox0, iy1, ox1 - ox0, oy1 - iy1)
    {
        out.push(r);
    }
    // Left middle
    if ox0 < ix0
        && let Some(r) = mk(ox0, iy0, ix0 - ox0, iy1 - iy0)
    {
        out.push(r);
    }
    // Right middle
    if ix1 < ox1
        && let Some(r) = mk(ix1, iy0, ox1 - ix1, iy1 - iy0)
    {
        out.push(r);
    }
    out
}

pub(super) fn handle_copy_plane(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    if body.len() < 28 {
        return Ok(RequestOutcome::Handled);
    }
    let src = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
    let dst = ResourceId(u32::from_le_bytes([body[4], body[5], body[6], body[7]]));
    let gc = ResourceId(u32::from_le_bytes([body[8], body[9], body[10], body[11]]));
    let sx = i16::from_le_bytes([body[12], body[13]]);
    let sy = i16::from_le_bytes([body[14], body[15]]);
    let dx = i16::from_le_bytes([body[16], body[17]]);
    let dy = i16::from_le_bytes([body[18], body[19]]);
    let w = u16::from_le_bytes([body[20], body[21]]);
    let h = u16::from_le_bytes([body[22], body[23]]);
    let plane = u32::from_le_bytes([body[24], body[25], body[26], body[27]]);
    if w == 0 || h == 0 {
        return Ok(RequestOutcome::Handled);
    }
    // CopyPlane: dst.depth must match gc.depth; src can be any depth
    // (the plane mask extracts a single bit from src).
    if let Err((code, bad_value)) = validate_drawable_only(state, src) {
        return emit_x11_error(state, client_id, sequence, code, bad_value, 63);
    }
    if let Err((code, bad_value)) = validate_drawable_and_gc(state, dst, gc) {
        return emit_x11_error(state, client_id, sequence, code, bad_value, 63);
    }
    let draw_state = state.resources.resolve_draw_state(gc);
    let src_target = state.resources.host_drawable_target(src);
    let dst_target = state.resources.host_drawable_target(dst);
    if let (Some(srct), Some(dstt)) = (src_target, dst_target) {
        let st = draw_state.unwrap_or_default();
        backend.apply_clip_state(origin, &st.clip)?;
        backend.apply_draw_state(origin, &st)?;
        // Clamp to the available source region (same contract as
        // CopyArea — out-of-bounds source becomes GraphicsExpose).
        let graphics_exposures = state.resources.gc(gc).is_some_and(|g| g.graphics_exposures);
        let (avail, exposed) = if window_unviewable(state, dst) {
            // Xorg miDoCopy returns before copying or exposing: NoExpose (micopy.c:157).
            (None, None)
        } else {
            (
                copy_area_source_split(state, src, sx, sy, dx, dy, w, h).0,
                copy_exposed_region(
                    state,
                    src,
                    dst,
                    &st,
                    graphics_exposures,
                    (sx, sy),
                    (w, h),
                    (dx, dy),
                ),
            )
        };
        if let Some((asx, asy, adx, ady, aw, ah)) = avail {
            backend.copy_plane(
                origin,
                srct.host_xid(),
                dstt.host_xid(),
                asx,
                asy,
                adx,
                ady,
                aw,
                ah,
                plane,
            )?;
            let _dropped = accumulate_damage_to_state(state, dst, adx, ady, aw, ah);
        }
        // The same exposure contract as CopyArea (`miCopyPlane` hands its
        // region to `miHandleExposures` too).
        finish_copy_exposures(
            state,
            backend,
            origin,
            client_id,
            dst,
            dstt.host_xid(),
            exposed,
            graphics_exposures,
            63,
        )?;
    }
    debug!(
        "client {} #{} CopyPlane src=0x{:x} dst=0x{:x} src=({},{}) dst=({},{}) {}x{} plane=0x{:x}",
        client_id.0, sequence.0, src.0, dst.0, sx, sy, dx, dy, w, h, plane,
    );
    Ok(RequestOutcome::Handled)
}
