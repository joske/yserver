use super::*;

/// X-Resource (`Res`) extension. `QueryClients` returns the real list of
/// connected clients (their XID ranges); `QueryClientResources` returns
/// real per-type resource counts for a client (the two queries `xrestop`
/// leans on). `QueryClientPixmapBytes` computes live padded pixmap storage.
/// `QueryClientIds` reports ClientXID identities (PID identities are omitted
/// because peer credentials are not retained). `QueryResourceBytes` remains
/// empty until yserver keeps recursive resource-size accounting.
pub(super) fn handle_x_resource_request(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    use yserver_protocol::x11::{ClientByteOrder, x_resource as x11xres};

    /// Peer pid of a client's connection, for X-Resource's LocalClientPID
    /// identity. Read from the live socket rather than cached at accept: the
    /// credentials are fixed for the socket's lifetime, and `ClientState` is
    /// built at 46 sites (mostly tests), so a lazy read avoids threading a
    /// field through all of them for a rarely-issued request.
    ///
    /// `SO_PEERCRED`/`ucred` is Linux-specific. FreeBSD's `getpeereid` and
    /// `LOCAL_PEERCRED` expose uid/gid but no pid, so there we report no PID
    /// identity — which is a supported outcome, not a gap: Xorg's
    /// `GetClientPid` returns -1 whenever the OS cannot supply one and the
    /// caller then omits the identity (`Xext/xres.c`).
    #[cfg(target_os = "linux")]
    fn client_peer_pid(client: &crate::server::ClientState) -> Option<u32> {
        use std::os::fd::AsRawFd;
        if !client.is_local {
            return None;
        }
        let guard = client
            .writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut cred = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut len = u32::try_from(std::mem::size_of::<libc::ucred>()).ok()?;
        // SAFETY: `guard` keeps the socket alive for the call, and `cred`/`len`
        // are the exact out-param types SO_PEERCRED documents.
        let rc = unsafe {
            libc::getsockopt(
                guard.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                std::ptr::from_mut(&mut cred).cast(),
                &mut len,
            )
        };
        drop(guard);
        (rc == 0 && cred.pid > 0).then(|| cred.pid.cast_unsigned())
    }

    #[cfg(not(target_os = "linux"))]
    fn client_peer_pid(_client: &crate::server::ClientState) -> Option<u32> {
        None
    }

    fn target_client_for_xid(state: &ServerState, xid: u32) -> Option<ClientId> {
        state
            .clients
            .iter()
            .find(|(_, client)| (xid & !client.resource_id_mask) == client.resource_id_base)
            .map(|(id, _)| ClientId(*id))
    }
    let byte_order = state
        .clients
        .get(&client_id.0)
        .map_or(ClientByteOrder::LittleEndian, |c| c.byte_order);
    let minor = header.data;
    let reply = match minor {
        x11xres::QUERY_VERSION => {
            let (cmaj, cmin) = x11xres::parse_query_version(body).unwrap_or((0, 0));
            let major = u16::from(cmaj).min(x11xres::MAJOR_VERSION);
            let minor_ver = u16::from(cmin).min(x11xres::MINOR_VERSION);
            debug!(
                "client {} #{} X-Resource::QueryVersion client={cmaj}.{cmin} -> {major}.{minor_ver}",
                client_id.0, sequence.0
            );
            x11xres::encode_query_version_reply(byte_order, sequence, major, minor_ver)
        }
        x11xres::QUERY_CLIENTS => {
            // Every connected client by its XID resource range
            // (resource_base, resource_mask), sorted by client id for a
            // deterministic reply. This is exactly what xrestop and other
            // resource monitors read.
            let mut entries: Vec<(u32, u32, u32)> = state
                .clients
                .iter()
                .map(|(id, c)| (*id, c.resource_id_base, c.resource_id_mask))
                .collect();
            entries.sort_by_key(|(id, _, _)| *id);
            let clients: Vec<(u32, u32)> = entries
                .iter()
                .map(|(_, base, mask)| (*base, *mask))
                .collect();
            debug!(
                "client {} #{} X-Resource::QueryClients -> {} clients",
                client_id.0,
                sequence.0,
                clients.len()
            );
            x11xres::encode_query_clients_reply(byte_order, sequence, &clients)
        }
        x11xres::QUERY_CLIENT_RESOURCES => {
            // body: xid(4). X-Resource identifies the target client by the
            // xid's resource range (Xorg CLIENT_ID()), not by it being a
            // live resource — xrestop passes the client's resource_base.
            let xid = if body.len() >= 4 {
                u32::from_le_bytes([body[0], body[1], body[2], body[3]])
            } else {
                0
            };
            let Some(target) = target_client_for_xid(state, xid) else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    xid,
                    u16::from(minor),
                    header.opcode,
                );
            };
            let pairs = state.resources.resource_counts_by_owner(target);
            let types: Vec<(u32, u32)> = pairs
                .iter()
                .map(|(name, count)| (state.atoms.intern(name, false).0, *count))
                .collect();
            debug!(
                "client {} #{} X-Resource::QueryClientResources xid=0x{xid:x} -> {} types",
                client_id.0,
                sequence.0,
                types.len()
            );
            x11xres::encode_query_client_resources_reply(byte_order, sequence, &types)
        }
        x11xres::QUERY_CLIENT_PIXMAP_BYTES => {
            let xid = body
                .get(0..4)
                .map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap()))
                .unwrap_or(0);
            let Some(target) = target_client_for_xid(state, xid) else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    xid,
                    u16::from(minor),
                    header.opcode,
                );
            };
            let bytes = state.resources.pixmap_bytes_by_owner(target);
            debug!(
                "client {} #{} X-Resource::QueryClientPixmapBytes xid=0x{xid:x} -> {bytes}",
                client_id.0, sequence.0,
            );
            x11xres::encode_query_client_pixmap_bytes_reply(byte_order, sequence, bytes)
        }
        x11xres::QUERY_CLIENT_IDS => {
            let num_specs = body
                .get(0..4)
                .map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap()))
                .unwrap_or(0);
            // Track the requested mask per client: Xorg applies each spec's
            // mask to the clients that spec selects (`WillConstructMask`,
            // Xext/xres.c), where mask 0 means "every identity". A client
            // named by several specs gets the union.
            let mut selected: Vec<(u32, u32)> = Vec::new();
            for spec in body
                .get(4..)
                .unwrap_or_default()
                .chunks_exact(8)
                .take(usize::try_from(num_specs).unwrap_or(usize::MAX))
            {
                let requested = u32::from_le_bytes(spec[0..4].try_into().unwrap());
                let mask = u32::from_le_bytes(spec[4..8].try_into().unwrap());
                const KNOWN: u32 = x11xres::CLIENT_XID_MASK | x11xres::LOCAL_CLIENT_PID_MASK;
                if mask != 0 && mask & KNOWN == 0 {
                    continue;
                }
                if requested == 0 {
                    selected.extend(state.clients.keys().map(|id| (*id, mask)));
                } else if let Some(target) = target_client_for_xid(state, requested) {
                    selected.push((target.0, mask));
                }
            }
            selected.sort_unstable();
            let mut merged: Vec<(u32, u32)> = Vec::with_capacity(selected.len());
            for (id, mask) in selected {
                match merged.last_mut() {
                    Some((prev, acc)) if *prev == id => *acc |= mask,
                    _ => merged.push((id, mask)),
                }
            }

            let mut entries = Vec::with_capacity(merged.len());
            for (id, mask) in merged {
                let Some(client) = state.clients.get(&id) else {
                    continue;
                };
                let base = client.resource_id_base;
                // Xorg emits ClientXID first, then LocalClientPID.
                if mask == 0 || mask & x11xres::CLIENT_XID_MASK != 0 {
                    entries.push(x11xres::ClientIdEntry::xid(base));
                }
                if mask == 0 || mask & x11xres::LOCAL_CLIENT_PID_MASK != 0 {
                    // Omitted when the OS cannot supply a pid, exactly as Xorg
                    // skips the identity when GetClientPid returns -1.
                    if let Some(pid) = client_peer_pid(client) {
                        entries.push(x11xres::ClientIdEntry::pid(base, pid));
                    }
                }
            }
            debug!(
                "client {} #{} X-Resource::QueryClientIds -> {} identities",
                client_id.0,
                sequence.0,
                entries.len(),
            );
            x11xres::encode_query_client_ids_reply(byte_order, sequence, &entries)
        }
        x11xres::QUERY_RESOURCE_BYTES => {
            debug!(
                "client {} #{} X-Resource::QueryResourceBytes -> 0 sizes (stub)",
                client_id.0, sequence.0
            );
            x11xres::encode_query_resource_bytes_empty_reply(byte_order, sequence)
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
    };
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    Ok(write_to_client(client, client_id, &reply))
}

