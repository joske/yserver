use super::*;

pub(super) fn mirror_shape_to_host_state(
    state: &ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    window: ResourceId,
    kind: u8,
) {
    use yserver_protocol::x11::shape as x11shape;
    // Mirror Bounding, Clip, AND Input shapes to the backend. Pre-fix
    // (2026-05-26 adapta-nokto investigation): only Bounding/Clip were
    // mirrored, so the backend's `core.shape_input` never reflected a
    // window's SHAPE Input region. That broke v2's `cursor_inside_shape`
    // hit-test for windows with non-default Input shapes — notably
    // adapta-nokto's MATE panel menu, which sets a shrunken Input shape
    // (`{x=16, y=12, w=188, h=227}` inside a 220x260 window) to make
    // the CSS box-shadow margin click-through. Without the Input shape
    // in the backend, `window_under_cursor` HIT the menu in its shadow
    // zone (window-local Y < 12), the protocol-layer fanout's hit-test
    // disagreed (it correctly excluded the shadow), and the Enter
    // event yserver emitted on the menu's host xid got re-routed by
    // the fanout to whatever's structurally under the cursor (the
    // panel) — never reaching the menu's client. GTK menu hover
    // state machine never engaged → no highlight, no click.
    if kind != x11shape::KIND_BOUNDING
        && kind != x11shape::KIND_CLIP
        && kind != x11shape::KIND_INPUT
    {
        return;
    }
    let Some(w) = state.resources.window(window) else {
        return;
    };
    let Some(host_xid) = w.host_xid else {
        return;
    };
    // Multi-monitor Bug A: a window with NO explicitly-set Bounding shape
    // is unshaped — its effective bounding region is the *live* window
    // geometry, which the backend scene already honors via the full-window
    // emit path. `shape_rects_for` would instead materialize
    // `default_shape_rect` (the geometry at THIS instant) into a concrete
    // rect; mirroring that freezes a fixed extent into the backend's
    // `shape_bounding` that goes stale when the window is later resized.
    // The Composite Overlay Window is the load-bearing case: marco resets
    // its Bounding shape to None while the screen is single-head, we froze
    // (0,0 2560x1440), then RANDR grew the COW to 5120 on apply — the scene
    // kept clipping the now-5120 COW to the stale 2560 rect, so the 2nd
    // output sampled the wrong (left) half placed off-screen and screen 2
    // went dark. For an unset Bounding shape, mirror EMPTY rects so the
    // backend drops the entry and the scene tracks live geometry (Xorg
    // parity: a None bounding region is never materialized into a rect).
    //
    // CLIP joined Bounding on 2026-09-08 (#133). The note here used to say
    // "Clip/Input keep mirroring the default rect — the scene's compose clip
    // only consults Bounding"; #133 step 5 made the walk clip DESCENDANTS to
    // the parent's clip shape (`SetWinSize` intersects winSize with it,
    // `dix/window.c:1735`), which retired that premise and turned the frozen
    // rect into a visible defect. Measured in vng: awesome resets its client
    // frame's clip shape with `ShapeMask(Clip, src=None)` while the frame is
    // 820x583, we froze that rect, and after the tiling resize to 608x734 the
    // client was clipped 168 rows short — leaving the frame's uninitialised
    // storage on screen as a white block. Xorg on the identical scenario has
    // no white pixel at all, because an unset clip region is never
    // materialized there either.
    //
    // Input still mirrors the default rect: it drives the cursor hit-test,
    // which wants a concrete region and does not clip descendants.
    if (kind == x11shape::KIND_BOUNDING || kind == x11shape::KIND_CLIP)
        && !crate::nested::shape_kind_is_set(state, window, kind)
    {
        // Unset Bounding/Clip shape → None (drop the backend entry; the scene
        // tracks live window geometry). Distinct from an explicit empty
        // region — see set_shape_rectangles' Option contract (DRIFT 1).
        let _ = backend.set_shape_rectangles(origin, host_xid.as_raw(), kind, None);
        return;
    }
    let rects = crate::nested::shape_rects_for(state, window, kind);
    // Explicit shape (Some), possibly empty — `Some(&[])` is an empty
    // region (click-through / drawn-as-nothing), NOT unset.
    let _ = backend.set_shape_rectangles(origin, host_xid.as_raw(), kind, Some(&rects));
}

pub(super) fn drawable_full_rect_xfixes(
    state: &ServerState,
    drawable: ResourceId,
) -> yserver_protocol::x11::xfixes::RegionRect {
    use yserver_protocol::x11::xfixes;
    if let Some(window) = state.resources.window(drawable) {
        return xfixes::RegionRect {
            x: 0,
            y: 0,
            width: window.width,
            height: window.height,
        };
    }
    state.resources.pixmap(drawable).map_or(
        xfixes::RegionRect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        },
        |pixmap| xfixes::RegionRect {
            x: 0,
            y: 0,
            width: pixmap.width,
            height: pixmap.height,
        },
    )
}

pub(super) fn normalize_region_rects(
    rects: Vec<yserver_protocol::x11::xfixes::RegionRect>,
) -> Vec<yserver_protocol::x11::xfixes::RegionRect> {
    crate::nested::normalize_region_rects(rects)
}

pub(super) fn format_region_rects(rects: &[yserver_protocol::x11::xfixes::RegionRect]) -> String {
    use std::fmt::Write as _;

    let mut out = String::from("[");
    for (i, rect) in rects.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let _ = write!(
            out,
            "({},{} {}x{})",
            rect.x, rect.y, rect.width, rect.height
        );
    }
    out.push(']');
    out
}

#[allow(clippy::too_many_arguments)]
/// Emit a `ShapeNotify` event to every client that `ShapeSelectInput`'d
/// `window`, after its `kind` region (bounding/clip/input) changed. Xorg's
/// `SendShapeNotify` does this on every shape mutation; yserver previously
/// applied the change to its own store (so its hit-test stayed correct) but
/// never told subscribers, so a compositing WM's cached input region went
/// permanently stale — clicks over the grown region fell through to the
/// window below (cinnamon "nemo rises"). No-op when nobody selected.
pub(super) fn emit_shape_notify(state: &mut ServerState, window: ResourceId, kind: u8) {
    let targets: Vec<ClientId> = state
        .shape_select_masks
        .iter()
        .filter_map(|((cid, w), enabled)| (*enabled && *w == window).then_some(ClientId(*cid)))
        .collect();
    if targets.is_empty() {
        return;
    }
    let rects = crate::nested::shape_rects_for(state, window, kind);
    let shaped = crate::nested::shape_kind_is_set(state, window, kind);
    let extents = crate::nested::region_extents(&rects);
    let server_time = state.timestamp_now();
    let _dropped = fanout_event_to_clients(state, &targets, |buf, seq, order| {
        yserver_protocol::x11::shape::encode_shape_notify_event(
            buf,
            seq,
            order,
            crate::nested::SHAPE_FIRST_EVENT,
            kind,
            window.0,
            extents,
            shaped,
            server_time,
        );
    });
}

/// Compact one-line dump of a SHAPE region's rectangles for the
/// `shape diag` traces, e.g. `[(0,0 306x49)]`. Truncates past 8 rects.
fn fmt_shape_rects(rects: &[yserver_protocol::x11::xfixes::RegionRect]) -> String {
    let mut s = String::from("[");
    for (i, r) in rects.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        if i == 8 {
            s.push_str(&format!("…+{}", rects.len() - 8));
            break;
        }
        s.push_str(&format!("({},{} {}x{})", r.x, r.y, r.width, r.height));
    }
    s.push(']');
    s
}

