use super::*;

pub(super) fn handle_recolor_cursor(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    let cursor = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
    if !state.resources.cursor_exists(cursor) {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_CURSOR,
            cursor.0,
            96,
        );
    }
    let fore = (
        u16::from_le_bytes([body[4], body[5]]),
        u16::from_le_bytes([body[6], body[7]]),
        u16::from_le_bytes([body[8], body[9]]),
    );
    let back = (
        u16::from_le_bytes([body[10], body[11]]),
        u16::from_le_bytes([body[12], body[13]]),
        u16::from_le_bytes([body[14], body[15]]),
    );
    if let Some(host_xid) = state.resources.cursor_host_xid(cursor) {
        backend.recolor_cursor(origin, host_xid, fore, back)?;
    }
    debug!(
        "client {} #{} RecolorCursor 0x{:x}",
        client_id.0, sequence.0, cursor.0
    );
    Ok(RequestOutcome::Handled)
}

/// SetDashes (58): replace the GC's dash pattern with the multi-byte
/// list supplied. Distinct from the single-byte `CPDashList` form in
/// `CreateGC` / `ChangeGC` (which can only set a uniform [n, n]).
///
/// Wire: `gc u32, dash_offset u16, n u16, dashes u8[n]`. `n == 0` or
/// any `dashes[i] == 0` is `BadValue` per spec.
pub(super) fn handle_set_dashes(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    if body.len() < 8 {
        return Ok(RequestOutcome::Handled);
    }
    let gc = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
    let dash_offset = u16::from_le_bytes([body[4], body[5]]);
    let n = u16::from_le_bytes([body[6], body[7]]) as usize;
    debug!(
        "client {} #{} SetDashes gc=0x{:x} offset={} n={}",
        client_id.0, sequence.0, gc.0, dash_offset, n
    );
    if n == 0 {
        return emit_x11_error(state, client_id, sequence, x11::error::BAD_VALUE, 0, 58);
    }
    let end = 8usize.saturating_add(n);
    if body.len() < end {
        return Ok(RequestOutcome::Handled);
    }
    let dashes = &body[8..end];
    // BadGC takes priority over BadValue when the GC id itself is
    // bogus. Use the client-range check rather than `gc().is_some()`
    // because other GC mutators (`set_clip_rectangles`, etc.)
    // auto-vivify entries via `entry().or_insert_with`, so an
    // invalid id can be present in the map from an earlier request.
    if xid_out_of_client_range(state, client_id, gc.0) || state.resources.gc(gc).is_none() {
        return emit_x11_error(state, client_id, sequence, x11::error::BAD_GC, gc.0, 58);
    }
    if dashes.contains(&0) {
        return emit_x11_error(state, client_id, sequence, x11::error::BAD_VALUE, 0, 58);
    }
    state
        .resources
        .set_dashes(client_id, gc, dash_offset, dashes);
    Ok(RequestOutcome::Handled)
}

pub(super) fn handle_create_cursor(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    if body.len() >= 28 {
        let cursor_id = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
        if state.resources.xid_in_use(cursor_id)
            || xid_out_of_client_range(state, client_id, cursor_id.0)
        {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_ID_CHOICE,
                cursor_id.0,
                93,
            );
        }
        let source_id = ResourceId(u32::from_le_bytes([body[4], body[5], body[6], body[7]]));
        let mask_id = ResourceId(u32::from_le_bytes([body[8], body[9], body[10], body[11]]));
        let fore = (
            u16::from_le_bytes([body[12], body[13]]),
            u16::from_le_bytes([body[14], body[15]]),
            u16::from_le_bytes([body[16], body[17]]),
        );
        let back = (
            u16::from_le_bytes([body[18], body[19]]),
            u16::from_le_bytes([body[20], body[21]]),
            u16::from_le_bytes([body[22], body[23]]),
        );
        let hot_x = u16::from_le_bytes([body[24], body[25]]);
        let hot_y = u16::from_le_bytes([body[26], body[27]]);

        let src_host = state.resources.pixmap(source_id).and_then(|p| p.host_xid);
        let mask_host = if mask_id.0 == 0 {
            None
        } else {
            state.resources.pixmap(mask_id).and_then(|p| p.host_xid)
        };
        state.resources.create_cursor(client_id, cursor_id);
        if let Some(src_host) = src_host {
            match backend.create_cursor(origin, src_host, mask_host, fore, back, hot_x, hot_y) {
                Ok(handle) => {
                    state.resources.set_cursor_host_xid(cursor_id, handle);
                }
                Err(err) => {
                    log::warn!("client {} CreateCursor failed: {err}", client_id.0);
                }
            }
        }
    }
    debug!("client {} #{} CreateCursor", client_id.0, sequence.0);
    Ok(RequestOutcome::Handled)
}