/// Returns `(major_opcode, first_event, first_error)` for an extension
/// the client just queried, or `None` if it isn't available. Mirrors
/// `nested::extension_query_reply` but operates on a live
/// `&mut dyn Backend` instead of `Option<&Arc<Mutex<dyn Backend>>>`.
pub(super) fn extension_query_reply(name: &str, backend: &mut dyn Backend) -> Option<(u8, u8, u8)> {
    use crate::nested::{EXTENSIONS, ExtensionAvailability};
    let ext = EXTENSIONS.iter().find(|ext| ext.name == name)?;
    let available = match ext.availability {
        ExtensionAvailability::Always => true,
        ExtensionAvailability::HostRender => backend.render_opcode().is_some(),
        ExtensionAvailability::HostXkb => backend.xkb_opcode().is_some(),
        ExtensionAvailability::Dri3 => backend.dri3_capabilities().version != (0, 0),
    };
    if !available {
        return None;
    }
    if ext.availability == ExtensionAvailability::HostXkb {
        let (_, first_event, first_error) = backend.xkb_info()?;
        return Some((ext.major_opcode, first_event, first_error));
    }
    Some((ext.major_opcode, ext.first_event, ext.first_error))
}

pub(super) fn advertised_extension_names(backend: &mut dyn Backend) -> Vec<&'static str> {
    use crate::nested::{EXTENSIONS, ExtensionAvailability};
    let render_available = backend.render_opcode().is_some();
    let xkb_available = backend.xkb_opcode().is_some();
    let dri3_available = backend.dri3_capabilities().version != (0, 0);
    EXTENSIONS
        .iter()
        .filter(|ext| match ext.availability {
            ExtensionAvailability::Always => true,
            ExtensionAvailability::HostRender => render_available,
            ExtensionAvailability::HostXkb => xkb_available,
            ExtensionAvailability::Dri3 => dri3_available,
        })
        .map(|ext| ext.name)
        .collect()
}