/// SHAPE requests, and the exposures a viewable window's new bounding or clip
/// shape makes: Xorg's `miSetShape` (`mi/miwindow.c:637-677`) revalidates
/// the tree, so what the window no longer covers is exposed beneath it
/// and what it newly covers is exposed to it. GDK clips a native window
/// inside a client-side one with its bounding shape and shifts that
/// shape on every scroll; the dialog repaints its button bar below the
/// viewport only on that Expose.
pub(super) fn handle_shape_request(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    use yserver_protocol::x11::shape as x11shape;
    // Rectangles, Mask and Combine carry the destination kind at byte 1,
    // Offset at byte 0; all four the destination window at bytes 4..8.
    let kind_at = match header.data {
        x11shape::RECTANGLES | x11shape::MASK | x11shape::COMBINE => Some(1),
        x11shape::OFFSET => Some(0),
        _ => None,
    };
    let tree_change = kind_at
        .filter(|at| {
            body.get(*at)
                .is_some_and(|k| *k == x11shape::KIND_BOUNDING || *k == x11shape::KIND_CLIP)
        })
        .and_then(|_| body.get(4..8))
        .map(|b| ResourceId(u32::from_le_bytes([b[0], b[1], b[2], b[3]])))
        .and_then(|w| crate::core_loop::clip_list::TreeChange::begin(state, w, None));
    let outcome =
        handle_shape_request_ops(state, backend, origin, client_id, sequence, header, body)?;
    if let Some(change) = tree_change {
        for (w, region) in change.exposed(state, None) {
            send_window_exposures(state, backend, origin, w, &region, true);
        }
    }
    Ok(outcome)
}

fn handle_shape_request_ops(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    use yserver_protocol::x11::{ClientByteOrder, shape as x11shape};
    let byte_order = state
        .clients
        .get(&client_id.0)
        .map_or(ClientByteOrder::LittleEndian, |c| c.byte_order);
    let minor = header.data;
    debug!(
        "client {} #{} SHAPE::minor={} body_len={}",
        client_id.0,
        sequence.0,
        minor,
        body.len()
    );
    match minor {
        x11shape::QUERY_VERSION => {
            let reply = x11shape::encode_query_version_reply(byte_order, sequence);
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            let _byte_order = client.byte_order;
            return Ok(write_to_client(client, client_id, &reply));
        }
        x11shape::RECTANGLES => {
            if let Some((req, rects)) = x11shape::parse_rectangles_request(body) {
                let window = ResourceId(req.dest);
                debug!(
                    "client {} SHAPE::Rectangles dest=0x{:x} kind={} op={} off=({},{}) nrects={}",
                    client_id.0,
                    req.dest,
                    req.dest_kind,
                    req.op,
                    req.x_off,
                    req.y_off,
                    rects.len(),
                );
                let source = crate::nested::offset_rects(rects, req.x_off, req.y_off);
                let current = crate::nested::shape_rects_for(state, window, req.dest_kind);
                let new_rects = crate::nested::apply_shape_op(current, source, req.op);
                let changed =
                    crate::nested::set_shape_rects(state, window, req.dest_kind, new_rects);
                mirror_shape_to_host_state(state, backend, origin, window, req.dest_kind);
                // Xorg miSetShape re-evaluates the pointer before ShapeNotify
                // goes out (`mi/miwindow.c:680`); so do the other SHAPE ops.
                backend.windows_restructured(state);
                if changed {
                    emit_shape_notify(state, window, req.dest_kind);
                }
            }
        }
        x11shape::MASK => {
            if let Some(req) = x11shape::parse_mask_request(body) {
                let window = ResourceId(req.dest);
                debug!(
                    "client {} SHAPE::Mask dest=0x{:x} kind={} op={} src=0x{:x} off=({},{})",
                    client_id.0, req.dest, req.dest_kind, req.op, req.src, req.x_off, req.y_off,
                );
                if req.src == 0 {
                    let changed = crate::nested::clear_shape_rects(state, window, req.dest_kind);
                    mirror_shape_to_host_state(state, backend, origin, window, req.dest_kind);
                    backend.windows_restructured(state);
                    if changed {
                        emit_shape_notify(state, window, req.dest_kind);
                    }
                    return Ok(RequestOutcome::Handled);
                }
                // Read the depth-1 mask pixels and YX-band them into
                // rectangles. Falls back to the source pixmap's
                // bounding-box rect if the backend can't introspect
                // (host-X11 proxy mode), preserving the previous
                // best-effort behaviour for that path.
                let src_id = ResourceId(req.src);
                let host_xid = state.resources.pixmap(src_id).and_then(|p| p.host_xid);
                let banded = if let Some(host) = host_xid {
                    match backend.read_depth1_pixmap(origin, host.as_raw()) {
                        Ok(Some((w, h, bytes))) => {
                            let rects = crate::nested::bitmap_to_yx_banded_rects(&bytes, w, h);
                            log::debug!(
                                "shape diag: MASK src=0x{:x} read_depth1_pixmap ok ({}x{} \
                                 bytes={}) banded_rects={}",
                                req.src,
                                w,
                                h,
                                bytes.len(),
                                rects.len()
                            );
                            Some(rects)
                        }
                        Ok(None) => {
                            log::debug!(
                                "shape diag: MASK src=0x{:x} read_depth1_pixmap returned None \
                                 (no pixmap mirror)",
                                req.src
                            );
                            None
                        }
                        Err(e) => {
                            log::debug!(
                                "shape diag: MASK src=0x{:x} read_depth1_pixmap err: {e}",
                                req.src
                            );
                            None
                        }
                    }
                } else {
                    log::debug!(
                        "shape diag: MASK src=0x{:x} no host_xid for source pixmap (fallback to \
                         bounding-box rect)",
                        req.src
                    );
                    None
                };
                let source =
                    banded.unwrap_or_else(|| crate::nested::shape_mask_source_rects(state, src_id));
                let pre_off_count = source.len();
                let source = crate::nested::offset_rects(source, req.x_off, req.y_off);
                let current = crate::nested::shape_rects_for(state, window, req.dest_kind);
                let current_count = current.len();
                let new_rects = crate::nested::apply_shape_op(current, source, req.op);
                log::debug!(
                    "shape diag: MASK dest=0x{:x} kind={} op={} source_rects(pre_offset)={} \
                     current_rects={} new_rects={}",
                    req.dest,
                    req.dest_kind,
                    req.op,
                    pre_off_count,
                    current_count,
                    new_rects.len()
                );
                let changed =
                    crate::nested::set_shape_rects(state, window, req.dest_kind, new_rects);
                mirror_shape_to_host_state(state, backend, origin, window, req.dest_kind);
                backend.windows_restructured(state);
                if changed {
                    emit_shape_notify(state, window, req.dest_kind);
                }
            }
        }
        x11shape::COMBINE => {
            if let Some(req) = x11shape::parse_combine_request(body) {
                let dest = ResourceId(req.dest);
                let src = ResourceId(req.src);
                debug!(
                    "client {} SHAPE::Combine dest=0x{:x} dest_kind={} src=0x{:x} src_kind={} op={} off=({},{})",
                    client_id.0,
                    req.dest,
                    req.dest_kind,
                    req.src,
                    req.src_kind,
                    req.op,
                    req.x_off,
                    req.y_off,
                );
                let src_rects = crate::nested::shape_rects_for(state, src, req.src_kind);
                let src_count = src_rects.len();
                let src_dims = fmt_shape_rects(&src_rects);
                let source = crate::nested::offset_rects(src_rects, req.x_off, req.y_off);
                let current = crate::nested::shape_rects_for(state, dest, req.dest_kind);
                let current_count = current.len();
                let new_rects = crate::nested::apply_shape_op(current, source, req.op);
                let new_count = new_rects.len();
                log::debug!(
                    "shape diag: COMBINE dest=0x{:x}(kind={}) src=0x{:x}(kind={}) op={} \
                     src_rects={src_count} src_dims={src_dims} current_rects={current_count} \
                     new_rects={new_count} new_dims={}",
                    req.dest,
                    req.dest_kind,
                    req.src,
                    req.src_kind,
                    req.op,
                    fmt_shape_rects(&new_rects),
                );
                let changed = crate::nested::set_shape_rects(state, dest, req.dest_kind, new_rects);
                mirror_shape_to_host_state(state, backend, origin, dest, req.dest_kind);
                backend.windows_restructured(state);
                if changed {
                    emit_shape_notify(state, dest, req.dest_kind);
                }
            }
        }
        x11shape::OFFSET => {
            if let Some(req) = x11shape::parse_offset_request(body) {
                let dest = ResourceId(req.dest);
                let mut translated = false;
                if let Some(s) = state.shape_windows.get_mut(&dest)
                    && let Some(slot) = s.rects_mut(req.dest_kind)
                    && let Some(rects) = slot.as_mut()
                {
                    crate::nested::translate_region(rects, req.x_off, req.y_off);
                    translated = true;
                }
                if translated {
                    mirror_shape_to_host_state(state, backend, origin, dest, req.dest_kind);
                    backend.windows_restructured(state);
                    emit_shape_notify(state, dest, req.dest_kind);
                }
            }
        }
        x11shape::QUERY_EXTENTS => {
            let window = ResourceId(x11shape::parse_window(body).unwrap_or(ROOT_WINDOW.0));
            let bounding_rects =
                crate::nested::shape_rects_for(state, window, x11shape::KIND_BOUNDING);
            let clip_rects = crate::nested::shape_rects_for(state, window, x11shape::KIND_CLIP);
            let bounding_shaped =
                crate::nested::shape_kind_is_set(state, window, x11shape::KIND_BOUNDING);
            let clip_shaped = crate::nested::shape_kind_is_set(state, window, x11shape::KIND_CLIP);
            let bounding = crate::nested::region_extents(&bounding_rects);
            let clip = crate::nested::region_extents(&clip_rects);
            let reply = x11shape::encode_query_extents_reply(
                byte_order,
                sequence,
                bounding_shaped,
                clip_shaped,
                bounding,
                clip,
            );
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            let _byte_order = client.byte_order;
            return Ok(write_to_client(client, client_id, &reply));
        }
        x11shape::SELECT_INPUT => {
            if let Some(req) = x11shape::parse_select_input_request(body) {
                let key = (client_id.0, ResourceId(req.window));
                if req.enable {
                    state.shape_select_masks.insert(key, true);
                } else {
                    state.shape_select_masks.remove(&key);
                }
            }
        }
        x11shape::INPUT_SELECTED => {
            let window = ResourceId(x11shape::parse_window(body).unwrap_or(ROOT_WINDOW.0));
            let enabled = state
                .shape_select_masks
                .get(&(client_id.0, window))
                .copied()
                .unwrap_or(false);
            let reply = x11shape::encode_input_selected_reply(byte_order, sequence, enabled);
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            let _byte_order = client.byte_order;
            return Ok(write_to_client(client, client_id, &reply));
        }
        x11shape::GET_RECTANGLES => {
            let (window, kind) = x11shape::parse_get_rectangles_request(body)
                .map(|(w, k)| (ResourceId(w), k))
                .unwrap_or((ROOT_WINDOW, x11shape::KIND_BOUNDING));
            let rects = crate::nested::shape_rects_for(state, window, kind);
            let reply = x11shape::encode_get_rectangles_reply(
                byte_order,
                sequence,
                x11shape::ORDERING_YX_BANDED,
                &rects,
            );
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            let _byte_order = client.byte_order;
            return Ok(write_to_client(client, client_id, &reply));
        }
        other => {
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
    }
    Ok(RequestOutcome::Handled)
}

/// Xorg `ProcXFixesDispatch`: a client may only use the requests of the
/// XFIXES major version it negotiated — before its first `QueryVersion`
/// that is `QueryVersion` alone. Anything else is BadRequest.
pub(super) fn dispatch_xfixes_request(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    use yserver_protocol::x11::xfixes as x11xfixes;
    let client_major = state
        .xfixes_client_major
        .get(&client_id.0)
        .copied()
        .unwrap_or(0);
    if !x11xfixes::request_allowed(client_major, header.data) {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_REQUEST,
            0,
            u16::from(header.data),
            header.opcode,
        );
    }
    handle_xfixes_request(state, backend, origin, client_id, sequence, header, body)
}