pub(super) fn handle_create_glyph_cursor(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    if body.len() >= 28
        && let Some(cursor) = x11::create_glyph_cursor_id(body)
    {
        let new_id = cursor.0;
        let validation_failed = {
            let handle = state.clients.get(&client_id.0).expect("client registered");
            let owned = crate::server::IdAllocator::validate_owned(
                new_id,
                handle.resource_id_base,
                handle.resource_id_mask,
            );
            let in_use = state.xid_occupied(cursor.0);
            !owned || in_use
        };
        if validation_failed {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_ID_CHOICE,
                new_id,
                94,
            );
        }

        let source_id = ResourceId(u32::from_le_bytes([body[4], body[5], body[6], body[7]]));
        let mask_id = ResourceId(u32::from_le_bytes([body[8], body[9], body[10], body[11]]));
        let source_char = u16::from_le_bytes([body[12], body[13]]);
        let mask_char = u16::from_le_bytes([body[14], body[15]]);
        let fore = (
            u16::from_le_bytes([body[16], body[17]]),
            u16::from_le_bytes([body[18], body[19]]),
            u16::from_le_bytes([body[20], body[21]]),
        );
        let back = (
            u16::from_le_bytes([body[22], body[23]]),
            u16::from_le_bytes([body[24], body[25]]),
            u16::from_le_bytes([body[26], body[27]]),
        );

        let src_host = state.resources.font(source_id).map(|f| f.host_xid);
        let mask_host = if mask_id.0 == 0 {
            None
        } else {
            state.resources.font(mask_id).map(|f| f.host_xid)
        };

        state.resources.create_glyph_cursor(client_id, cursor);
        if let Some(src_host) = src_host {
            match backend.create_glyph_cursor(
                origin,
                src_host,
                mask_host,
                source_char,
                mask_char,
                fore,
                back,
            ) {
                Ok(handle) => {
                    state.resources.set_cursor_host_xid(cursor, handle);
                }
                Err(err) => {
                    log::warn!("client {} CreateGlyphCursor failed: {err}", client_id.0);
                }
            }
        } else {
            log::warn!(
                "client {} CreateGlyphCursor: source font 0x{:x} unknown",
                client_id.0,
                source_id.0
            );
        }
    }
    debug!("client {} #{} CreateGlyphCursor", client_id.0, sequence.0);
    Ok(RequestOutcome::Handled)
}

pub(super) fn handle_create_gc(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    if let Some(request) = x11::create_gc_request(body) {
        let new_id = request.gc.0;
        let validation_failed = {
            let handle = state.clients.get(&client_id.0).expect("client registered");
            let owned = crate::server::IdAllocator::validate_owned(
                new_id,
                handle.resource_id_base,
                handle.resource_id_mask,
            );
            let in_use = state.xid_occupied(request.gc.0);
            !owned || in_use
        };
        if validation_failed {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_ID_CHOICE,
                new_id,
                55,
            );
        }
        state.resources.create_gc(client_id, request);
    }
    debug!("client {} #{} CreateGC", client_id.0, sequence.0);
    Ok(RequestOutcome::Handled)
}