pub(super) fn handle_grab_server(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
) -> io::Result<RequestOutcome> {
    // Requests from another client are held by the core loop and therefore
    // never reach this handler while a grab is active. Re-grabbing by the
    // owner is idempotent, as in Xorg's ProcGrabServer.
    if state.server_grab_owner.is_none() || state.server_grab_owner == Some(client_id) {
        state.server_grab_owner = Some(client_id);
    }
    debug!("client {} #{} GrabServer", client_id.0, sequence.0);
    Ok(RequestOutcome::Handled)
}

pub(super) fn handle_ungrab_server(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
) -> io::Result<RequestOutcome> {
    // A non-owner cannot normally reach this handler: its request is parked
    // behind the active grab. Keeping the ownership check makes direct unit
    // calls and future dispatch changes fail closed.
    if state.server_grab_owner == Some(client_id) {
        state.server_grab_owner = None;
    }
    debug!("client {} #{} UngrabServer", client_id.0, sequence.0);
    Ok(RequestOutcome::Handled)
}

pub(super) fn handle_list_hosts(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
) -> io::Result<RequestOutcome> {
    debug!("client {} #{} ListHosts", client_id.0, sequence.0);
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let byte_order = client.byte_order;
    let mut buf: Vec<u8> = Vec::with_capacity(32);
    x11::write_list_hosts_reply(&mut buf, byte_order, sequence)?;
    Ok(write_to_client(client, client_id, &buf))
}

/// SetCloseDownMode (112): mode is in header.data. Valid values are
/// 0 (Destroy), 1 (RetainPermanent), 2 (RetainTemporary). Any other
/// value is BadValue. The stored mode is consulted by `process_disconnect`
/// to decide whether to free the client's resources or keep them
/// (with the original `owner: ClientId` intact) and record the client
/// as a zombie in `ServerState.zombie_clients`.
pub(super) fn handle_set_close_down_mode(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
) -> io::Result<RequestOutcome> {
    debug!(
        "client {} #{} SetCloseDownMode mode={}",
        client_id.0, sequence.0, header.data
    );
    if header.data > 2 {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_VALUE,
            u32::from(header.data),
            112,
        );
    }
    if header.data == 0 {
        state.close_down_modes.remove(&client_id.0);
    } else {
        state.close_down_modes.insert(client_id.0, header.data);
    }
    Ok(RequestOutcome::Handled)
}