/// Xorg `VERIFY_CURSOR` for XFIXES: resolve a cursor XID to its host
/// handle, or send BadCursor naming it. `Err(outcome)` carries the error
/// already written.
fn xfixes_verify_cursor(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    cursor: u32,
) -> Result<Option<u32>, io::Result<RequestOutcome>> {
    if state.resources.cursor_exists(ResourceId(cursor)) {
        return Ok(state.resources.cursor_host_xid(ResourceId(cursor)));
    }
    Err(emit_x11_error_with_minor(
        state,
        client_id,
        sequence,
        x11::error::BAD_CURSOR,
        cursor,
        u16::from(header.data),
        header.opcode,
    ))
}

/// Xorg `ReplaceCursor` for XFIXES `ChangeCursor` / `ChangeCursorByName`:
/// every cursor XID and every displayed use of host cursor `old_host`
/// switches to `source`'s cursor. Replacing a cursor with itself is a
/// no-op, and the old host cursor is released once nothing names it.
fn xfixes_replace_cursor(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    old_host: u32,
    source: ResourceId,
) {
    let Some(new_host) = state.resources.cursor_host_xid(source) else {
        return;
    };
    if old_host == new_host {
        return;
    }
    state.resources.retarget_cursor_host(old_host, source);
    let _ = backend.replace_cursor(origin, old_host, new_host);
    if !state.resources.cursor_host_referenced(old_host) {
        state.resources.cursor_host_released(old_host);
        let _ = backend.free_cursor(origin, old_host);
    }
}

/// Xorg `ExpandRegion` (`xfixes/region.c`): grow each rectangle of
/// `source` by the four margins and union the results. Coordinates are
/// computed wide and clamped to the protocol's 16-bit range.
fn xfixes_expand_region_rects(
    source: &[yserver_protocol::x11::xfixes::RegionRect],
    left: u16,
    right: u16,
    top: u16,
    bottom: u16,
) -> Vec<yserver_protocol::x11::xfixes::RegionRect> {
    use yserver_protocol::x11::xfixes::RegionRect;
    let clamp_i16 = |v: i32| {
        i16::try_from(v.clamp(i32::from(i16::MIN), i32::from(i16::MAX))).unwrap_or_default()
    };
    let grown: Vec<RegionRect> = source
        .iter()
        .map(|r| {
            let x1 = i32::from(r.x) - i32::from(left);
            let y1 = i32::from(r.y) - i32::from(top);
            let x2 = i32::from(r.x) + i32::from(r.width) + i32::from(right);
            let y2 = i32::from(r.y) + i32::from(r.height) + i32::from(bottom);
            let (x1, y1, x2, y2) = (clamp_i16(x1), clamp_i16(y1), clamp_i16(x2), clamp_i16(y2));
            RegionRect {
                x: x1,
                y: y1,
                width: u16::try_from(i32::from(x2) - i32::from(x1)).unwrap_or(0),
                height: u16::try_from(i32::from(y2) - i32::from(y1)).unwrap_or(0),
            }
        })
        .collect();
    grown.iter().fold(Vec::new(), |acc, rect| {
        crate::nested::union_regions(&acc, &[*rect])
    })
}