/// Host-free the pixmaps a GC change displaced once nothing else references
/// them — the client may have freed them while the GC still held them
/// (`handle_free_pixmap` defers that free). Same orphan rule as the
/// ChangeWindowAttributes release.
pub(super) fn release_displaced_gc_pixmaps(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    displaced: Vec<crate::backend::PixmapHandle>,
) {
    for handle in displaced {
        if !state.resources.host_xid_still_referenced(handle) {
            let _ = backend.free_pixmap(origin, handle.as_raw());
            state.resources.host_pixmap_freed(handle.as_raw());
        }
    }
}

pub(super) fn handle_change_gc(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    if let Some(request) = x11::change_gc_request(body) {
        if let Err((code, bad_value)) = validate_gc_only(state, request.gc) {
            return emit_x11_error(state, client_id, sequence, code, bad_value, 56);
        }
        let displaced = state.resources.change_gc(client_id, request);
        release_displaced_gc_pixmaps(state, backend, origin, displaced);
    }
    debug!("client {} #{} ChangeGC", client_id.0, sequence.0);
    Ok(RequestOutcome::Handled)
}

pub(super) fn handle_copy_gc(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    if body.len() >= 12 {
        let src_gc = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
        let dst_gc = ResourceId(u32::from_le_bytes([body[4], body[5], body[6], body[7]]));
        let value_mask = u32::from_le_bytes([body[8], body[9], body[10], body[11]]);
        let displaced = state.resources.copy_gc(src_gc, dst_gc, value_mask);
        release_displaced_gc_pixmaps(state, backend, origin, displaced);
    }
    debug!("client {} #{} CopyGC", client_id.0, sequence.0);
    Ok(RequestOutcome::Handled)
}

pub(super) fn handle_set_clip_rectangles(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    if let Some(request) = x11::set_clip_rectangles_request(header.data, body) {
        if let Err((code, bad_value)) = validate_gc_only(state, request.gc) {
            return emit_x11_error(state, client_id, sequence, code, bad_value, 59);
        }
        let displaced = state.resources.set_clip_rectangles(client_id, request);
        release_displaced_gc_pixmaps(state, backend, origin, displaced);
    }
    debug!("client {} #{} SetClipRectangles", client_id.0, sequence.0);
    Ok(RequestOutcome::Handled)
}

/// Xorg `ProcFreeGC` (`../xserver/dix/dispatch.c:1677`) resolves the id
/// with `dixLookupGC` -> `dixLookupResourceByType(X11_RESTYPE_GC)`, which
/// on a miss returns that resource type's `errorValue` — `BadGC`
/// (`../xserver/dix/resource.c:454`) — after setting `client->errorValue`
/// to the id exactly as sent. Silently succeeding is the same defect
/// class as #143 on FreePixmap: for a *checked void* request the error
/// packet is the only thing that can carry a higher sequence number back
/// to XCB, so swallowing it leaves every preceding checked request of
/// that client uncompleted.
pub(super) fn handle_free_gc(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    let Some(gc) = x11::free_resource_id(body) else {
        debug!(
            "client {} #{} FreeGC (parse failed)",
            client_id.0, sequence.0
        );
        return Ok(RequestOutcome::Handled);
    };
    if state.resources.gc(gc).is_none() {
        debug!(
            "client {} #{} FreeGC gc=0x{:x} unknown -> BadGC",
            client_id.0, sequence.0, gc.0
        );
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_GC,
            gc.0,
            FREE_GC_OPCODE,
        );
    }
    let displaced = state.resources.free_gc(gc);
    release_displaced_gc_pixmaps(state, backend, origin, displaced);
    debug!(
        "client {} #{} FreeGC gc=0x{:x} freed",
        client_id.0, sequence.0, gc.0
    );
    Ok(RequestOutcome::Handled)
}