/// KillClient (113): resource ID lives in the first 4 bytes of `body`.
/// `resource == 0` is the spec-defined `AllTemporary` magic — destroy
/// every zombie client whose stored close-down mode is `RetainTemporary`.
/// Otherwise look up the resource's owner: if it's a live client,
/// force-disconnect them (their close-down mode still applies); if it's
/// a zombie, destroy that zombie's resources; if it's `SERVER_OWNER`,
/// noop; if the resource doesn't exist, BadValue.
pub(super) fn handle_kill_client(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    if body.len() < 4 {
        return Ok(RequestOutcome::Handled);
    }
    let resource = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
    debug!(
        "client {} #{} KillClient resource=0x{:x}",
        client_id.0, sequence.0, resource
    );
    if resource == 0 {
        let temp_zombies: Vec<u32> = state
            .zombie_clients
            .iter()
            .filter_map(|(&id, &mode)| (mode == 2).then_some(id))
            .collect();
        for zid in temp_zombies {
            crate::core_loop::process_disconnect::destroy_zombie_resources(
                state,
                backend,
                ClientId(zid),
            );
            state.zombie_clients.remove(&zid);
        }
        return Ok(RequestOutcome::Handled);
    }
    let Some(owner) = state.resources.resource_owner(ResourceId(resource)) else {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_VALUE,
            resource,
            113,
        );
    };
    if owner == crate::resources::SERVER_OWNER {
        // Server-reserved resource (root window, overlay, etc.). Noop.
        return Ok(RequestOutcome::Handled);
    }
    if state.zombie_clients.contains_key(&owner.0) {
        crate::core_loop::process_disconnect::destroy_zombie_resources(state, backend, owner);
        state.zombie_clients.remove(&owner.0);
        return Ok(RequestOutcome::Handled);
    }
    if owner == client_id {
        // X11 spec: KillClient targeting any of your own resources
        // forces your own connection closed. Your stored close-down
        // mode still applies — process_disconnect consults it.
        return Ok(RequestOutcome::Disconnect(client_id));
    }
    if state.clients.contains_key(&owner.0) {
        // Force-disconnect the other client. Their close-down mode is
        // honored: if they set RetainPermanent / RetainTemporary,
        // their resources survive (they become a zombie) instead of
        // being freed.
        crate::core_loop::process_disconnect::process_disconnect(state, backend, owner);
    }
    Ok(RequestOutcome::Handled)
}

/// GetMotionEvents (39): return bounded pointer history translated into the
/// requested window, filtering samples outside its border-inclusive bounds.
pub(super) fn handle_get_motion_events(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    let window = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
    let Some(win) = state.resources.window(window) else {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_WINDOW,
            window.0,
            39,
        );
    };
    let (width, height, border) = (win.width, win.height, win.border_width);
    let (origin_x, origin_y) = state.resources.window_absolute_position(window);
    let start = u32::from_le_bytes(body[4..8].try_into().expect("four bytes"));
    let stop = u32::from_le_bytes(body[8..12].try_into().expect("four bytes"));
    let border = i32::from(border);
    let xmin = origin_x - border;
    let ymin = origin_y - border;
    let xmax = origin_x + i32::from(width) + border;
    let ymax = origin_y + i32::from(height) + border;
    let history: Vec<_> = motion_history_range(state, start, stop)
        .into_iter()
        .filter(|record| {
            let x = i32::from(record.root_x);
            let y = i32::from(record.root_y);
            (xmin..xmax).contains(&x) && (ymin..ymax).contains(&y)
        })
        .collect();
    debug!("client {} #{} GetMotionEvents", client_id.0, sequence.0);
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let length_words = u32::try_from(history.len())
        .unwrap_or(u32::MAX)
        .saturating_mul(2);
    let mut buf = x11::fixed_reply(client.byte_order, sequence, 0, length_words);
    x11::write_u32(
        client.byte_order,
        &mut buf,
        u32::try_from(history.len()).unwrap_or(u32::MAX),
    );
    buf.extend_from_slice(&[0u8; 20]);
    for record in history {
        x11::write_u32(client.byte_order, &mut buf, record.time);
        x11::write_i16(
            client.byte_order,
            &mut buf,
            i16::try_from(i32::from(record.root_x) - origin_x).unwrap_or(i16::MAX),
        );
        x11::write_i16(
            client.byte_order,
            &mut buf,
            i16::try_from(i32::from(record.root_y) - origin_y).unwrap_or(i16::MAX),
        );
    }
    Ok(write_to_client(client, client_id, &buf))
}

