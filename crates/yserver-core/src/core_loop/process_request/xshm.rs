use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) fn handle_mit_shm_request(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
    attached_fd: Option<OwnedFd>,
) -> io::Result<RequestOutcome> {
    use yserver_protocol::x11::mit_shm as shm;
    const MIT_SHM_MAJOR_OPCODE: u8 = 130;
    let minor = header.data;
    debug!(
        "client {} #{} MIT-SHM dispatch minor={minor} body_len={}",
        client_id.0,
        sequence.0,
        body.len()
    );
    match minor {
        shm::QUERY_VERSION => {
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            let byte_order = client.byte_order;
            // Xorg reports the server's effective uid/gid, stored in the
            // 16-bit wire fields (truncating, as the C assignment does).
            // SAFETY: geteuid/getegid take no arguments and cannot fail.
            let (euid, egid) = unsafe { (libc::geteuid(), libc::getegid()) };
            #[allow(clippy::cast_possible_truncation)]
            let reply = shm::encode_query_version_reply(
                byte_order,
                sequence,
                false,
                euid as u16,
                egid as u16,
            );
            return Ok(write_to_client(client, client_id, &reply));
        }
        shm::ATTACH => {
            let Some(req) = shm::parse_attach(body) else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_LENGTH,
                    0,
                    u16::from(minor),
                    MIT_SHM_MAJOR_OPCODE,
                );
            };
            match crate::server::MitShmSegment::from_shmid(client_id, req.shmid, req.read_only) {
                Ok(segment) => {
                    state.mit_shm_segments.insert(req.shmseg, segment);
                    debug!(
                        "client {} #{} MIT-SHM::Attach shmseg=0x{:x} shmid=0x{:x}",
                        client_id.0, sequence.0, req.shmseg, req.shmid
                    );
                }
                Err(_) => {
                    return emit_x11_error_with_minor(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_VALUE,
                        req.shmseg,
                        u16::from(minor),
                        MIT_SHM_MAJOR_OPCODE,
                    );
                }
            }
        }
        shm::ATTACH_FD => {
            let Some(req) = shm::parse_attach_fd(body) else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_LENGTH,
                    0,
                    u16::from(minor),
                    MIT_SHM_MAJOR_OPCODE,
                );
            };
            let Some(fd) = attached_fd else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    req.shmseg,
                    u16::from(minor),
                    MIT_SHM_MAJOR_OPCODE,
                );
            };
            // OwnedFd carries an automatic close-on-drop; from_fd takes
            // raw and assumes ownership transfer.
            let raw = std::os::fd::IntoRawFd::into_raw_fd(fd);
            match crate::server::MitShmSegment::from_fd(client_id, raw, req.read_only) {
                Ok(segment) => {
                    state.mit_shm_segments.insert(req.shmseg, segment);
                }
                Err(_) => {
                    return emit_x11_error_with_minor(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_VALUE,
                        req.shmseg,
                        u16::from(minor),
                        MIT_SHM_MAJOR_OPCODE,
                    );
                }
            }
        }
        shm::DETACH => {
            if let Some(shmseg) = shm::parse_detach(body) {
                state.mit_shm_segments.remove(&shmseg);
            }
        }
        shm::CREATE_PIXMAP => {
            let Some(req) = shm::parse_create_pixmap(body) else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_LENGTH,
                    0,
                    u16::from(minor),
                    MIT_SHM_MAJOR_OPCODE,
                );
            };
            return handle_mit_shm_create_pixmap(state, backend, origin, client_id, sequence, req);
        }
        shm::PUT_IMAGE => {
            let Some(req) = shm::parse_put_image(body) else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_LENGTH,
                    0,
                    u16::from(minor),
                    MIT_SHM_MAJOR_OPCODE,
                );
            };
            return handle_mit_shm_put_image(state, backend, origin, client_id, sequence, req);
        }
        shm::GET_IMAGE => {
            let Some(req) = shm::parse_get_image(body) else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_LENGTH,
                    0,
                    u16::from(minor),
                    MIT_SHM_MAJOR_OPCODE,
                );
            };
            return handle_mit_shm_get_image(state, backend, origin, client_id, sequence, req);
        }
        shm::CREATE_SEGMENT => {
            let Some(req) = shm::parse_create_segment(body) else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_LENGTH,
                    0,
                    u16::from(minor),
                    MIT_SHM_MAJOR_OPCODE,
                );
            };
            return handle_mit_shm_create_segment(state, client_id, sequence, req);
        }
        other => {
            return emit_x11_error_with_minor(
                state,
                client_id,
                sequence,
                x11::error::BAD_REQUEST,
                0,
                u16::from(other),
                MIT_SHM_MAJOR_OPCODE,
            );
        }
    }
    Ok(RequestOutcome::Handled)
}