/// Xorg `ProcFreeCursor` (`../xserver/dix/dispatch.c:3100`) looks the id
/// up with `dixLookupResourceByType(X11_RESTYPE_CURSOR)` and on a miss
/// sets `client->errorValue = stuff->id` and returns the type's
/// `errorValue`, `BadCursor` (`../xserver/dix/resource.c:466`). Same
/// checked-void-request reasoning as `handle_free_gc` above.
pub(super) fn handle_free_cursor(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    let Some(cursor) = x11::free_resource_id(body) else {
        debug!(
            "client {} #{} FreeCursor (parse failed)",
            client_id.0, sequence.0
        );
        return Ok(RequestOutcome::Handled);
    };
    if !state.resources.cursor_exists(cursor) {
        debug!(
            "client {} #{} FreeCursor cursor=0x{:x} unknown -> BadCursor",
            client_id.0, sequence.0, cursor.0
        );
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_CURSOR,
            cursor.0,
            FREE_CURSOR_OPCODE,
        );
    }
    let host_xid = state.resources.free_cursor(cursor);
    if let Some(host_xid) = host_xid {
        let _ = backend.free_cursor(origin, host_xid);
    }
    debug!(
        "client {} #{} FreeCursor cursor=0x{:x} freed host_xid={host_xid:?}",
        client_id.0, sequence.0, cursor.0
    );
    Ok(RequestOutcome::Handled)
}

pub(super) fn handle_create_pixmap(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    let Some(request) = x11::create_pixmap_request(header.data, body) else {
        debug!(
            "client {} #{} CreatePixmap (parse failed)",
            client_id.0, sequence.0
        );
        return Ok(RequestOutcome::Handled);
    };
    debug!(
        "client {} #{} CreatePixmap pid=0x{:x} depth={} {}x{} drawable=0x{:x}",
        client_id.0,
        sequence.0,
        request.pixmap.0,
        request.depth,
        request.width,
        request.height,
        request.drawable.0,
    );
    let new_id = request.pixmap.0;
    let (owned, in_use, drawable_exists) = {
        let handle = state.clients.get(&client_id.0).expect("client registered");
        let owned = crate::server::IdAllocator::validate_owned(
            new_id,
            handle.resource_id_base,
            handle.resource_id_mask,
        );
        let in_use = state.xid_occupied(request.pixmap.0);
        let drawable_exists = state.resources.window(request.drawable).is_some()
            || state.resources.pixmap(request.drawable).is_some();
        (owned, in_use, drawable_exists)
    };
    if !owned || in_use {
        log::warn!(
            "client {} CreatePixmap BadIDChoice pid=0x{:x} ({})",
            client_id.0,
            new_id,
            if !owned { "out-of-range" } else { "in-use" },
        );
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_ID_CHOICE,
            new_id,
            53,
        );
    }
    if !drawable_exists {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_DRAWABLE,
            request.drawable.0,
            53,
        );
    }
    if !supported_pixmap_depth(request.depth) {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_VALUE,
            u32::from(request.depth),
            53,
        );
    }
    let host_xid = match backend.create_pixmap(origin, request.depth, request.width, request.height)
    {
        Ok(handle) => Some(handle),
        Err(err) => {
            log::warn!("client {} host CreatePixmap failed: {err}", client_id.0);
            None
        }
    };
    state.resources.create_pixmap(client_id, request);
    if let Some(xid) = host_xid {
        let updated = state.resources.set_pixmap_host_xid(request.pixmap, xid);
        debug_assert!(updated, "pixmap was just inserted above");
    }
    Ok(RequestOutcome::Handled)
}

