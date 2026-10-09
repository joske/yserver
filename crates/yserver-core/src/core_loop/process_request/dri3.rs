use super::*;

pub(super) fn handle_dri3_request(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
    attached_fd: Option<OwnedFd>,
) -> io::Result<RequestOutcome> {
    use yserver_protocol::x11::{ClientByteOrder, dri3 as x11dri3};
    const DRI3_MAJOR_OPCODE: u8 = 147;
    let byte_order = state
        .clients
        .get(&client_id.0)
        .map_or(ClientByteOrder::LittleEndian, |c| c.byte_order);
    let caps = backend.dri3_capabilities();
    let minor = header.data;
    match minor {
        x11dri3::QUERY_VERSION => {
            let (cmaj, cmin) = x11dri3::parse_query_version(body).unwrap_or((0, 0));
            // Server caps the version per Dri3Caps::version. Without
            // syncobj support, version is (1, 3); fence_fd governs
            // individual request availability rather than the version
            // reply.
            let server_major = caps.version.0;
            let server_minor = caps.version.1;
            let major = cmaj.min(server_major);
            let minor = cmin.min(server_minor);
            debug!(
                "client {} #{} DRI3::QueryVersion client={cmaj}.{cmin} -> {major}.{minor}",
                client_id.0, sequence.0
            );
            let reply = x11dri3::encode_query_version_reply(byte_order, sequence, major, minor);
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            return Ok(write_to_client(client, client_id, &reply));
        }
        x11dri3::OPEN => {
            let req = x11dri3::parse_open(body);
            let drawable = req.map_or(0, |r| r.drawable);
            match backend.dri3_open(drawable) {
                Ok(fd) => {
                    debug!(
                        "client {} #{} DRI3::Open drawable=0x{drawable:x} -> fd={}",
                        client_id.0,
                        sequence.0,
                        std::os::fd::AsRawFd::as_raw_fd(&fd)
                    );
                    let reply = x11dri3::encode_open_reply(byte_order, sequence);
                    let Some(client) = state.clients.get_mut(&client_id.0) else {
                        return Ok(RequestOutcome::Handled);
                    };
                    let raw = std::os::fd::AsRawFd::as_raw_fd(&fd);
                    if let Err(e) = send_reply_with_fd(client, &reply, raw) {
                        log::warn!("DRI3::Open SCM_RIGHTS dispatch failed: {e}");
                        return Ok(RequestOutcome::Disconnect(client_id));
                    }
                    drop(fd);
                    return Ok(RequestOutcome::Handled);
                }
                Err(err) => {
                    debug!(
                        "client {} #{} DRI3::Open drawable=0x{drawable:x} -> BadAlloc ({err})",
                        client_id.0, sequence.0
                    );
                    return emit_x11_error_with_minor(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_ALLOC,
                        0,
                        u16::from(header.data),
                        DRI3_MAJOR_OPCODE,
                    );
                }
            }
        }
        x11dri3::PIXMAP_FROM_BUFFER => {
            let Some(req) = x11dri3::parse_pixmap_from_buffer(body) else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_LENGTH,
                    0,
                    u16::from(header.data),
                    DRI3_MAJOR_OPCODE,
                );
            };
            let Some(fd) = attached_fd else {
                debug!(
                    "client {} #{} DRI3::PixmapFromBuffer no SCM_RIGHTS fd attached -> BadAlloc",
                    client_id.0, sequence.0
                );
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_ALLOC,
                    req.pixmap,
                    u16::from(header.data),
                    DRI3_MAJOR_OPCODE,
                );
            };
            match backend.dri3_import_pixmap(
                fd,
                req.width,
                req.height,
                u32::from(req.stride),
                0,
                // DRI3 1.0 carries no modifier. That means the layout is
                // IMPLICIT, to be resolved from the buffer -- not linear.
                // This line used to pass `0`, i.e. an explicit
                // DRM_FORMAT_MOD_LINEAR, which is #138: Chrome hands us a
                // TILED VA-API decode buffer here, we recorded it as
                // linear, and `BuffersFromPixmap` then handed that lie
                // back so Chrome sampled its own frame wrong.
                crate::backend::Dri3ImportModifier::Implicit { size: req.size },
                req.depth,
                req.bpp,
            ) {
                Ok(handle) => {
                    debug!(
                        "client {} #{} DRI3::PixmapFromBuffer pixmap=0x{:x} {}x{} stride={} depth={} bpp={} -> imported",
                        client_id.0,
                        sequence.0,
                        req.pixmap,
                        req.width,
                        req.height,
                        req.stride,
                        req.depth,
                        req.bpp,
                    );
                    state.resources.create_pixmap(
                        client_id,
                        yserver_protocol::x11::CreatePixmapRequest {
                            pixmap: ResourceId(req.pixmap),
                            drawable: ResourceId(req.drawable),
                            width: req.width,
                            height: req.height,
                            depth: req.depth,
                        },
                    );
                    let _ = state
                        .resources
                        .set_pixmap_host_xid(ResourceId(req.pixmap), handle);
                }
                Err(err) => {
                    debug!(
                        "client {} #{} DRI3::PixmapFromBuffer pixmap=0x{:x} {}x{} stride={} depth={} bpp={} -> BadAlloc ({err})",
                        client_id.0,
                        sequence.0,
                        req.pixmap,
                        req.width,
                        req.height,
                        req.stride,
                        req.depth,
                        req.bpp,
                    );
                    return emit_x11_error_with_minor(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_ALLOC,
                        req.pixmap,
                        u16::from(header.data),
                        DRI3_MAJOR_OPCODE,
                    );
                }
            }
        }
        x11dri3::PIXMAP_FROM_BUFFERS => {
            let Some(req) = x11dri3::parse_pixmap_from_buffers(body) else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_LENGTH,
                    0,
                    u16::from(header.data),
                    DRI3_MAJOR_OPCODE,
                );
            };
            // Phase 4.2 scope: single-plane RGB only. num_buffers > 1
            // is wire-decoded (so logging is accurate) but rejected.
            if req.num_buffers != 1 {
                debug!(
                    "client {} #{} DRI3::PixmapFromBuffers num_buffers={} -> BadAlloc \
                     (multi-plane out of scope for Phase 4.2)",
                    client_id.0, sequence.0, req.num_buffers
                );
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_ALLOC,
                    req.pixmap,
                    u16::from(header.data),
                    DRI3_MAJOR_OPCODE,
                );
            }
            let Some(fd) = attached_fd else {
                debug!(
                    "client {} #{} DRI3::PixmapFromBuffers no SCM_RIGHTS fd attached -> BadAlloc",
                    client_id.0, sequence.0
                );
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_ALLOC,
                    req.pixmap,
                    u16::from(header.data),
                    DRI3_MAJOR_OPCODE,
                );
            };
            match backend.dri3_import_pixmap(
                fd,
                req.width,
                req.height,
                req.strides[0],
                req.offsets[0],
                crate::backend::Dri3ImportModifier::Explicit(req.modifier),
                req.depth,
                req.bpp,
            ) {
                Ok(handle) => {
                    debug!(
                        "client {} #{} DRI3::PixmapFromBuffers pixmap=0x{:x} {}x{} stride={} offset={} modifier=0x{:x} depth={} bpp={} -> imported",
                        client_id.0,
                        sequence.0,
                        req.pixmap,
                        req.width,
                        req.height,
                        req.strides[0],
                        req.offsets[0],
                        req.modifier,
                        req.depth,
                        req.bpp,
                    );
                    // Register the X resource id so subsequent
                    // CopyArea / PresentPixmap can resolve it. Mirror
                    // what handle_create_pixmap does.
                    state.resources.create_pixmap(
                        client_id,
                        yserver_protocol::x11::CreatePixmapRequest {
                            pixmap: ResourceId(req.pixmap),
                            drawable: ResourceId(req.window),
                            width: req.width,
                            height: req.height,
                            depth: req.depth,
                        },
                    );
                    let _ = state
                        .resources
                        .set_pixmap_host_xid(ResourceId(req.pixmap), handle);
                }
                Err(err) => {
                    debug!(
                        "client {} #{} DRI3::PixmapFromBuffers pixmap=0x{:x} {}x{} stride={} offset={} modifier=0x{:x} depth={} bpp={} -> BadAlloc ({err})",
                        client_id.0,
                        sequence.0,
                        req.pixmap,
                        req.width,
                        req.height,
                        req.strides[0],
                        req.offsets[0],
                        req.modifier,
                        req.depth,
                        req.bpp,
                    );
                    return emit_x11_error_with_minor(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_ALLOC,
                        req.pixmap,
                        u16::from(header.data),
                        DRI3_MAJOR_OPCODE,
                    );
                }
            }
        }
        x11dri3::BUFFER_FROM_PIXMAP => {
            let Some(pixmap) = x11dri3::parse_buffer_from_pixmap(body) else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_LENGTH,
                    0,
                    u16::from(header.data),
                    DRI3_MAJOR_OPCODE,
                );
            };
            let host_xid = match state
                .resources
                .pixmap(yserver_protocol::x11::ResourceId(pixmap))
                .and_then(|p| p.host_xid.map(|h| h.as_raw()))
            {
                Some(h) => h,
                None => {
                    return emit_x11_error_with_minor(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_DRAWABLE,
                        pixmap,
                        u16::from(header.data),
                        DRI3_MAJOR_OPCODE,
                    );
                }
            };
            match backend.dri3_export_pixmap(host_xid) {
                Ok((size, width, height, stride, depth, bpp, fd)) => {
                    debug!(
                        "client {} #{} DRI3::BufferFromPixmap pixmap=0x{pixmap:x} size={size} {width}x{height} stride={stride} depth={depth} bpp={bpp}",
                        client_id.0, sequence.0
                    );
                    let reply = x11dri3::encode_buffer_from_pixmap_reply(
                        byte_order, sequence, size, width, height, stride, depth, bpp,
                    );
                    let Some(client) = state.clients.get_mut(&client_id.0) else {
                        return Ok(RequestOutcome::Handled);
                    };
                    let raw = std::os::fd::AsRawFd::as_raw_fd(&fd);
                    if let Err(e) = send_reply_with_fd(client, &reply, raw) {
                        log::warn!("DRI3::BufferFromPixmap SCM_RIGHTS dispatch failed: {e}");
                        return Ok(RequestOutcome::Disconnect(client_id));
                    }
                    drop(fd);
                    return Ok(RequestOutcome::Handled);
                }
                Err(err) => {
                    // Xorg dri3/dri3_request.c:277 maps export failure to BadPixmap.
                    // BadDrawable is reserved for the unresolvable-XID case above.
                    // Note: Xorg returns BadAlloc when sending the fd fails; yserver
                    // instead disconnects (see the send_reply_with_fd path above) —
                    // a broken SCM_RIGHTS socket is unrecoverable for the client.
                    debug!(
                        "client {} #{} DRI3::BufferFromPixmap pixmap=0x{pixmap:x} -> BadPixmap ({err})",
                        client_id.0, sequence.0
                    );
                    return emit_x11_error_with_minor(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_PIXMAP,
                        pixmap,
                        u16::from(header.data),
                        DRI3_MAJOR_OPCODE,
                    );
                }
            }
        }
        x11dri3::BUFFERS_FROM_PIXMAP => {
            // DRI3 op 8 (BUFFERS_FROM_PIXMAP). mesa's loader_dri3 uses op 8
            // (not op 3) whenever the server advertises DRI3 >= 1.2 — and a
            // modifier-tiled backing (the RADV export path) is unusable by the
            // client without the modifier op 8 carries. yserver exports a
            // single plane, so the reply has nfd=1.
            let Some(pixmap) = x11dri3::parse_buffers_from_pixmap(body) else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_LENGTH,
                    0,
                    u16::from(header.data),
                    DRI3_MAJOR_OPCODE,
                );
            };
            let host_xid = match state
                .resources
                .pixmap(yserver_protocol::x11::ResourceId(pixmap))
                .and_then(|p| p.host_xid.map(|h| h.as_raw()))
            {
                Some(h) => h,
                None => {
                    return emit_x11_error_with_minor(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_DRAWABLE,
                        pixmap,
                        u16::from(header.data),
                        DRI3_MAJOR_OPCODE,
                    );
                }
            };
            match backend.dri3_export_pixmap_buffers(host_xid) {
                Ok(export) => {
                    debug!(
                        "client {} #{} DRI3::BuffersFromPixmap pixmap=0x{pixmap:x} {}x{} stride={} offset={} modifier=0x{:x} depth={} bpp={}",
                        client_id.0,
                        sequence.0,
                        export.width,
                        export.height,
                        export.stride,
                        export.offset,
                        export.modifier,
                        export.depth,
                        export.bpp,
                    );
                    let reply = x11dri3::encode_buffers_from_pixmap_reply(
                        byte_order,
                        sequence,
                        export.width,
                        export.height,
                        export.modifier,
                        export.depth,
                        export.bpp,
                        &[export.stride],
                        &[export.offset],
                    );
                    let Some(client) = state.clients.get_mut(&client_id.0) else {
                        return Ok(RequestOutcome::Handled);
                    };
                    let raw = std::os::fd::AsRawFd::as_raw_fd(&export.fd);
                    if let Err(e) = send_reply_with_fd(client, &reply, raw) {
                        log::warn!("DRI3::BuffersFromPixmap SCM_RIGHTS dispatch failed: {e}");
                        return Ok(RequestOutcome::Disconnect(client_id));
                    }
                    drop(export.fd);
                    return Ok(RequestOutcome::Handled);
                }
                Err(err) => {
                    debug!(
                        "client {} #{} DRI3::BuffersFromPixmap pixmap=0x{pixmap:x} -> BadPixmap ({err})",
                        client_id.0, sequence.0
                    );
                    return emit_x11_error_with_minor(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_PIXMAP,
                        pixmap,
                        u16::from(header.data),
                        DRI3_MAJOR_OPCODE,
                    );
                }
            }
        }
        x11dri3::GET_SUPPORTED_MODIFIERS => {
            let req = match x11dri3::parse_get_supported_modifiers(body) {
                Some(r) => r,
                None => {
                    return emit_x11_error_with_minor(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_LENGTH,
                        0,
                        u16::from(header.data),
                        DRI3_MAJOR_OPCODE,
                    );
                }
            };
            let (window_mods, screen_mods) =
                backend.dri3_supported_modifiers(req.window, req.depth, req.bpp);
            debug!(
                "client {} #{} DRI3::GetSupportedModifiers w=0x{:x} d={} bpp={} -> window={:?} screen={:?}",
                client_id.0,
                sequence.0,
                req.window,
                req.depth,
                req.bpp,
                window_mods
                    .iter()
                    .map(|m| format!("0x{m:x}"))
                    .collect::<Vec<_>>(),
                screen_mods
                    .iter()
                    .map(|m| format!("0x{m:x}"))
                    .collect::<Vec<_>>(),
            );
            let reply = x11dri3::encode_get_supported_modifiers_reply(
                byte_order,
                sequence,
                &window_mods,
                &screen_mods,
            );
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            return Ok(write_to_client(client, client_id, &reply));
        }
        x11dri3::FENCE_FROM_FD => {
            if !caps.fence_fd {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_IMPLEMENTATION,
                    0,
                    u16::from(header.data),
                    DRI3_MAJOR_OPCODE,
                );
            }
            let Some(req) = x11dri3::parse_fence_from_fd(body) else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_LENGTH,
                    0,
                    u16::from(header.data),
                    DRI3_MAJOR_OPCODE,
                );
            };
            let Some(fd) = attached_fd else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_ALLOC,
                    req.fence,
                    u16::from(header.data),
                    DRI3_MAJOR_OPCODE,
                );
            };
            // Best-effort import: many clients (Mesa included) send an
            // fd that doesn't import cleanly via SYNC_FD on Venus
            // passthrough. Per design §3.4 the GPU-side semaphore is
            // an optimisation; XSync trigger/await semantics still
            // work server-side via the fence resource. Always mirror
            // onto state.sync_fences so QueryFence / TriggerFence /
            // ResetFence behave correctly.
            match backend.dri3_fence_from_fd(req.fence, fd) {
                Ok(()) => {
                    debug!(
                        "client {} #{} DRI3::FenceFromFD 0x{:x} initially_triggered={} -> imported (semaphore-backed)",
                        client_id.0, sequence.0, req.fence, req.initially_triggered
                    );
                }
                Err(e) => {
                    log::warn!(
                        "DRI3::FenceFromFD 0x{:x}: VkSemaphore import failed ({e}); \
                         falling back to server-only fence (XSync trigger/await still works)",
                        req.fence
                    );
                }
            }
            state.sync_fences.insert(
                req.fence,
                crate::server::SyncFence {
                    owner: client_id,
                    triggered: req.initially_triggered,
                },
            );
        }
        x11dri3::FD_FROM_FENCE => {
            if !caps.fence_fd {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_IMPLEMENTATION,
                    0,
                    u16::from(header.data),
                    DRI3_MAJOR_OPCODE,
                );
            }
            let Some(req) = x11dri3::parse_fd_from_fence(body) else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_LENGTH,
                    0,
                    u16::from(header.data),
                    DRI3_MAJOR_OPCODE,
                );
            };
            match backend.dri3_fd_from_fence(req.fence) {
                Ok(fd) => {
                    let reply = x11dri3::encode_fd_from_fence_reply(byte_order, sequence);
                    let Some(client) = state.clients.get_mut(&client_id.0) else {
                        return Ok(RequestOutcome::Handled);
                    };
                    let raw = std::os::fd::AsRawFd::as_raw_fd(&fd);
                    if let Err(e) = send_reply_with_fd(client, &reply, raw) {
                        log::warn!("DRI3::FDFromFence SCM_RIGHTS dispatch failed: {e}");
                        return Ok(RequestOutcome::Disconnect(client_id));
                    }
                    drop(fd);
                    return Ok(RequestOutcome::Handled);
                }
                Err(e) => {
                    debug!(
                        "client {} #{} DRI3::FDFromFence 0x{:x} -> BadAlloc ({e})",
                        client_id.0, sequence.0, req.fence
                    );
                    return emit_x11_error_with_minor(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_ALLOC,
                        req.fence,
                        u16::from(header.data),
                        DRI3_MAJOR_OPCODE,
                    );
                }
            }
        }
        x11dri3::IMPORT_SYNCOBJ => {
            if !caps.syncobj {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_IMPLEMENTATION,
                    0,
                    u16::from(header.data),
                    DRI3_MAJOR_OPCODE,
                );
            }
            let Some(req) = x11dri3::parse_import_syncobj(body) else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_LENGTH,
                    0,
                    u16::from(header.data),
                    DRI3_MAJOR_OPCODE,
                );
            };
            if xid_out_of_client_range(state, client_id, req.syncobj)
                || state.xid_occupied(req.syncobj)
            {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_ID_CHOICE,
                    req.syncobj,
                    u16::from(header.data),
                    DRI3_MAJOR_OPCODE,
                );
            }
            let drawable_exists = state.resources.window(ResourceId(req.drawable)).is_some()
                || state.resources.pixmap(ResourceId(req.drawable)).is_some();
            if !drawable_exists {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_DRAWABLE,
                    req.drawable,
                    u16::from(header.data),
                    DRI3_MAJOR_OPCODE,
                );
            }
            let Some(fd) = attached_fd else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    req.syncobj,
                    u16::from(header.data),
                    DRI3_MAJOR_OPCODE,
                );
            };
            if let Err(e) = backend.dri3_import_syncobj(client_id, req.syncobj, fd) {
                debug!(
                    "client {} #{} DRI3::ImportSyncobj 0x{:x} -> BadAlloc ({e})",
                    client_id.0, sequence.0, req.syncobj
                );
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_ALLOC,
                    req.syncobj,
                    u16::from(header.data),
                    DRI3_MAJOR_OPCODE,
                );
            }
            assert!(
                state
                    .resources
                    .register_dri3_syncobj(ResourceId(req.syncobj), client_id),
                "validated DRI3 syncobj XID became occupied before registration"
            );
            debug!(
                "client {} #{} DRI3::ImportSyncobj 0x{:x} -> imported",
                client_id.0, sequence.0, req.syncobj
            );
        }
        x11dri3::FREE_SYNCOBJ => {
            if !caps.syncobj {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_IMPLEMENTATION,
                    0,
                    u16::from(header.data),
                    DRI3_MAJOR_OPCODE,
                );
            }
            let Some(syncobj) = x11dri3::parse_free_syncobj(body) else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_LENGTH,
                    0,
                    u16::from(header.data),
                    DRI3_MAJOR_OPCODE,
                );
            };
            let Some(owner) = state.resources.dri3_syncobj_owner(ResourceId(syncobj)) else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    syncobj,
                    u16::from(header.data),
                    DRI3_MAJOR_OPCODE,
                );
            };
            if owner != client_id {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_ACCESS,
                    syncobj,
                    u16::from(header.data),
                    DRI3_MAJOR_OPCODE,
                );
            }
            if let Err(e) = backend.dri3_free_syncobj(client_id, syncobj) {
                let code = if e.kind() == std::io::ErrorKind::PermissionDenied {
                    debug!(
                        "client {} #{} DRI3::FreeSyncobj 0x{:x} -> BadAccess ({e})",
                        client_id.0, sequence.0, syncobj
                    );
                    x11::error::BAD_ACCESS
                } else {
                    debug!(
                        "client {} #{} DRI3::FreeSyncobj 0x{:x} -> BadValue ({e})",
                        client_id.0, sequence.0, syncobj
                    );
                    x11::error::BAD_VALUE
                };
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    code,
                    syncobj,
                    u16::from(header.data),
                    DRI3_MAJOR_OPCODE,
                );
            }
            let removed = state.resources.remove_dri3_syncobj(ResourceId(syncobj));
            debug_assert_eq!(removed, Some(client_id));
            // Pairs with the `-> imported` line above. A successful free was
            // silent, so an import count could not be matched against a free
            // count and "are retired swapchain syncobjs released?" was
            // unanswerable from a capture — mpv scrubbing imports 6 per
            // swapchain rebuild.
            debug!(
                "client {} #{} DRI3::FreeSyncobj 0x{:x} -> freed",
                client_id.0, sequence.0, syncobj
            );
        }
        x11dri3::SET_DRM_DEVICE_IN_USE => {
            // Acknowledged but ignored — single-GPU only per design §1.
            debug!(
                "client {} #{} DRI3::SetDRMDeviceInUse (single-GPU; ignored)",
                client_id.0, sequence.0
            );
        }
        other => {
            return emit_x11_error_with_minor(
                state,
                client_id,
                sequence,
                x11::error::BAD_REQUEST,
                0,
                u16::from(other),
                DRI3_MAJOR_OPCODE,
            );
        }
    }
    Ok(RequestOutcome::Handled)
}