fn handle_mit_shm_create_pixmap(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    req: yserver_protocol::x11::mit_shm::CreatePixmapRequest,
) -> io::Result<RequestOutcome> {
    const MIT_SHM_MAJOR_OPCODE: u8 = 130;
    use yserver_protocol::x11::mit_shm as shm;
    debug!(
        "client {} #{} MIT-SHM::CreatePixmap pid=0x{:x} drawable=0x{:x} {}x{} d{}",
        client_id.0, sequence.0, req.pid, req.drawable, req.width, req.height, req.depth,
    );
    let validation_failed = {
        let handle = state.clients.get(&client_id.0).expect("client registered");
        let owned = crate::server::IdAllocator::validate_owned(
            req.pid,
            handle.resource_id_base,
            handle.resource_id_mask,
        );
        let in_use = state.xid_occupied(req.pid);
        !owned || in_use
    };
    let drawable_exists = state.resources.window(ResourceId(req.drawable)).is_some()
        || state.resources.pixmap(ResourceId(req.drawable)).is_some();
    if validation_failed {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_ID_CHOICE,
            req.pid,
            u16::from(shm::CREATE_PIXMAP),
            MIT_SHM_MAJOR_OPCODE,
        );
    }
    if !drawable_exists {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_DRAWABLE,
            req.drawable,
            u16::from(shm::CREATE_PIXMAP),
            MIT_SHM_MAJOR_OPCODE,
        );
    }
    if !supported_pixmap_depth(req.depth) {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_VALUE,
            u32::from(req.depth),
            u16::from(shm::CREATE_PIXMAP),
            MIT_SHM_MAJOR_OPCODE,
        );
    }
    let Some(expected_len) = zpixmap_expected_len(req.width, req.height, req.depth) else {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_VALUE,
            req.shmseg,
            u16::from(shm::CREATE_PIXMAP),
            MIT_SHM_MAJOR_OPCODE,
        );
    };
    let host_xid = match backend.create_pixmap(origin, req.depth, req.width, req.height) {
        Ok(handle) => Some(handle),
        Err(err) => {
            log::warn!(
                "client {} MIT-SHM::CreatePixmap host CreatePixmap failed: {err}",
                client_id.0
            );
            None
        }
    };
    let snapshot: Vec<u8> = {
        let Some(segment) = state.mit_shm_segments.get(&req.shmseg) else {
            return emit_x11_error_with_minor(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                req.shmseg,
                u16::from(shm::CREATE_PIXMAP),
                MIT_SHM_MAJOR_OPCODE,
            );
        };
        let bytes = segment.as_slice();
        let start = req.offset as usize;
        let end = start.saturating_add(expected_len);
        if end > bytes.len() {
            return emit_x11_error_with_minor(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                req.offset,
                u16::from(shm::CREATE_PIXMAP),
                MIT_SHM_MAJOR_OPCODE,
            );
        }
        bytes[start..end].to_vec()
    };
    if let Some(host_xid) = host_xid {
        // The new pixmap takes the segment's bytes as they are: no client
        // GC, so not the function or plane mask the last draw left behind.
        let copy_gc = crate::backend::DrawState::default();
        let _ = backend
            .apply_clip_state(origin, &copy_gc.clip)
            .and_then(|()| backend.apply_draw_state(origin, &copy_gc));
        if let Err(err) = backend.put_image(
            origin,
            host_xid.as_raw(),
            req.depth,
            req.width,
            req.height,
            0,
            0,
            &snapshot,
        ) {
            log::warn!(
                "client {} MIT-SHM::CreatePixmap put_image failed: {err}",
                client_id.0
            );
        }
    }
    state.resources.create_pixmap(
        client_id,
        x11::CreatePixmapRequest {
            depth: req.depth,
            pixmap: ResourceId(req.pid),
            drawable: ResourceId(req.drawable),
            width: req.width,
            height: req.height,
        },
    );
    if let Some(xid) = host_xid {
        let updated = state
            .resources
            .set_pixmap_host_xid(ResourceId(req.pid), xid);
        debug_assert!(updated, "pixmap was just inserted above");
    }
    Ok(RequestOutcome::Handled)
}