pub(super) fn handle_ge_request(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
) -> io::Result<RequestOutcome> {
    if header.data == 0 {
        debug!("client {} #{} GEQueryVersion", client_id.0, sequence.0);
        let Some(client) = state.clients.get_mut(&client_id.0) else {
            return Ok(RequestOutcome::Handled);
        };
        let byte_order = client.byte_order;
        let mut buf: Vec<u8> = Vec::with_capacity(32);
        x11::write_ge_query_version_reply(&mut buf, byte_order, sequence)?;
        return Ok(write_to_client(client, client_id, &buf));
    }
    emit_x11_error_with_minor(
        state,
        client_id,
        sequence,
        x11::error::BAD_REQUEST,
        0,
        u16::from(header.data),
        header.opcode,
    )
}

pub(super) fn handle_big_requests_request(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
) -> io::Result<RequestOutcome> {
    if header.data != 0 {
        // Malformed minor — the reader thread parked waiting for an
        // Apply / Ignore signal after sending the Enable through the
        // channel; unblock it with `IgnoreBigRequests` so it doesn't
        // deadlock.
        if let Some(client) = state.clients.get(&client_id.0)
            && let Some(tx) = client.reader_control.as_ref()
        {
            let _ = tx.send(crate::server::ReaderControl::IgnoreBigRequests);
        }
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
    debug!("client {} #{} BigRequestsEnable", client_id.0, sequence.0);
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let byte_order = client.byte_order;
    client.big_requests_enabled = true;
    let mut buf: Vec<u8> = Vec::with_capacity(32);
    x11::write_big_requests_enable_reply(
        &mut buf,
        byte_order,
        sequence,
        x11::MAX_BIG_REQUEST_UNITS,
    )?;
    let outcome = write_to_client(client, client_id, &buf);
    // Unblock the reader so subsequent requests use big-framing.
    if let Some(tx) = client.reader_control.as_ref() {
        let _ = tx.send(crate::server::ReaderControl::ApplyBigRequests);
    }
    Ok(outcome)
}

pub(super) fn handle_query_extension(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    let name = x11::query_extension_name(body);
    let (present, major_opcode, first_event, first_error) = extension_query_reply(&name, backend)
        .map(|(major_opcode, first_event, first_error)| {
            (true, major_opcode, first_event, first_error)
        })
        .unwrap_or((false, 0, 0, 0));
    debug!(
        "client {} #{} QueryExtension {:?} -> {}",
        client_id.0,
        sequence.0,
        name,
        if present { "present" } else { "absent" }
    );
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let byte_order = client.byte_order;
    let mut buf: Vec<u8> = Vec::with_capacity(32);
    x11::write_query_extension_reply(
        &mut buf,
        byte_order,
        sequence,
        present,
        major_opcode,
        first_event,
        first_error,
    )?;
    Ok(write_to_client(client, client_id, &buf))
}

pub(super) fn handle_list_extensions(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
    sequence: SequenceNumber,
) -> io::Result<RequestOutcome> {
    debug!("client {} #{} ListExtensions", client_id.0, sequence.0);
    let names = advertised_extension_names(backend);
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let byte_order = client.byte_order;
    let mut buf: Vec<u8> = Vec::with_capacity(32 + names.len() * 16);
    x11::write_list_extensions_reply(&mut buf, byte_order, sequence, &names)?;
    Ok(write_to_client(client, client_id, &buf))
}

/// Largest contiguous free run in `[max(base,1) .. base|mask]`,
/// given the SORTED+DEDUPED occupied ids in that range. Returns
/// Xorg's exact exhaustion wire shape `(0, 1)` when nothing is free
/// (dix/resource.c:733-737 zeroes min/max; the reply encodes
/// max-min+1 = 1; libxcb treats start 0 as exhausted). Largest-gap
/// is an implementation choice — the protocol promises only "a
/// contiguous range of unused IDs" (spec "GetXIDRange algorithm").
pub(super) fn largest_free_xid_gap(base: u32, mask: u32, used_sorted: &[u32]) -> (u32, u32) {
    let lo = base.max(1); // XID 0 is never allocatable
    let hi = base | mask;
    let mut best_start = 0u32;
    let mut best_len = 0u64;
    let mut cursor = lo;
    for &u in used_sorted {
        if u < lo {
            continue;
        }
        if u > hi {
            break;
        }
        if u > cursor {
            let len = u64::from(u - cursor);
            if len > best_len {
                best_len = len;
                best_start = cursor;
            }
        }
        cursor = cursor.max(u.saturating_add(1));
    }
    if cursor <= hi {
        let len = u64::from(hi - cursor) + 1;
        if len > best_len {
            best_len = len;
            best_start = cursor;
        }
    }
    if best_len == 0 {
        (0, 1)
    } else {
        #[allow(clippy::cast_possible_truncation)]
        (best_start, best_len as u32) // len ≤ hi - lo + 1 ≤ hi ≤ u32::MAX
    }
}

/// XC-MISC (major opcode 152) — XID recycling for long-lived clients.
/// Spec docs/superpowers/specs/2026-06-12-xcmisc-design.md; Xorg ref
/// Xext/xcmisc.c. Length validation lives here because the top-level
/// exact-length table is core-opcode-only.
pub(super) fn handle_xcmisc_request(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    use yserver_protocol::x11::ClientByteOrder;
    let byte_order = state
        .clients
        .get(&client_id.0)
        .map_or(ClientByteOrder::LittleEndian, |c| c.byte_order);
    let minor = header.data;
    match minor {
        // GetVersion — req 8 bytes (client version, ignored).
        0 => {
            if body.len() != 4 {
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
            let mut buf: Vec<u8> = Vec::with_capacity(32);
            x11::write_xcmisc_get_version_reply(&mut buf, byte_order, sequence)?;
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            Ok(write_to_client(client, client_id, &buf))
        }
        // GetXIDRange — req 4 bytes (header only).
        1 => {
            if !body.is_empty() {
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
            let Some((base, mask)) = state
                .clients
                .get(&client_id.0)
                .map(|c| (c.resource_id_base, c.resource_id_mask))
            else {
                return Ok(RequestOutcome::Handled);
            };
            let used = state.used_xids_in(base, mask);
            let (start_id, count) = largest_free_xid_gap(base, mask, &used);
            log::info!(
                "client {} XC-MISC GetXIDRange → start=0x{start_id:x} count={count} \
                 ({} ids in use)",
                client_id.0,
                used.len(),
            );
            let mut buf: Vec<u8> = Vec::with_capacity(32);
            x11::write_xcmisc_get_xid_range_reply(&mut buf, byte_order, sequence, start_id, count)?;
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            Ok(write_to_client(client, client_id, &buf))
        }
        // GetXIDList — req 8 bytes: count(4).
        2 => {
            if body.len() != 4 {
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
            let want = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
            let Some((base, mask)) = state
                .clients
                .get(&client_id.0)
                .map(|c| (c.resource_id_base, c.resource_id_mask))
            else {
                return Ok(RequestOutcome::Handled);
            };
            // Clamp to the range size — never allocate
            // client-controlled gigabytes (explicit Xorg deviation,
            // spec "Edge cases"; Xorg would BadAlloc instead).
            let limit = u64::from(want).min(u64::from(mask) + 1);
            let mut ids: Vec<u32> = Vec::new();
            let lo = base.max(1);
            let hi = base | mask;
            let mut id = lo;
            while ids.len() < limit as usize {
                if !state.xid_occupied(id) {
                    ids.push(id);
                }
                if id == hi {
                    break;
                }
                id += 1;
            }
            log::info!(
                "client {} XC-MISC GetXIDList want={want} → {} ids",
                client_id.0,
                ids.len(),
            );
            let mut buf: Vec<u8> = Vec::with_capacity(32 + ids.len() * 4);
            x11::write_xcmisc_get_xid_list_reply(&mut buf, byte_order, sequence, &ids)?;
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            Ok(write_to_client(client, client_id, &buf))
        }
        other => emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_REQUEST,
            0,
            u16::from(other),
            header.opcode,
        ),
    }
}