/// Send XFIXES `DisplayCursorNotify` for a backend-reported sprite change
/// to every (client, window) that selected it — one event per selection,
/// as Xorg's `CursorDisplayCursor` walks its `cursorEvents` list. Called
/// after each request and at the end of each loop iteration.
pub(crate) fn emit_xfixes_cursor_notify(state: &mut ServerState, backend: &mut dyn Backend) {
    use yserver_protocol::x11::xfixes as x11xfixes;
    let Some(change) = backend.take_displayed_cursor_change() else {
        return;
    };
    let mut selections: Vec<(u32, u32)> = state
        .xfixes_cursor_masks
        .iter()
        .filter(|(_, mask)| **mask & x11xfixes::DISPLAY_CURSOR_NOTIFY_MASK != 0)
        .map(|((client, window), _)| (*client, window.0))
        .collect();
    if selections.is_empty() {
        return;
    }
    selections.sort_unstable();
    let name = state
        .resources
        .cursor_name_for_host(change.host_xid)
        .map_or(0, |atom| atom.0);
    let timestamp = state.timestamp_now();
    for (client, window) in selections {
        let _dropped = fanout_event_to_clients(state, &[ClientId(client)], |buf, seq, order| {
            x11xfixes::encode_cursor_notify_event(
                buf,
                order,
                crate::nested::XFIXES_FIRST_EVENT,
                seq,
                window,
                change.serial,
                timestamp,
                name,
            );
        });
    }
}