pub(super) fn handle_mit_shm_put_image(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    req: yserver_protocol::x11::mit_shm::PutImageRequest,
) -> io::Result<RequestOutcome> {
    const MIT_SHM_MAJOR_OPCODE: u8 = 130;
    use yserver_protocol::x11::mit_shm as shm;
    // MIT-SHM PutImage perf-cliff diagnostic (2026-05-28 cinnamon
    // telemetry: single op130 calls at 100-128ms blocking the core
    // loop for ~6-8 vsync intervals). Section-time so we can pin
    // which step dominates. Threshold-gated at exit; steady-state
    // healthy calls (sub-100µs) stay silent.
    let t_entry = std::time::Instant::now();
    debug!(
        "client {} #{} MIT-SHM::PutImage drawable=0x{:x} {}x{} d{}",
        client_id.0, sequence.0, req.drawable, req.src_width, req.src_height, req.depth
    );
    let drawable = ResourceId(req.drawable);
    let gc = ResourceId(req.gc);
    if let Err((code, bad_value)) = validate_drawable_and_gc(state, drawable, gc) {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            code,
            bad_value,
            u16::from(shm::PUT_IMAGE),
            MIT_SHM_MAJOR_OPCODE,
        );
    }
    let draw_state = state.resources.resolve_draw_state(gc).unwrap_or_default();
    let target = state.resources.host_drawable_target(drawable);
    let Some(target) = target else {
        return Ok(RequestOutcome::Handled);
    };
    let Some(segment) = state.mit_shm_segments.get(&req.shmseg) else {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_VALUE,
            req.shmseg,
            u16::from(shm::PUT_IMAGE),
            MIT_SHM_MAJOR_OPCODE,
        );
    };
    let Some(snapshot) = extract_shm_zpixmap_region(
        segment.as_slice(),
        req.offset,
        req.total_width,
        req.total_height,
        req.src_x,
        req.src_y,
        req.src_width,
        req.src_height,
        req.depth,
    ) else {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_VALUE,
            req.offset,
            u16::from(shm::PUT_IMAGE),
            MIT_SHM_MAJOR_OPCODE,
        );
    };
    let t_after_extract = t_entry.elapsed();
    let snapshot_borrowed = matches!(snapshot, std::borrow::Cow::Borrowed(_));
    let snapshot_bytes = snapshot.len();
    backend.apply_clip_state(origin, &draw_state.clip)?;
    backend.apply_draw_state(origin, &draw_state)?;
    let t_after_apply_state = t_entry.elapsed();
    backend.put_image(
        origin,
        target.host_xid(),
        req.depth,
        req.src_width,
        req.src_height,
        req.dst_x,
        req.dst_y,
        &snapshot,
    )?;
    let t_after_put_image = t_entry.elapsed();
    let _dropped = accumulate_damage_to_state(
        state,
        ResourceId(req.drawable),
        req.dst_x,
        req.dst_y,
        req.src_width,
        req.src_height,
    );
    // ShmPutImage with send_event=true obligates a ShmCompletion event
    // once the transfer is done (shmproto.h). GTK/GDK pools SHM segments
    // and blocks the next frame until the completion frees one — without
    // this the render loop stalls after the pool is exhausted (the
    // cinnamon-settings repaint freeze). MIT-SHM first-event base is 65;
    // ShmCompletion offset is 0.
    if req.send_event {
        const MIT_SHM_FIRST_EVENT: u8 = 65;
        let _dropped = fanout_event_to_clients(state, &[client_id], |buf, seq, order| {
            x11::encode_shm_completion_event(
                buf,
                order,
                seq,
                MIT_SHM_FIRST_EVENT,
                ResourceId(req.drawable),
                u16::from(shm::PUT_IMAGE),
                MIT_SHM_MAJOR_OPCODE,
                req.shmseg,
                req.offset,
            );
        });
    }
    let total = t_entry.elapsed();
    // 5ms threshold matches the suspected cliff territory; healthy
    // PutImage steady-state is sub-100µs per call so threshold-
    // gating keeps the log quiet.
    if total >= std::time::Duration::from_millis(5) {
        // CALLING client (the one issuing PutImage) is `client_id` —
        // that's what we want to attribute the hot-path activity to.
        // The drawable's OWNER (high bits of `req.drawable`) is a
        // separate, less-actionable identity — typically a pixmap
        // whose creator may have disconnected with the resource kept
        // alive via X11 RetainTemporary / Permanent / SaveSet. So
        // log the caller's WM_CLASS as the primary attribution and
        // include the drawable XID for cross-reference.
        let caller_class = state
            .client_wm_class
            .get(&client_id.0)
            .map(String::as_str)
            .unwrap_or("<unknown>");
        log::debug!(
            "MIT-SHM PutImage perf: total={total_us}us \
             [extract={ext_us}us apply_state+={state_us}us put_image+={pi_us}us] \
             {w}x{h} depth={depth} bytes={bytes} borrowed={borrowed} \
             drawable=0x{drawable:x} caller=client{caller_id}/{caller_class:?}",
            total_us = total.as_micros(),
            ext_us = t_after_extract.as_micros(),
            state_us = t_after_apply_state
                .saturating_sub(t_after_extract)
                .as_micros(),
            pi_us = t_after_put_image
                .saturating_sub(t_after_apply_state)
                .as_micros(),
            w = req.src_width,
            h = req.src_height,
            depth = req.depth,
            bytes = snapshot_bytes,
            borrowed = snapshot_borrowed,
            drawable = req.drawable,
            caller_id = client_id.0,
        );
    }
    Ok(RequestOutcome::Handled)
}