/// Xorg `ProcFreePixmap` (`../xserver/dix/dispatch.c:1529`) resolves the
/// id with `dixLookupResourceByType(X11_RESTYPE_PIXMAP)`; on a miss it
/// sets `client->errorValue = stuff->id` and returns that resource
/// type's `errorValue`, i.e. `BadPixmap` (`../xserver/dix/resource.c:448`)
/// — `None` (0) included, since 0 is simply an XID that is not a pixmap.
///
/// #143: picom sends a deliberate `FreePixmap(drawable=None)` as a sync
/// barrier before every sleep (`x_prepare_for_sleep`, picom `src/x.c:1194`)
/// and *expects* `BadPixmap` back. XCB can only mark a checked **void**
/// request complete once it reads a packet carrying a higher sequence
/// number, and for a void request that packet is the error. Returning
/// silent success left picom's nine checked `ChangeWindowAttributes`
/// uncompleted, so `wm_handle_set_event_mask_reply` never fired and the
/// second half of its window import stalled ~10s (Xorg: ~120ms).
pub(super) fn handle_free_pixmap(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    let Some(pixmap) = x11::free_resource_id(body) else {
        debug!(
            "client {} #{} FreePixmap (parse failed)",
            client_id.0, sequence.0
        );
        return Ok(RequestOutcome::Handled);
    };
    let Some(removed) = state.resources.free_pixmap(pixmap) else {
        debug!(
            "client {} #{} FreePixmap pixmap=0x{:x} unknown -> BadPixmap",
            client_id.0, sequence.0, pixmap.0
        );
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_PIXMAP,
            pixmap.0,
            FREE_PIXMAP_OPCODE,
        );
    };
    if removed.composite_name {
        // A name owns its own alias ref: a sibling name sharing the backing must not hide it.
        let names: Vec<_> = removed.host_xid.into_iter().collect();
        let last = crate::core_loop::process_disconnect::release_removed_names(
            state, backend, origin, &names,
        );
        crate::core_loop::process_disconnect::free_orphaned_host_pixmaps(
            state,
            backend,
            last.clone(),
            &last,
            None,
        );
        debug!(
            "client {} #{} FreePixmap pixmap=0x{:x} (window name) host_xid={:?}",
            client_id.0,
            sequence.0,
            pixmap.0,
            removed.host_xid.map(crate::backend::PixmapHandle::as_raw),
        );
        return Ok(RequestOutcome::Handled);
    }
    let still_referenced = removed
        .host_xid
        // The BORDER reference is as load-bearing as the background one:
        // `XCreatePixmap` → `XSetWindowBorderPixmap` → `XFreePixmap` is
        // ordinary client code, and X11 keeps the storage alive because
        // the window still names it (Xorg refcounts
        // `pWin->border.pixmap`). Omitting it freed the host handle
        // underneath a ring that was still sampling it (#133). All four
        // release sites now share one rule.
        .is_some_and(|xid| state.resources.host_xid_still_referenced(xid));
    if let Some(xid) = removed.host_xid
        && !still_referenced
    {
        backend.free_pixmap(origin, xid.as_raw())?;
    }
    debug!(
        "client {} #{} FreePixmap pixmap=0x{:x} freed host_xid={:?} retained={}",
        client_id.0,
        sequence.0,
        pixmap.0,
        removed.host_xid.map(crate::backend::PixmapHandle::as_raw),
        still_referenced
    );
    Ok(RequestOutcome::Handled)
}

pub(super) fn handle_query_best_size(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    let width = if body.len() >= 8 {
        u16::from_le_bytes([body[4], body[5]])
    } else {
        0
    };
    let height = if body.len() >= 8 {
        u16::from_le_bytes([body[6], body[7]])
    } else {
        0
    };
    debug!("client {} #{} QueryBestSize", client_id.0, sequence.0);
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let byte_order = client.byte_order;
    let mut buf: Vec<u8> = Vec::with_capacity(32);
    x11::write_query_best_size_reply(&mut buf, byte_order, sequence, width, height)?;
    Ok(write_to_client(client, client_id, &buf))
}