/// Drop `client`'s XFIXES per-client state on disconnect. Xorg frees the
/// client's `CursorHideCountRec` resource, which re-displays the sprite
/// once no other client holds a hide.
pub(crate) fn release_xfixes_client_state(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client: ClientId,
) {
    state.xfixes_client_major.remove(&client.0);
    if state.xfixes_cursor_hide_counts.remove(&client.0).is_some()
        && state.xfixes_cursor_hide_counts.is_empty()
    {
        backend.set_cursor_hidden(false);
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn handle_xfixes_request(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    use yserver_protocol::x11::{ClientByteOrder, xfixes as x11xfixes};
    let byte_order = state
        .clients
        .get(&client_id.0)
        .map_or(ClientByteOrder::LittleEndian, |c| c.byte_order);
    let minor = header.data;
    match minor {
        x11xfixes::QUERY_VERSION => {
            // Xorg `ProcXFixesQueryVersion`: REQUEST_SIZE_MATCH, then the
            // lower-of-the-two rule with a sticky per-client major.
            let Some((client_major, client_minor)) =
                x11xfixes::parse_query_version(byte_order, body)
            else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_LENGTH,
                    0,
                    u16::from(minor),
                    header.opcode,
                );
            };
            let previous = state
                .xfixes_client_major
                .get(&client_id.0)
                .copied()
                .unwrap_or(0);
            let negotiated = x11xfixes::negotiate_version(previous, client_major, client_minor);
            state
                .xfixes_client_major
                .insert(client_id.0, negotiated.client_major);
            debug!(
                "client {} #{} XFIXES::QueryVersion client={client_major}.{client_minor} -> {}.{}",
                client_id.0, sequence.0, negotiated.reply_major, negotiated.reply_minor,
            );
            let reply = x11xfixes::encode_query_version_reply(
                byte_order,
                sequence,
                negotiated.reply_major,
                negotiated.reply_minor,
            );
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            return Ok(write_to_client(client, client_id, &reply));
        }
        x11xfixes::SELECT_SELECTION_INPUT => {
            if let Some(req) = x11xfixes::parse_select_selection_input(body) {
                // Xorg `ProcXFixesSelectSelectionInput`
                // (xfixes/select.c:189-196): validate the window
                // first, then the mask, before mutating state.
                if state.resources.window(ResourceId(req.window)).is_none() {
                    return emit_x11_error(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_WINDOW,
                        req.window,
                        XFIXES_MAJOR_OPCODE,
                    );
                }
                if req.event_mask & !x11xfixes::SELECTION_ALL_EVENTS_MASK != 0 {
                    return emit_x11_error(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_VALUE,
                        req.event_mask,
                        XFIXES_MAJOR_OPCODE,
                    );
                }
                let key = (
                    client_id.0,
                    ResourceId(req.window),
                    yserver_protocol::x11::AtomId(req.selection),
                );
                if req.event_mask == 0 {
                    state.xfixes_selection_masks.remove(&key);
                } else {
                    state.xfixes_selection_masks.insert(key, req.event_mask);
                }
            }
        }
        x11xfixes::SELECT_CURSOR_INPUT => {
            if let Some(req) = x11xfixes::parse_select_cursor_input(body) {
                // Xorg `ProcXFixesSelectCursorInput`: window first, then
                // the mask (captured on Xvfb: BadWindow, BadValue).
                if state.resources.window(ResourceId(req.window)).is_none() {
                    return emit_x11_error_with_minor(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_WINDOW,
                        req.window,
                        u16::from(minor),
                        header.opcode,
                    );
                }
                if req.event_mask & !x11xfixes::CURSOR_ALL_EVENTS_MASK != 0 {
                    return emit_x11_error_with_minor(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_VALUE,
                        req.event_mask,
                        u16::from(minor),
                        header.opcode,
                    );
                }
                let key = (client_id.0, ResourceId(req.window));
                if req.event_mask == 0 {
                    state.xfixes_cursor_masks.remove(&key);
                } else {
                    state.xfixes_cursor_masks.insert(key, req.event_mask);
                }
            }
        }
        x11xfixes::GET_CURSOR_IMAGE => {
            // Stage 5 unblock for audit #14: source the active
            // cursor from the backend. Pre-Stage-5 backends
            // (`ynest`, `RecordingBackend`) return `None` — fall
            // back to the empty reply so existing client behaviour
            // doesn't regress (a 0×0 reply is still a valid X11
            // GetCursorImage response).
            let reply = match backend.get_active_cursor_image() {
                Some(img) => x11xfixes::encode_get_cursor_image_reply(
                    byte_order,
                    sequence,
                    img.x,
                    img.y,
                    img.width,
                    img.height,
                    img.hot_x,
                    img.hot_y,
                    img.serial,
                    img.bgra_bytes.as_ref(),
                ),
                None => x11xfixes::encode_get_cursor_image_empty_reply(byte_order, sequence),
            };
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            let _byte_order = client.byte_order;
            return Ok(write_to_client(client, client_id, &reply));
        }
        x11xfixes::GET_CURSOR_IMAGE_AND_NAME => {
            // Superset of GetCursorImage (opcode 25). Real X screen
            // recorders (gpu-screen-recorder, …) call this to grab the
            // cursor; with no reply they block forever in poll(). The
            // name is the displayed cursor's XFIXES name (Xorg
            // `pCursor->name`), empty when it was never named.
            let reply = match backend.get_active_cursor_image() {
                Some(img) => {
                    let atom = state
                        .resources
                        .cursor_name_for_host(img.host_xid)
                        .unwrap_or(AtomId(0));
                    let name = state.atoms.name(atom).map(str::as_bytes).unwrap_or(&[]);
                    x11xfixes::encode_get_cursor_image_and_name_reply(
                        byte_order,
                        sequence,
                        img.x,
                        img.y,
                        img.width,
                        img.height,
                        img.hot_x,
                        img.hot_y,
                        img.serial,
                        atom.0,
                        name,
                        img.bgra_bytes.as_ref(),
                    )
                }
                None => {
                    x11xfixes::encode_get_cursor_image_and_name_empty_reply(byte_order, sequence)
                }
            };
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            return Ok(write_to_client(client, client_id, &reply));
        }
        x11xfixes::CREATE_REGION => {
            if let Some((region, rects)) = x11xfixes::parse_create_region(body) {
                if xfixes_region_xid_already_taken(state, region)
                    || xid_out_of_client_range(state, client_id, region)
                {
                    return emit_x11_error(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_ID_CHOICE,
                        region,
                        XFIXES_MAJOR_OPCODE,
                    );
                }
                state.xfixes_regions.insert(
                    region,
                    crate::server::XFixesRegion {
                        owner: client_id,
                        rects: normalize_region_rects(rects),
                    },
                );
            }
        }
        x11xfixes::CREATE_POINTER_BARRIER => {
            if let Some(req) = x11xfixes::parse_create_pointer_barrier(body) {
                // Xorg XICreatePointerBarrier validation order:
                // geometry -> negative-on-own-axis -> window -> devices -> xid.
                let horizontal = req.y1 == req.y2;
                let vertical = req.x1 == req.x2;
                if horizontal == vertical {
                    return emit_x11_error(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_VALUE,
                        req.barrier,
                        XFIXES_MAJOR_OPCODE,
                    );
                }
                if (horizontal && (req.y1 < 0 || req.y2 < 0))
                    || (vertical && (req.x1 < 0 || req.x2 < 0))
                {
                    return emit_x11_error(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_VALUE,
                        req.barrier,
                        XFIXES_MAJOR_OPCODE,
                    );
                }
                if state.resources.window(ResourceId(req.window)).is_none() {
                    return emit_x11_error(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_WINDOW,
                        req.window,
                        XFIXES_MAJOR_OPCODE,
                    );
                }
                for &d in &req.devices {
                    if !(d == 0 || d == 1 || d == 2) {
                        return emit_x11_error_with_minor(
                            state,
                            client_id,
                            sequence,
                            XI2_FIRST_ERROR,
                            u32::from(d),
                            u16::from(x11xfixes::CREATE_POINTER_BARRIER),
                            XFIXES_MAJOR_OPCODE,
                        );
                    }
                }
                if state.xid_occupied(req.barrier) {
                    return emit_x11_error(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_ALLOC,
                        req.barrier,
                        XFIXES_MAJOR_OPCODE,
                    );
                }
                if xid_out_of_client_range(state, client_id, req.barrier) {
                    return emit_x11_error(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_ID_CHOICE,
                        req.barrier,
                        XFIXES_MAJOR_OPCODE,
                    );
                }
                let (mut x1, mut x2) = (req.x1, req.x2);
                let (mut y1, mut y2) = (req.y1, req.y2);
                if x1 >= 0 && x2 >= 0 && x1 > x2 {
                    std::mem::swap(&mut x1, &mut x2);
                }
                if y1 >= 0 && y2 >= 0 && y1 > y2 {
                    std::mem::swap(&mut y1, &mut y2);
                }
                let directions = if horizontal {
                    req.directions & !(1 | 4)
                } else {
                    req.directions & !(2 | 8)
                };
                state.pointer_barriers.insert(
                    req.barrier,
                    crate::server::PointerBarrier {
                        owner: client_id,
                        window: ResourceId(req.window),
                        x1,
                        y1,
                        x2,
                        y2,
                        directions,
                        devices: req.devices,
                        hit: false,
                        seen: false,
                        event_id: 1,
                        release_event_id: 0,
                        last_timestamp: 0,
                    },
                );
                log::trace!(
                    target: "yserver_core::barriers",
                    "create barrier xid=0x{:x} window=0x{:x} ({x1},{y1})-({x2},{y2}) dirs={directions} -> {} active",
                    req.barrier,
                    req.window,
                    state.pointer_barriers.len(),
                );
            }
        }
        x11xfixes::DELETE_POINTER_BARRIER => {
            if let Some(bid) = x11xfixes::parse_delete_pointer_barrier(body) {
                match state.pointer_barriers.get(&bid) {
                    None => {
                        return emit_x11_error(
                            state,
                            client_id,
                            sequence,
                            x11::error::BAD_VALUE,
                            bid,
                            XFIXES_MAJOR_OPCODE,
                        );
                    }
                    Some(b) if b.owner != client_id => {
                        return emit_x11_error(
                            state,
                            client_id,
                            sequence,
                            x11::error::BAD_ACCESS,
                            bid,
                            XFIXES_MAJOR_OPCODE,
                        );
                    }
                    Some(barrier) => {
                        if barrier.hit {
                            // Xorg BarrierFreeBarrier emits the released leave
                            // with the CURRENT time + sprite position (not the
                            // last-hit values), xibarriers.c:668. Copy the
                            // barrier fields out first to release the borrow.
                            let (owner, window, eid) =
                                (barrier.owner, barrier.window, barrier.event_id);
                            let time = state.timestamp_now();
                            let (rx, ry) = state.pointer_root;
                            let _dropped = crate::core_loop::pointer_fanout::emit_barrier_event(
                                state,
                                bid,
                                owner,
                                window,
                                26,
                                time,
                                eid,
                                0,
                                1,
                                0,
                                i32::from(rx),
                                i32::from(ry),
                                0.0,
                                0.0,
                            );
                        }
                        state.pointer_barriers.remove(&bid);
                    }
                }
            }
        }
        x11xfixes::CREATE_REGION_FROM_BITMAP | x11xfixes::CREATE_REGION_FROM_GC => {
            if let Some((region, source)) = x11xfixes::parse_u32_pair(body) {
                if xfixes_region_xid_already_taken(state, region)
                    || xid_out_of_client_range(state, client_id, region)
                {
                    return emit_x11_error(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_ID_CHOICE,
                        region,
                        XFIXES_MAJOR_OPCODE,
                    );
                }
                if minor == x11xfixes::CREATE_REGION_FROM_GC {
                    let gc = ResourceId(source);
                    let Some(g) = state.resources.gc(gc) else {
                        return emit_x11_error(
                            state,
                            client_id,
                            sequence,
                            x11::error::BAD_GC,
                            source,
                            XFIXES_MAJOR_OPCODE,
                        );
                    };
                    let Some(clip) = g.clip_rectangles.clone() else {
                        return emit_x11_error(
                            state,
                            client_id,
                            sequence,
                            x11::error::BAD_MATCH,
                            source,
                            XFIXES_MAJOR_OPCODE,
                        );
                    };
                    // Audit #7 (docs/protocol-audit-2026-05-19.md):
                    // copy the GC's clip rects RAW — Xorg's
                    // `ProcXFixesCreateRegionFromGC`
                    // (`xfixes/region.c:219-226`) does
                    // `XFixesRegionCopy(pGC->clientClip)` with no
                    // translation. `clip.x_origin` / `clip.y_origin`
                    // are properties of the GC's USE, not part of
                    // the region's content; otherwise a subsequent
                    // `SetGcClipRegion(gc, region, x_origin,
                    // y_origin)` double-translates.
                    let rects = x11xfixes::parse_rectangles(&clip.rectangles);
                    state.xfixes_regions.insert(
                        region,
                        crate::server::XFixesRegion {
                            owner: client_id,
                            rects,
                        },
                    );
                } else {
                    state.xfixes_regions.insert(
                        region,
                        crate::server::XFixesRegion {
                            owner: client_id,
                            rects: Vec::new(),
                        },
                    );
                }
            }
        }
        x11xfixes::CREATE_REGION_FROM_WINDOW => {
            if let Some((region, window, kind)) = x11xfixes::parse_create_region_from_window(body) {
                if xfixes_region_xid_already_taken(state, region)
                    || xid_out_of_client_range(state, client_id, region)
                {
                    return emit_x11_error(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_ID_CHOICE,
                        region,
                        XFIXES_MAJOR_OPCODE,
                    );
                }
                // BadWindow on unknown window xid
                // (`xfixes/region.c:158-163`) — Xorg sets
                // `client->error_value = stuff->window`.
                if state.resources.window(ResourceId(window)).is_none() {
                    return emit_x11_error(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_WINDOW,
                        window,
                        XFIXES_MAJOR_OPCODE,
                    );
                }
                // BadValue on `kind` outside {Bounding=0, Clip=1}
                // (`xfixes/region.c:164-181`). The SHAPE-shaped
                // Input(2) is explicitly rejected by Xorg here.
                if kind > 1 {
                    return emit_x11_error(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_VALUE,
                        u32::from(kind),
                        XFIXES_MAJOR_OPCODE,
                    );
                }
                let rects = crate::nested::shape_rects_for(state, ResourceId(window), kind);
                trace!(
                    target: "yserver::xfixes::region",
                    "CreateRegionFromWindow region=0x{region:08x} window=0x{window:08x} kind={} rects{}",
                    kind,
                    format_region_rects(&rects),
                );
                state.xfixes_regions.insert(
                    region,
                    crate::server::XFixesRegion {
                        owner: client_id,
                        rects,
                    },
                );
            }
        }
        x11xfixes::CREATE_REGION_FROM_PICTURE => {
            if let Some((region, picture)) = x11xfixes::parse_create_region_from_picture(body) {
                if xfixes_region_xid_already_taken(state, region)
                    || xid_out_of_client_range(state, client_id, region)
                {
                    return emit_x11_error(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_ID_CHOICE,
                        region,
                        XFIXES_MAJOR_OPCODE,
                    );
                }
                // VERIFY_PICTURE (`xfixes/region.c:255`) returns
                // `RenderErrBase + BadPicture` on an unknown
                // picture xid — NOT BadDrawable.
                let bad_picture = crate::nested::RENDER_FIRST_ERROR + 1;
                let Some(picture_state) = state.resources.picture(ResourceId(picture)) else {
                    return emit_x11_error(
                        state,
                        client_id,
                        sequence,
                        bad_picture,
                        picture,
                        XFIXES_MAJOR_OPCODE,
                    );
                };
                // `if (!pPicture->pDrawable) return RenderErrBase +
                // BadPicture` at `xfixes/region.c:257-258`. yserver's
                // `PictureKind::Sourceless` (SolidFill / gradient
                // pictures) is exactly that case.
                if matches!(
                    picture_state.kind,
                    crate::resources::PictureKind::Sourceless
                ) {
                    return emit_x11_error(
                        state,
                        client_id,
                        sequence,
                        bad_picture,
                        picture,
                        XFIXES_MAJOR_OPCODE,
                    );
                }
                // An unbacked Picture has no clip set, like a fresh one.
                let client_clip = match picture_state.host_picture_xid {
                    Some(hp) => backend.picture_client_clip_rects(hp.as_raw()),
                    None => Some(None),
                };
                let Some(client_clip) = client_clip else {
                    return emit_x11_error(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_MATCH,
                        picture,
                        XFIXES_MAJOR_OPCODE,
                    );
                };
                let Some(rects) = client_clip else {
                    return emit_x11_error(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_MATCH,
                        picture,
                        XFIXES_MAJOR_OPCODE,
                    );
                };
                state.xfixes_regions.insert(
                    region,
                    crate::server::XFixesRegion {
                        owner: client_id,
                        rects: normalize_region_rects(rects),
                    },
                );
            }
        }
        x11xfixes::DESTROY_REGION => {
            if let Some((region, _)) = x11xfixes::parse_u32_pair(body) {
                state.xfixes_regions.remove(&region);
            } else if body.len() >= 4 {
                let region = u32::from_le_bytes(body[0..4].try_into().unwrap());
                state.xfixes_regions.remove(&region);
            }
        }
        x11xfixes::SET_REGION => {
            if let Some((region, rects)) = x11xfixes::parse_create_region(body) {
                state
                    .xfixes_regions
                    .entry(region)
                    .and_modify(|r| r.rects = normalize_region_rects(rects.clone()))
                    .or_insert_with(|| crate::server::XFixesRegion {
                        owner: client_id,
                        rects: normalize_region_rects(rects),
                    });
            }
        }
        x11xfixes::COPY_REGION => {
            if let Some((source, dest)) = x11xfixes::parse_u32_pair(body) {
                let rects = state
                    .xfixes_regions
                    .get(&source)
                    .map(|r| r.rects.clone())
                    .unwrap_or_default();
                trace!(
                    target: "yserver::xfixes::region",
                    "CopyRegion src=0x{source:08x} dst=0x{dest:08x} rects{}",
                    format_region_rects(&rects),
                );
                state.xfixes_regions.insert(
                    dest,
                    crate::server::XFixesRegion {
                        owner: client_id,
                        rects,
                    },
                );
            }
        }
        x11xfixes::UNION_REGION | x11xfixes::INTERSECT_REGION | x11xfixes::SUBTRACT_REGION => {
            if let Some((source1, source2, dest)) = x11xfixes::parse_u32_triplet(body) {
                let a = state
                    .xfixes_regions
                    .get(&source1)
                    .map(|r| r.rects.clone())
                    .unwrap_or_default();
                let b = state
                    .xfixes_regions
                    .get(&source2)
                    .map(|r| r.rects.clone())
                    .unwrap_or_default();
                let op_name = match minor {
                    x11xfixes::UNION_REGION => "UnionRegion",
                    x11xfixes::INTERSECT_REGION => "IntersectRegion",
                    x11xfixes::SUBTRACT_REGION => "SubtractRegion",
                    _ => unreachable!(),
                };
                let rects = match minor {
                    x11xfixes::UNION_REGION => crate::nested::union_regions(&a, &b),
                    x11xfixes::INTERSECT_REGION => crate::nested::intersect_regions(&a, &b),
                    x11xfixes::SUBTRACT_REGION => {
                        // Real rect-band subtraction (a - b). The
                        // previous implementation collapsed any
                        // overlap to an empty result — wrong for
                        // any compositor that builds a wallpaper
                        // clip via SubtractRegion(screen, windows).
                        crate::nested::subtract_regions(&a, &b)
                    }
                    _ => unreachable!(),
                };
                trace!(
                    target: "yserver::xfixes::region",
                    "{op_name} a=0x{source1:08x} rects{} b=0x{source2:08x} rects{} dst=0x{dest:08x} rects{}",
                    format_region_rects(&a),
                    format_region_rects(&b),
                    format_region_rects(&rects),
                );
                state.xfixes_regions.insert(
                    dest,
                    crate::server::XFixesRegion {
                        owner: client_id,
                        rects,
                    },
                );
            }
        }
        x11xfixes::INVERT_REGION => {
            if let Some((source, bounds, dest)) = x11xfixes::parse_invert_region(body) {
                let source_rects = state
                    .xfixes_regions
                    .get(&source)
                    .map(|r| r.rects.clone())
                    .unwrap_or_default();
                state.xfixes_regions.insert(
                    dest,
                    crate::server::XFixesRegion {
                        owner: client_id,
                        rects: crate::nested::subtract_regions(
                            &normalize_region_rects(vec![bounds]),
                            &source_rects,
                        ),
                    },
                );
            }
        }
        x11xfixes::TRANSLATE_REGION => {
            if let Some((region_id, dx, dy)) = x11xfixes::parse_translate_region(body)
                && let Some(region) = state.xfixes_regions.get_mut(&region_id)
            {
                let before = region.rects.clone();
                crate::nested::translate_region(&mut region.rects, dx, dy);
                trace!(
                    target: "yserver::xfixes::region",
                    "TranslateRegion region=0x{region_id:08x} delta=({dx}, {dy}) before{} after{}",
                    format_region_rects(&before),
                    format_region_rects(&region.rects),
                );
            }
        }
        x11xfixes::REGION_EXTENTS => {
            if let Some((source, dest)) = x11xfixes::parse_u32_pair(body) {
                let rect = state
                    .xfixes_regions
                    .get(&source)
                    .map(|r| crate::nested::region_extents(&r.rects))
                    .unwrap_or(x11xfixes::RegionRect {
                        x: 0,
                        y: 0,
                        width: 0,
                        height: 0,
                    });
                state.xfixes_regions.insert(
                    dest,
                    crate::server::XFixesRegion {
                        owner: client_id,
                        rects: normalize_region_rects(vec![rect]),
                    },
                );
            }
        }
        x11xfixes::FETCH_REGION => {
            let region = body
                .get(0..4)
                .map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap()))
                .unwrap_or(0);
            // A fetched damage region can include coalesced paint that did
            // not generate another DamageNotify. Submit it before the reply
            // lets an external compositor sample the corresponding pixels.
            backend.flush_before_damage_notify();
            let rects = state
                .xfixes_regions
                .get(&region)
                .map(|r| r.rects.clone())
                .unwrap_or_default();
            let extents = crate::nested::region_extents(&rects);
            let reply = x11xfixes::encode_fetch_region_reply(byte_order, sequence, extents, &rects);
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            let _byte_order = client.byte_order;
            return Ok(write_to_client(client, client_id, &reply));
        }
        x11xfixes::CHANGE_SAVE_SET => {
            if let Some(req) = x11xfixes::parse_change_save_set(body)
                && let Some(c) = state.clients.get_mut(&client_id.0)
            {
                let win = ResourceId(req.window);
                match req.mode {
                    0 => {
                        c.save_set.insert(win);
                    }
                    1 => {
                        c.save_set.remove(&win);
                    }
                    _ => {}
                }
                // target (Nearest/Root) and map (Map/Unmap) parameters affect
                // disconnect-time reparent/remap behaviour; that path is
                // already deferred from the Phase 2 wrap-up. Ignore for now.
            }
        }
        x11xfixes::SET_GC_CLIP_REGION => {
            if let Some(req) = x11xfixes::parse_set_gc_clip_region(body) {
                let gc_id = ResourceId(req.gc);
                // A clip-mask pixmap this displaces is released as by
                // SetClipRectangles (`ChangeClip`, `dix/gc.c`).
                let displaced = if req.region == 0 {
                    let displaced = state.resources.clear_gc_clip(gc_id);
                    state
                        .resources
                        .set_gc_clip_origin(gc_id, req.x_origin, req.y_origin);
                    displaced
                } else {
                    let rects = state
                        .xfixes_regions
                        .get(&req.region)
                        .map(|r| r.rects.clone())
                        .unwrap_or_default();
                    let mut rectangles = Vec::with_capacity(rects.len() * 8);
                    for rect in &rects {
                        rectangles.extend_from_slice(&rect.x.to_le_bytes());
                        rectangles.extend_from_slice(&rect.y.to_le_bytes());
                        rectangles.extend_from_slice(&rect.width.to_le_bytes());
                        rectangles.extend_from_slice(&rect.height.to_le_bytes());
                    }
                    state.resources.set_clip_rectangles(
                        client_id,
                        yserver_protocol::x11::SetClipRectanglesRequest {
                            gc: gc_id,
                            clip: yserver_protocol::x11::ClipRectangles {
                                ordering: 0, // Unsorted; matches xfixes region semantics.
                                x_origin: req.x_origin,
                                y_origin: req.y_origin,
                                rectangles,
                            },
                        },
                    )
                };
                release_displaced_gc_pixmaps(state, backend, origin, displaced);
            }
        }
        x11xfixes::SET_WINDOW_SHAPE_REGION => {
            if let Some(req) = x11xfixes::parse_set_window_shape_region(body) {
                let window = ResourceId(req.dest);
                if req.region == 0 {
                    crate::nested::clear_shape_rects(state, window, req.dest_kind);
                } else {
                    let rects = state
                        .xfixes_regions
                        .get(&req.region)
                        .map(|r| r.rects.clone())
                        .unwrap_or_default();
                    let source = crate::nested::offset_rects(rects, req.x_offset, req.y_offset);
                    crate::nested::set_shape_rects(state, window, req.dest_kind, source);
                }
                mirror_shape_to_host_state(state, backend, origin, window, req.dest_kind);
                // Xorg SetWindowShapeRegion goes through miSetShape too.
                backend.windows_restructured(state);
            }
        }
        x11xfixes::SET_PICTURE_CLIP_REGION => {
            if let Some(req) = x11xfixes::parse_set_picture_clip_region(body) {
                let pic_id = ResourceId(req.picture);
                let host_pic = state
                    .resources
                    .picture(pic_id)
                    .and_then(|p| p.host_picture_xid)
                    .map(|h| h.as_raw());
                if let Some(hp) = host_pic {
                    if req.region == 0 {
                        // RENDER CPClipMask = 0x40; value=None clears the clip.
                        let mut out = Vec::with_capacity(12);
                        out.extend_from_slice(&req.picture.to_le_bytes());
                        out.extend_from_slice(&0x40_u32.to_le_bytes());
                        out.extend_from_slice(&0_u32.to_le_bytes());
                        let _ = backend.render_change_picture(origin, hp, &out);
                    } else {
                        let rects = state
                            .xfixes_regions
                            .get(&req.region)
                            .map(|r| r.rects.clone())
                            .unwrap_or_default();
                        trace!(
                            target: "yserver::xfixes::clip",
                            "SetPictureClipRegion client_pic=0x{:08x} host_pic=0x{hp:08x} region=0x{:08x} origin=({}, {}) n={} rects{}",
                            req.picture,
                            req.region,
                            req.x_origin,
                            req.y_origin,
                            rects.len(),
                            format_region_rects(&rects),
                        );
                        // RENDER SetPictureClipRectangles body layout:
                        // picture(4) | x_origin(2) | y_origin(2) | rects(8*N).
                        let mut out = Vec::with_capacity(8 + rects.len() * 8);
                        out.extend_from_slice(&req.picture.to_le_bytes());
                        out.extend_from_slice(&req.x_origin.to_le_bytes());
                        out.extend_from_slice(&req.y_origin.to_le_bytes());
                        for rect in &rects {
                            out.extend_from_slice(&rect.x.to_le_bytes());
                            out.extend_from_slice(&rect.y.to_le_bytes());
                            out.extend_from_slice(&rect.width.to_le_bytes());
                            out.extend_from_slice(&rect.height.to_le_bytes());
                        }
                        let _ = backend.render_set_picture_clip_rectangles(origin, hp, &out);
                    }
                }
            }
        }
        x11xfixes::CHANGE_CURSOR => {
            // Xorg `ProcXFixesChangeCursor`: both cursors must exist
            // (source checked first), then every use of `destination` —
            // its XIDs, window cursors, grabs — becomes `source`.
            let Some((source, destination)) = x11xfixes::parse_change_cursor(byte_order, body)
            else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_LENGTH,
                    0,
                    u16::from(minor),
                    header.opcode,
                );
            };
            if let Err(outcome) = xfixes_verify_cursor(state, client_id, sequence, header, source) {
                return outcome;
            }
            let dest_host =
                match xfixes_verify_cursor(state, client_id, sequence, header, destination) {
                    Ok(host) => host,
                    Err(outcome) => return outcome,
                };
            if let Some(dest_host) = dest_host {
                xfixes_replace_cursor(state, backend, origin, dest_host, ResourceId(source));
            }
        }
        x11xfixes::CHANGE_CURSOR_BY_NAME => {
            // Xorg `ProcXFixesChangeCursorByName`: the source must exist;
            // a name that was never interned matches nothing (MakeAtom
            // with create=FALSE) and succeeds silently.
            if let Some((cursor_xid, name_bytes)) = x11xfixes::parse_change_cursor_by_name(body) {
                if let Err(outcome) =
                    xfixes_verify_cursor(state, client_id, sequence, header, cursor_xid)
                {
                    return outcome;
                }
                let name = std::str::from_utf8(name_bytes).unwrap_or("");
                let atom = state.atoms.intern(name, true);
                if atom.0 != 0 {
                    for host in state.resources.cursor_hosts_named(atom) {
                        xfixes_replace_cursor(state, backend, origin, host, ResourceId(cursor_xid));
                    }
                }
            }
        }
        x11xfixes::EXPAND_REGION => {
            let Some(req) = x11xfixes::parse_expand_region(byte_order, body) else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_LENGTH,
                    0,
                    u16::from(minor),
                    header.opcode,
                );
            };
            // VERIFY_REGION source then destination: XFixes BadRegion
            // (error base + 0).
            for region in [req.source, req.destination] {
                if !state.xfixes_regions.contains_key(&region) {
                    return emit_x11_error_with_minor(
                        state,
                        client_id,
                        sequence,
                        crate::nested::XFIXES_FIRST_ERROR,
                        region,
                        u16::from(minor),
                        header.opcode,
                    );
                }
            }
            let source = state.xfixes_regions[&req.source].rects.clone();
            // An empty source leaves the destination untouched (Xorg only
            // rewrites it inside `if (nBoxes)`; confirmed on Xvfb).
            if !source.is_empty() {
                let rects =
                    xfixes_expand_region_rects(&source, req.left, req.right, req.top, req.bottom);
                trace!(
                    target: "yserver::xfixes::region",
                    "ExpandRegion src=0x{:08x} dst=0x{:08x} l={} r={} t={} b={} rects{}",
                    req.source,
                    req.destination,
                    req.left,
                    req.right,
                    req.top,
                    req.bottom,
                    format_region_rects(&rects),
                );
                if let Some(dest) = state.xfixes_regions.get_mut(&req.destination) {
                    dest.rects = rects;
                }
            }
        }
        x11xfixes::SET_CURSOR_NAME => {
            // SetCursorName(cursor, name) — XFixes 2.0: tag a cursor
            // with a name string. Xorg interns the name as an atom and
            // stores it on the cursor (`pCursor->name = atom`) so
            // GetCursorName can read it back. yserver mirrors the same
            // shape: intern, store on `Cursor.name_atom`.
            if let Some((cursor_xid, name_bytes)) = x11xfixes::parse_set_cursor_name(body) {
                if let Err(outcome) =
                    xfixes_verify_cursor(state, client_id, sequence, header, cursor_xid)
                {
                    return outcome;
                }
                let name = std::str::from_utf8(name_bytes).unwrap_or("");
                let atom = state.atoms.intern(name, false);
                state
                    .resources
                    .set_cursor_name_atom(ResourceId(cursor_xid), atom);
                debug!(
                    "client {} #{} XFIXES::SetCursorName cursor=0x{:x} atom={} \"{}\"",
                    client_id.0, sequence.0, cursor_xid, atom.0, name,
                );
            } else {
                debug!(
                    "client {} #{} XFIXES::SetCursorName (malformed body, ignored)",
                    client_id.0, sequence.0,
                );
            }
        }
        x11xfixes::GET_CURSOR_NAME => {
            // GetCursorName(cursor) — reply with atom + name string.
            // If no name was ever set, return atom=None(0) and an
            // empty name, matching Xorg's behaviour for cursors with
            // `pCursor->name == 0`.
            let cursor_xid = body
                .get(0..4)
                .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
                .unwrap_or(0);
            if let Err(outcome) =
                xfixes_verify_cursor(state, client_id, sequence, header, cursor_xid)
            {
                return outcome;
            }
            let atom = state
                .resources
                .cursor_name_atom(ResourceId(cursor_xid))
                .unwrap_or(AtomId(0));
            let name = state.atoms.name(atom).map(str::as_bytes).unwrap_or(&[]);
            debug!(
                "client {} #{} XFIXES::GetCursorName cursor=0x{:x} atom={} ({} bytes)",
                client_id.0,
                sequence.0,
                cursor_xid,
                atom.0,
                name.len(),
            );
            let reply = x11xfixes::encode_get_cursor_name_reply(byte_order, sequence, atom.0, name);
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            return Ok(write_to_client(client, client_id, &reply));
        }
        x11xfixes::HIDE_CURSOR | x11xfixes::SHOW_CURSOR => {
            // Xorg `ProcXFixesHideCursor` / `ProcXFixesShowCursor`: the
            // window only names the screen (one here); the hide count is
            // per client. The sprite hides on the first count anywhere and
            // returns when the last one goes (ShowCursor to zero, or the
            // client disconnecting).
            let Some(window) = x11xfixes::parse_window(byte_order, body) else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_LENGTH,
                    0,
                    u16::from(minor),
                    header.opcode,
                );
            };
            if state.resources.window(ResourceId(window)).is_none() {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_WINDOW,
                    window,
                    u16::from(minor),
                    header.opcode,
                );
            }
            if minor == x11xfixes::HIDE_CURSOR {
                let was_visible = state.xfixes_cursor_hide_counts.is_empty();
                *state
                    .xfixes_cursor_hide_counts
                    .entry(client_id.0)
                    .or_insert(0) += 1;
                if was_visible {
                    backend.set_cursor_hidden(true);
                }
            } else {
                // Showing without a prior hide is BadMatch (Xvfb: error 8,
                // value = the window).
                let Some(count) = state.xfixes_cursor_hide_counts.get_mut(&client_id.0) else {
                    return emit_x11_error_with_minor(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_MATCH,
                        window,
                        u16::from(minor),
                        header.opcode,
                    );
                };
                *count -= 1;
                if *count == 0 {
                    state.xfixes_cursor_hide_counts.remove(&client_id.0);
                    if state.xfixes_cursor_hide_counts.is_empty() {
                        backend.set_cursor_hidden(false);
                    }
                }
            }
            debug!(
                "client {} #{} XFIXES::{}Cursor window=0x{window:x} hide_counts={:?}",
                client_id.0,
                sequence.0,
                if minor == x11xfixes::HIDE_CURSOR {
                    "Hide"
                } else {
                    "Show"
                },
                state.xfixes_cursor_hide_counts,
            );
        }
        other if other > x11xfixes::DELETE_POINTER_BARRIER => {
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
        other => {
            // Every 5.0 request has an arm above; the gate in
            // `dispatch_xfixes_request` keeps higher minors out.
            debug!(
                "client {} #{} XFIXES::unexpected minor={}",
                client_id.0, sequence.0, other
            );
        }
    }
    Ok(RequestOutcome::Handled)
}

/// CreateColormap (78): allocate colormap, BadIDChoice on duplicate.
/// `BadIDChoice` fires when a CreateXxx request's resource ID is
/// either already allocated OR outside the client's resource-id range
/// declared in the setup reply.
/// `LEGAL_NEW_RESOURCE` equivalent for XFIXES `CreateRegion*`: the
/// candidate region xid must NOT already name a resource (in the
/// core `resources` table OR in the dedicated `xfixes_regions`
/// map). Xorg's `xfixes/region.c:78,114,157,213` all guard their
/// CreateRegion variants with `LEGAL_NEW_RESOURCE(stuff->region,
/// client)` before doing any work — duplicates surface as
/// BadIDChoice (X error code 14). Returns true if the xid clashes
/// (the caller should emit BadIDChoice).
fn xfixes_region_xid_already_taken(state: &ServerState, rid: u32) -> bool {
    state.xfixes_regions.contains_key(&rid) || state.resources.xid_in_use(ResourceId(rid))
}