fn handle_mit_shm_get_image(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    req: yserver_protocol::x11::mit_shm::GetImageRequest,
) -> io::Result<RequestOutcome> {
    use yserver_protocol::x11::mit_shm as shm;
    const MIT_SHM_MAJOR_OPCODE: u8 = 130;
    {
        let Some(segment) = state.mit_shm_segments.get(&req.shmseg) else {
            return emit_x11_error_with_minor(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                req.shmseg,
                u16::from(shm::GET_IMAGE),
                MIT_SHM_MAJOR_OPCODE,
            );
        };
        if segment.read_only {
            return emit_x11_error_with_minor(
                state,
                client_id,
                sequence,
                x11::error::BAD_ACCESS,
                req.shmseg,
                u16::from(shm::GET_IMAGE),
                MIT_SHM_MAJOR_OPCODE,
            );
        }
    }
    let target = state
        .resources
        .host_drawable_target(ResourceId(req.drawable));
    let Some(target) = target else {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_DRAWABLE,
            req.drawable,
            u16::from(shm::GET_IMAGE),
            MIT_SHM_MAJOR_OPCODE,
        );
    };
    let host_bytes = backend
        .get_image(
            origin,
            target.host_xid(),
            req.format,
            req.x,
            req.y,
            req.width,
            req.height,
            req.plane_mask,
        )
        .ok()
        .flatten();
    let Some(host_reply_bytes) = host_bytes else {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_DRAWABLE,
            req.drawable,
            u16::from(shm::GET_IMAGE),
            MIT_SHM_MAJOR_OPCODE,
        );
    };
    let pixel_data: Vec<u8> = host_reply_bytes.get(32..).unwrap_or(&[]).to_vec();
    {
        let Some(segment) = state.mit_shm_segments.get_mut(&req.shmseg) else {
            return emit_x11_error_with_minor(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                req.shmseg,
                u16::from(shm::GET_IMAGE),
                MIT_SHM_MAJOR_OPCODE,
            );
        };
        let Some(buf) = segment.as_mut_slice() else {
            return emit_x11_error_with_minor(
                state,
                client_id,
                sequence,
                x11::error::BAD_ACCESS,
                req.shmseg,
                u16::from(shm::GET_IMAGE),
                MIT_SHM_MAJOR_OPCODE,
            );
        };
        let start = req.offset as usize;
        let end = start.saturating_add(pixel_data.len());
        if end > buf.len() {
            return emit_x11_error_with_minor(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                req.offset,
                u16::from(shm::GET_IMAGE),
                MIT_SHM_MAJOR_OPCODE,
            );
        }
        buf[start..end].copy_from_slice(&pixel_data);
    }
    let depth = host_reply_bytes.first().copied().unwrap_or(24);
    let visual = crate::resources::ROOT_VISUAL.0;
    #[allow(clippy::cast_possible_truncation)]
    let size = pixel_data.len() as u32;
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let byte_order = client.byte_order;
    let reply = shm::encode_get_image_reply(byte_order, sequence, depth, visual, size);
    Ok(write_to_client(client, client_id, &reply))
}

fn handle_mit_shm_create_segment(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    req: yserver_protocol::x11::mit_shm::CreateSegmentRequest,
) -> io::Result<RequestOutcome> {
    const MIT_SHM_MAJOR_OPCODE: u8 = 130;
    use yserver_protocol::x11::mit_shm as shm;
    if req.size == 0 {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_VALUE,
            req.shmseg,
            u16::from(shm::CREATE_SEGMENT),
            MIT_SHM_MAJOR_OPCODE,
        );
    }
    let fd = unsafe { libc::memfd_create(c"yserver-shm".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_ALLOC,
            req.shmseg,
            u16::from(shm::CREATE_SEGMENT),
            MIT_SHM_MAJOR_OPCODE,
        );
    }
    if unsafe { libc::ftruncate(fd, libc::off_t::from(req.size as i32)) } < 0 {
        unsafe { libc::close(fd) };
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_ALLOC,
            req.shmseg,
            u16::from(shm::CREATE_SEGMENT),
            MIT_SHM_MAJOR_OPCODE,
        );
    }
    let fd_for_client = unsafe { libc::dup(fd) };
    if fd_for_client < 0 {
        unsafe { libc::close(fd) };
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_ALLOC,
            req.shmseg,
            u16::from(shm::CREATE_SEGMENT),
            MIT_SHM_MAJOR_OPCODE,
        );
    }
    let segment = match crate::server::MitShmSegment::from_fd(client_id, fd, req.read_only) {
        Ok(s) => s,
        Err(_) => {
            unsafe { libc::close(fd_for_client) };
            return emit_x11_error_with_minor(
                state,
                client_id,
                sequence,
                x11::error::BAD_ALLOC,
                req.shmseg,
                u16::from(shm::CREATE_SEGMENT),
                MIT_SHM_MAJOR_OPCODE,
            );
        }
    };
    state.mit_shm_segments.insert(req.shmseg, segment);
    let send_res = match state.clients.get_mut(&client_id.0) {
        Some(client) => {
            let reply = yserver_protocol::x11::mit_shm::encode_create_segment_reply(
                client.byte_order,
                sequence,
            );
            send_reply_with_fd(client, &reply, fd_for_client)
        }
        None => Ok(()),
    };
    unsafe { libc::close(fd_for_client) };
    if send_res.is_err() {
        // Xorg's `ProcShmCreateSegment` (`Xext/shm.c:1323`):
        //
        //     if (WriteFdToClient(client, fd, TRUE) < 0) {
        //         FreeResource(stuff->shmseg, X11_RESTYPE_NONE);
        //         close(fd);
        //         return BadAlloc;
        //     }
        //
        // Both halves matter. Propagating the I/O error instead left the
        // segment in `mit_shm_segments` with no client able to reach it,
        // and sent no protocol reply at all — the client saw a request that
        // neither succeeded nor failed. The dispatch gate above should mean
        // this is now unreachable for the transport reason; it stays
        // because a short write is not the only way to get here.
        state.mit_shm_segments.remove(&req.shmseg);
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_ALLOC,
            req.shmseg,
            u16::from(shm::CREATE_SEGMENT),
            MIT_SHM_MAJOR_OPCODE,
        );
    }
    Ok(RequestOutcome::Handled)
}

/// Extract a tightly-packed `src_width × src_height × bpp(depth)`
/// region from an MIT-SHM ZPixmap buffer laid out as `total_width ×
/// total_height` with the server's standard scanline padding. The
/// caller passes the full shm segment in `bytes`; `offset` is the
/// byte offset to the START of the image (not the region).
///
/// Returns `None` if any computed offset would exceed `bytes.len()`,
/// or the depth is unsupported, or any required arithmetic
/// overflows. For `depth == 1` (bit-packed), `src_x` must be a
/// multiple of 8 — non-byte-aligned bit extraction is not
/// implemented; in practice all known clients use `src_x = 0` for
/// bitmasks.
pub(super) fn extract_shm_zpixmap_region(
    bytes: &[u8],
    offset: u32,
    total_width: u16,
    total_height: u16,
    src_x: i16,
    src_y: i16,
    src_width: u16,
    src_height: u16,
    depth: u8,
) -> Option<std::borrow::Cow<'_, [u8]>> {
    if src_x < 0 || src_y < 0 {
        return None;
    }
    let src_x = u32::try_from(src_x).ok()?;
    let src_y = u32::try_from(src_y).ok()?;
    // The region must fit inside the declared total image.
    let src_x_end = src_x.checked_add(u32::from(src_width))?;
    let src_y_end = src_y.checked_add(u32::from(src_height))?;
    if src_x_end > u32::from(total_width) || src_y_end > u32::from(total_height) {
        return None;
    }

    let total_stride = zpixmap_row_stride(total_width, depth)?;
    let src_stride = zpixmap_row_stride(src_width, depth)?;

    // Per-row leading-byte offset within the source row.
    let src_x_bytes: usize = match depth {
        1 => {
            if src_x % 8 != 0 {
                return None;
            }
            (src_x / 8) as usize
        }
        4 => {
            if src_x % 2 != 0 {
                return None;
            }
            (src_x / 2) as usize
        }
        8 => src_x as usize,
        24 | 32 => (src_x as usize).checked_mul(4)?,
        _ => return None,
    };

    let base = usize::try_from(offset).ok()?;
    // Bytes-per-source-row that actually carry image data (no trailing pad).
    let row_bytes: usize = match depth {
        24 | 32 => (src_width as usize).checked_mul(4)?,
        8 => src_width as usize,
        4 => (src_width as usize).div_ceil(2),
        1 => (src_width as usize).div_ceil(8),
        _ => return None,
    };
    if row_bytes > src_stride {
        return None;
    }

    // Fast path: full-frame extract (src origin = (0,0), src dims = total
    // dims) AND no trailing per-row pad gap (`row_bytes == src_stride ==
    // total_stride`). The wanted bytes are contiguous in the SHM segment
    // starting at `offset` — borrow directly instead of per-row memcpy
    // into a fresh Vec. 2026-05-28 cinnamon telemetry showed 28MB full-
    // frame extracts at ~8-10ms each; this fast path drops them to a
    // bounds-check + slice. The slow per-row path stays for partial
    // regions (src offset != 0 or src dims != total dims) and for depths
    // where `row_bytes < src_stride` (padding fills required).
    if src_x == 0
        && src_y == 0
        && src_width == total_width
        && src_height == total_height
        && row_bytes == src_stride
    {
        let total_bytes = src_stride.checked_mul(usize::from(src_height))?;
        let end = base.checked_add(total_bytes)?;
        if end > bytes.len() {
            return None;
        }
        return Some(std::borrow::Cow::Borrowed(&bytes[base..end]));
    }

    let mut out = Vec::with_capacity(src_stride.checked_mul(usize::from(src_height))?);
    for r in 0..usize::from(src_height) {
        let row_y = (src_y as usize).checked_add(r)?;
        let row_start = base
            .checked_add(row_y.checked_mul(total_stride)?)?
            .checked_add(src_x_bytes)?;
        let row_end = row_start.checked_add(row_bytes)?;
        if row_end > bytes.len() {
            return None;
        }
        out.extend_from_slice(&bytes[row_start..row_end]);
        // Pad scanline to 32-bit boundary if needed (d1/d4/d8 with
        // width not a multiple of the pad unit). Output buffer's
        // row stride is `src_stride`; pad with zeros so downstream
        // consumers see the standard ZPixmap layout.
        if src_stride > row_bytes {
            out.resize(out.len() + (src_stride - row_bytes), 0);
        }
    }
    Some(std::borrow::Cow::Owned(out))
}
