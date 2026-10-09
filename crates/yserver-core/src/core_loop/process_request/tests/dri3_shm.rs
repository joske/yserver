use super::*;

#[test]
fn remote_clients_are_rejected_before_dri3_and_mit_shm_handlers() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    state.clients.get_mut(&1).expect("test client").is_local = false;
    let mut backend = RecordingBackend::new();

    for (sequence, minor) in (0u8..=11).enumerate() {
        #[allow(clippy::cast_possible_truncation)]
        let sequence = sequence as u16 + 1;
        process_request(
            &mut state,
            &mut backend,
            ClientId(1),
            SequenceNumber(sequence),
            RequestHeader {
                opcode: 147,
                data: minor,
                length_units: 1,
            },
            &[],
            None,
        )
        .expect("dispatch");
        let reply = read_all_or_buffered(&mut state, 1, &mut peer);
        assert_eq!(reply.len(), 32);
        assert_eq!(reply[0], 0);
        assert_eq!(reply[1], x11::error::BAD_MATCH);
        assert_eq!(
            u16::from_le_bytes(reply[8..10].try_into().unwrap()),
            u16::from(minor)
        );
        assert_eq!(reply[10], 147);
    }

    for (sequence, minor) in (1u8..=7).enumerate() {
        #[allow(clippy::cast_possible_truncation)]
        let sequence = sequence as u16 + 20;
        process_request(
            &mut state,
            &mut backend,
            ClientId(1),
            SequenceNumber(sequence),
            RequestHeader {
                opcode: 130,
                data: minor,
                length_units: 1,
            },
            &[],
            None,
        )
        .expect("dispatch");
        let reply = read_all_or_buffered(&mut state, 1, &mut peer);
        assert_eq!(reply.len(), 32);
        assert_eq!(reply[0], 0);
        assert_eq!(reply[1], x11::error::BAD_REQUEST);
        assert_eq!(
            u16::from_le_bytes(reply[8..10].try_into().unwrap()),
            u16::from(minor)
        );
        assert_eq!(reply[10], 130);
    }
}

#[test]
fn remote_mit_shm_query_version_remains_reachable() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    state.clients.get_mut(&1).expect("test client").is_local = false;
    let mut backend = RecordingBackend::new();

    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode: 130,
            data: 0,
            length_units: 1,
        },
        &[],
        None,
    )
    .expect("ShmQueryVersion dispatch");
    let reply = read_all_or_buffered(&mut state, 1, &mut peer);
    assert_eq!(reply[0], 1, "ShmQueryVersion must not be locality-gated");
}

#[test]
fn local_clients_reach_dri3_and_mit_shm_handlers() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = syncobj_cap_backend();

    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode: 147,
            data: yserver_protocol::x11::dri3::QUERY_VERSION,
            length_units: 3,
        },
        &[1, 0, 0, 0, 4, 0, 0, 0],
        None,
    )
    .expect("DRI3::QueryVersion dispatch");
    let reply = read_all_or_buffered(&mut state, 1, &mut peer);
    assert_eq!(reply[0], 1, "local DRI3 request reaches its handler");

    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(2),
        RequestHeader {
            opcode: 130,
            data: yserver_protocol::x11::mit_shm::ATTACH,
            length_units: 1,
        },
        &[],
        None,
    )
    .expect("MIT-SHM Attach dispatch");
    let reply = read_all_or_buffered(&mut state, 1, &mut peer);
    assert_eq!(reply[0], 0, "malformed local request reaches the handler");
    assert_eq!(reply[1], x11::error::BAD_LENGTH);
}

#[test]
fn remote_client_can_query_present() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    state.clients.get_mut(&1).expect("test client").is_local = false;
    let mut backend = RecordingBackend::new();

    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode: 145,
            data: yserver_protocol::x11::present::QUERY_VERSION,
            length_units: 1,
        },
        &[],
        None,
    )
    .expect("Present::QueryVersion dispatch");
    let reply = read_all_or_buffered(&mut state, 1, &mut peer);
    assert_eq!(reply[0], 1, "Present must not be locality-gated");
}

#[test]
fn locality_gated_extensions_remain_advertised() {
    let mut backend = syncobj_cap_backend();
    let names = advertised_extension_names(&mut backend);

    for name in ["MIT-SHM", "DRI3", "XFree86-VidModeExtension"] {
        assert!(
            extension_query_reply(name, &mut backend).is_some(),
            "{name} must be advertised before its per-client locality gate"
        );
        assert!(
            names.contains(&name),
            "{name} must remain in ListExtensions"
        );
    }
}

#[test]
fn fd_reply_refuses_a_client_without_fd_passing() {
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let client = state.clients.get_mut(&1).expect("test client");
    client.fd_passing = false;
    let file = std::fs::File::open("/dev/null").expect("open null device");

    let error = send_reply_with_fd(client, &[1; 32], std::os::fd::AsRawFd::as_raw_fd(&file))
        .expect_err("non-FD-capable clients must reject descriptor replies");
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);
}

#[test]
fn dri3_hidden_when_caps_unsupported() {
    // Default RecordingBackend returns Dri3Caps::unsupported()
    // (version (0, 0)), so DRI3 must not appear in QueryExtension /
    // ListExtensions output. KmsBackend overrides
    // dri3_capabilities() and the Phase 4.2 vng smoke covers the
    // advertised path.
    let mut backend = RecordingBackend::new();
    assert!(extension_query_reply("DRI3", &mut backend).is_none());
    let names = advertised_extension_names(&mut backend);
    assert!(
        !names.contains(&"DRI3"),
        "DRI3 should be hidden when caps unsupported, got: {names:?}"
    );
}

/// Build an ImportSyncobj request body: syncobj(4) + drawable(4), fd via
/// SCM_RIGHTS (attached_fd).
fn import_syncobj_body(syncobj: u32, drawable: u32) -> Vec<u8> {
    let mut body = vec![0u8; 8];
    body[0..4].copy_from_slice(&syncobj.to_le_bytes());
    body[4..8].copy_from_slice(&drawable.to_le_bytes());
    body
}

/// RecordingBackend with a DRI3 1.4 / `syncobj: true` surface so the
/// IMPORT_SYNCOBJ / FREE_SYNCOBJ handlers pass their `caps.syncobj` gate.
/// Kept as a per-test opt-in: the DEFAULT backend must stay
/// `Dri3Caps::unsupported()` for the existing
/// `dri3_hidden_when_caps_unsupported` test (process_request.rs:37076).
fn syncobj_cap_backend() -> RecordingBackend {
    let mut backend = RecordingBackend::new();
    backend.dri3_caps = crate::backend::Dri3Caps {
        version: (1, 4),
        modifiers: false,
        fence_fd: false,
        syncobj: true,
    };
    backend
}

#[test]
fn import_syncobj_in_use_xid_is_bad_id_choice() {
    use yserver_protocol::x11::{CreatePixmapRequest, ResourceId};
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = syncobj_cap_backend();

    // install_client (process_request.rs:29047) gives every test client
    // base=0/mask=u32::MAX — "every xid is in range" — so the
    // xid_out_of_client_range half of the LEGAL_NEW_RESOURCE gate is
    // unreachable in this fixture. Exercise the OTHER half: name a
    // pixmap with the xid, then ImportSyncobj with the same xid ->
    // resources.xid_in_use fires.
    const XID: u32 = 0x0040_0001;
    state.resources.create_pixmap(
        ClientId(1),
        CreatePixmapRequest {
            depth: 24,
            pixmap: ResourceId(XID),
            drawable: ROOT_WINDOW,
            width: 1,
            height: 1,
        },
    );

    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode: 147,
            data: 10, // IMPORT_SYNCOBJ
            length_units: 3,
        },
        // drawable=0 is fine here: the xid check (Xorg LEGAL_NEW_RESOURCE)
        // fires before the drawable check.
        &import_syncobj_body(XID, 0),
        None, // no SCM_RIGHTS fd
    )
    .unwrap();
    peer.set_nonblocking(true).unwrap();
    let mut buf = [0u8; 32];
    peer.read_exact(&mut buf).unwrap();
    assert_eq!(
        read_error(&buf),
        x11::error::BAD_ID_CHOICE,
        "an in-use xid must be rejected as BadIDChoice (Xorg LEGAL_NEW_RESOURCE)",
    );
}

#[test]
fn import_syncobj_bad_drawable_is_bad_drawable() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = syncobj_cap_backend();

    // A legal syncobj xid (never used, so no xid_in_use) naming a drawable
    // that does not exist -> the Xorg drawable check (dixLookupDrawable)
    // must fire with BadDrawable. It runs AFTER the xid check and BEFORE
    // the fd check.
    const SYNCOBJ: u32 = 0x0010_0001;
    const MISSING_DRAWABLE: u32 = 0x0050_0001;
    assert!(
        state
            .resources
            .window(ResourceId(MISSING_DRAWABLE))
            .is_none()
            && state
                .resources
                .pixmap(ResourceId(MISSING_DRAWABLE))
                .is_none(),
        "fixture: MISSING_DRAWABLE must not exist"
    );

    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode: 147,
            data: 10, // IMPORT_SYNCOBJ
            length_units: 3,
        },
        &import_syncobj_body(SYNCOBJ, MISSING_DRAWABLE),
        None, // no SCM_RIGHTS fd
    )
    .unwrap();
    peer.set_nonblocking(true).unwrap();
    let mut buf = [0u8; 32];
    peer.read_exact(&mut buf).unwrap();
    assert_eq!(
        read_error(&buf),
        x11::error::BAD_DRAWABLE,
        "ImportSyncobj naming a nonexistent drawable must be BadDrawable",
    );
    // The bad drawable must abort the request: nothing imported.
    assert!(!backend.dri3_syncobj_owners.contains_key(&SYNCOBJ));
}

#[test]
fn import_syncobj_missing_fd_is_bad_value() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = syncobj_cap_backend();

    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode: 147,
            data: 10, // IMPORT_SYNCOBJ
            length_units: 3,
        },
        // legal xid; install_client is range-permissive. drawable=ROOT_WINDOW
        // (0x100) exists in the default ResourceTable, so the request passes
        // the xid + drawable checks and lands on the fd check (Xorg:
        // ReadFdFromClient < 0 -> BadValue).
        &import_syncobj_body(0x0010_0001, ROOT_WINDOW.0),
        None, // no fd attached
    )
    .unwrap();
    peer.set_nonblocking(true).unwrap();
    let mut buf = [0u8; 32];
    peer.read_exact(&mut buf).unwrap();
    assert_eq!(
        read_error(&buf),
        x11::error::BAD_VALUE,
        "ImportSyncobj without an fd must be BadValue (Xorg ReadFdFromClient)",
    );
}

#[test]
fn free_syncobj_unknown_is_bad_value() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = syncobj_cap_backend();

    let mut body = vec![0u8; 4];
    body[0..4].copy_from_slice(&0x0040_9999u32.to_le_bytes());
    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode: 147,
            data: 11, // FREE_SYNCOBJ
            length_units: 2,
        },
        &body,
        None,
    )
    .unwrap();
    peer.set_nonblocking(true).unwrap();
    let mut buf = [0u8; 32];
    peer.read_exact(&mut buf).unwrap();
    assert_eq!(
        read_error(&buf),
        x11::error::BAD_VALUE,
        "FreeSyncobj of an unknown xid must be BadValue",
    );
}

#[test]
fn free_syncobj_of_another_client_is_bad_access() {
    let mut state = ServerState::new();
    let _peer_a = install_client(&mut state, 1);
    let mut peer_b = install_client(&mut state, 2);
    let mut backend = syncobj_cap_backend();

    // Client A imports 0x0010_0001 (a legal xid for client 1 — the
    // handler now validates range). drawable=ROOT_WINDOW exists, so the
    // request passes the xid + drawable checks. RecordingBackend ignores
    // the fd's payload, only ownership is recorded.
    let fd = std::fs::File::open("/dev/null")
        .expect("open /dev/null")
        .into();
    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode: 147,
            data: 10,
            length_units: 3,
        },
        &import_syncobj_body(0x0010_0001, ROOT_WINDOW.0),
        Some(fd),
    )
    .unwrap();

    // Client B frees A's syncobj. FreeSyncobj has no range check (it is
    // not a new-resource request), so B can name A's xid — the ownership
    // check is what must reject it. Xorg's dixLookupResourceByType with
    // DixWriteAccess maps a foreign owner to BadAccess.
    let mut body = vec![0u8; 4];
    body[0..4].copy_from_slice(&0x0010_0001u32.to_le_bytes());
    process_request(
        &mut state,
        &mut backend,
        ClientId(2),
        SequenceNumber(1),
        RequestHeader {
            opcode: 147,
            data: 11,
            length_units: 2,
        },
        &body,
        None,
    )
    .unwrap();
    peer_b.set_nonblocking(true).unwrap();
    let mut buf = [0u8; 32];
    peer_b.read_exact(&mut buf).unwrap();
    assert_eq!(
        read_error(&buf),
        x11::error::BAD_ACCESS,
        "FreeSyncobj of another client's syncobj must be BadAccess",
    );

    // A still owns it (ImportSyncobj is a void request — nothing to read
    // on A's socket; the ownership assertion is the whole check).
    assert!(backend.dri3_syncobj_owners.contains_key(&0x0010_0001));
}

#[test]
fn import_syncobj_duplicate_xid_is_bad_id_choice() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = syncobj_cap_backend();

    const XID: u32 = 0x0010_0001;
    let fd1: std::os::fd::OwnedFd = std::fs::File::open("/dev/null")
        .expect("open /dev/null")
        .into();
    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode: 147,
            data: 10, // IMPORT_SYNCOBJ
            length_units: 3,
        },
        &import_syncobj_body(XID, ROOT_WINDOW.0),
        Some(fd1),
    )
    .unwrap();

    // Re-import of the SAME xid is rejected by the core X resource
    // registry before the backend is touched, matching Xorg's
    // LEGAL_NEW_RESOURCE gate.
    let fd2: std::os::fd::OwnedFd = std::fs::File::open("/dev/null")
        .expect("open /dev/null")
        .into();
    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(2),
        RequestHeader {
            opcode: 147,
            data: 10, // IMPORT_SYNCOBJ
            length_units: 3,
        },
        &import_syncobj_body(XID, ROOT_WINDOW.0),
        Some(fd2),
    )
    .unwrap();

    peer.set_nonblocking(true).unwrap();
    let mut buf = [0u8; 32];
    peer.read_exact(&mut buf).unwrap();
    assert_eq!(
        read_error(&buf),
        x11::error::BAD_ID_CHOICE,
        "re-importing a live syncobj XID must be BadIDChoice",
    );
}

#[test]
fn live_syncobj_xid_cannot_be_reused_for_pixmap() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = syncobj_cap_backend();
    const XID: u32 = 0x0010_0001;

    let fd: std::os::fd::OwnedFd = std::fs::File::open("/dev/null")
        .expect("open /dev/null")
        .into();
    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode: 147,
            data: 10,
            length_units: 3,
        },
        &import_syncobj_body(XID, ROOT_WINDOW.0),
        Some(fd),
    )
    .unwrap();
    assert!(state.resources.xid_in_use(ResourceId(XID)));

    let mut body = vec![0u8; 12];
    body[0..4].copy_from_slice(&XID.to_le_bytes());
    body[4..8].copy_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    body[8..10].copy_from_slice(&1u16.to_le_bytes());
    body[10..12].copy_from_slice(&1u16.to_le_bytes());
    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(2),
        RequestHeader {
            opcode: 53, // CreatePixmap
            data: 24,
            length_units: 4,
        },
        &body,
        None,
    )
    .unwrap();

    peer.set_nonblocking(true).unwrap();
    let mut error = [0u8; 32];
    peer.read_exact(&mut error).unwrap();
    assert_eq!(read_error(&error), x11::error::BAD_ID_CHOICE);
}

#[test]
fn kill_client_by_retained_syncobj_xid_destroys_zombie_resources() {
    const SYNCOBJ: u32 = 0x0070_0094;
    let mut state = ServerState::new();
    let _killer_peer = install_client(&mut state, 1);
    let _owner_peer = install_client(&mut state, 7);
    let mut backend = RecordingBackend::new();
    state.close_down_modes.insert(7, 1); // RetainPermanent
    assert!(
        state
            .resources
            .register_dri3_syncobj(ResourceId(SYNCOBJ), ClientId(7))
    );
    backend.seed_dri3_syncobj_for_test(SYNCOBJ, ClientId(7));
    crate::core_loop::process_disconnect::process_disconnect(&mut state, &mut backend, ClientId(7));
    assert_eq!(state.zombie_clients.get(&7), Some(&1));

    let outcome = process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode: 113, // KillClient
            data: 0,
            length_units: 2,
        },
        &SYNCOBJ.to_le_bytes(),
        None,
    )
    .unwrap();

    assert!(matches!(outcome, RequestOutcome::Handled));
    assert!(!state.resources.xid_in_use(ResourceId(SYNCOBJ)));
    assert!(!backend.dri3_syncobj_owners.contains_key(&SYNCOBJ));
    assert!(!state.zombie_clients.contains_key(&7));
}

#[test]
fn kill_client_all_temporary_destroys_retained_syncobj() {
    const SYNCOBJ: u32 = 0x0070_0095;
    let mut state = ServerState::new();
    let _killer_peer = install_client(&mut state, 1);
    let _owner_peer = install_client(&mut state, 7);
    let mut backend = RecordingBackend::new();
    state.close_down_modes.insert(7, 2); // RetainTemporary
    assert!(
        state
            .resources
            .register_dri3_syncobj(ResourceId(SYNCOBJ), ClientId(7))
    );
    backend.seed_dri3_syncobj_for_test(SYNCOBJ, ClientId(7));
    crate::core_loop::process_disconnect::process_disconnect(&mut state, &mut backend, ClientId(7));
    assert_eq!(state.zombie_clients.get(&7), Some(&2));

    let outcome = process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode: 113, // KillClient
            data: 0,
            length_units: 2,
        },
        &0u32.to_le_bytes(), // AllTemporary
        None,
    )
    .unwrap();

    assert!(matches!(outcome, RequestOutcome::Handled));
    assert!(!state.resources.xid_in_use(ResourceId(SYNCOBJ)));
    assert!(!backend.dri3_syncobj_owners.contains_key(&SYNCOBJ));
    assert!(!state.zombie_clients.contains_key(&7));
}

#[test]
fn dri3_get_supported_modifiers_default_window_subset_screen() {
    // Default Backend::dri3_supported_modifiers returns
    // ([LINEAR], [LINEAR]); the window list must always be a
    // subset of the screen list per design §3.2.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let mut body = vec![0u8; 8];
    body[0..4].copy_from_slice(&0xCAFEu32.to_le_bytes()); // window
    body[4] = 24;
    body[5] = 32;
    let outcome = process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(11),
        RequestHeader {
            opcode: 147,
            data: 6, // GET_SUPPORTED_MODIFIERS
            length_units: 3,
        },
        &body,
        None,
    )
    .unwrap();
    assert!(matches!(outcome, RequestOutcome::Handled));
    peer.set_nonblocking(true).unwrap();
    // Reply: 32-byte header + 8*nwin + 8*nscr; nwin=nscr=1.
    let mut buf = [0u8; 48];
    peer.read_exact(&mut buf).expect("reply bytes delivered");
    assert_eq!(buf[0], 1, "expected Reply, got opcode {}", buf[0]);
    let nwin = u32::from_le_bytes(buf[8..12].try_into().unwrap());
    let nscr = u32::from_le_bytes(buf[12..16].try_into().unwrap());
    assert!(nwin <= nscr, "window-list must be subset: {nwin} > {nscr}");
    assert_eq!(nwin, 1);
    assert_eq!(nscr, 1);
    let m_win = u64::from_le_bytes(buf[32..40].try_into().unwrap());
    let m_scr = u64::from_le_bytes(buf[40..48].try_into().unwrap());
    assert_eq!(m_win, 0); // LINEAR
    assert_eq!(m_scr, 0);
}

#[test]
fn dri3_pixmap_from_buffers_rejects_multi_plane() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    // 60-byte body with num_buffers=2 (multi-plane). Phase 4.2
    // accepts only num_buffers==1; everything else returns
    // BadAlloc per design "single-plane RGB only" scope.
    let mut body = vec![0u8; 60];
    body[0..4].copy_from_slice(&0xAAAA_AAAAu32.to_le_bytes()); // pixmap xid
    body[4..8].copy_from_slice(&0xBBBB_BBBBu32.to_le_bytes()); // window xid
    body[8] = 2; // num_buffers
    let outcome = process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(5),
        RequestHeader {
            opcode: 147,
            data: 7, // PIXMAP_FROM_BUFFERS
            length_units: 16,
        },
        &body,
        None,
    )
    .unwrap();
    assert!(matches!(outcome, RequestOutcome::Handled));
    peer.set_nonblocking(true).unwrap();
    let mut buf = [0u8; 32];
    peer.read_exact(&mut buf).expect("error bytes delivered");
    assert_eq!(buf[0], 0, "expected Error, got opcode {}", buf[0]);
    assert_eq!(buf[1], 11, "expected BadAlloc (11), got {}", buf[1]);
}

#[test]
fn dri3_open_emits_bad_alloc_when_backend_unsupported() {
    // RecordingBackend's default Backend::dri3_open returns Err
    // (DRI3 unsupported). The dispatcher must convert that to a
    // BadAlloc error event, not a successful reply.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let mut body = vec![0u8; 8];
    body[0..4].copy_from_slice(&0xCAFEu32.to_le_bytes()); // drawable
    body[4..8].copy_from_slice(&0u32.to_le_bytes()); // provider
    let outcome = process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(3),
        RequestHeader {
            opcode: 147,
            data: 1, // OPEN
            length_units: 3,
        },
        &body,
        None,
    )
    .unwrap();
    assert!(matches!(outcome, RequestOutcome::Handled));
    // Read 32 bytes from the peer end of the client's UnixStream:
    // X errors are 32-byte fixed-size messages with opcode 0 in
    // byte 0 and the error code in byte 1.
    peer.set_nonblocking(true).unwrap();
    let mut buf = [0u8; 32];
    peer.read_exact(&mut buf)
        .expect("error bytes delivered to peer");
    assert_eq!(buf[0], 0, "expected Error (opcode 0), got {}", buf[0]);
    assert_eq!(buf[1], 11, "expected BadAlloc (11), got {}", buf[1]);
}

// ---------------- extract_shm_zpixmap_region ----------------
//
// The protocol-level byte-extraction for MIT-SHM PutImage. Until
// 2026-05-15 the handler ignored `total_width` / `src_x` /
// `src_y` and read a tightly-packed `src_width × src_height ×
// bpp` block at `offset`. That happens to be correct when the
// client sets `total == src` and `src_xy == 0` (the common
// case), but reads garbage when the client uploads a sub-region
// of a larger shm image. xfdesktop's thumbnail upload pattern
// and any cairo-padded surface trigger the latter, which
// manifests as horizontal-band corruption around the thumbnail.

#[test]
fn mit_shm_put_image_honors_gc_clip_rectangles() {
    const CLIENT: u32 = 1;
    const PIXMAP: u32 = 0x0020_0001;
    const GC: u32 = 0x0020_0002;
    const SHMSEG: u32 = 0x0020_0003;
    const HOST_PIXMAP: u32 = 0x0040_0001;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    state.resources.create_pixmap(
        ClientId(CLIENT),
        x11::CreatePixmapRequest {
            depth: 32,
            pixmap: ResourceId(PIXMAP),
            drawable: ROOT_WINDOW,
            width: 4,
            height: 4,
        },
    );
    assert!(state.resources.set_pixmap_host_xid(
        ResourceId(PIXMAP),
        crate::backend::PixmapHandle::from_raw(HOST_PIXMAP).expect("non-zero host pixmap"),
    ));
    state.resources.create_gc(
        ClientId(CLIENT),
        CreateGcRequest {
            gc: ResourceId(GC),
            drawable: ResourceId(PIXMAP),
            function: None,
            plane_mask: None,
            foreground: None,
            background: None,
            line_width: None,
            line_style: None,
            cap_style: None,
            join_style: None,
            fill_style: None,
            fill_rule: None,
            tile: None,
            stipple: None,
            tile_x_origin: None,
            tile_y_origin: None,
            font: None,
            subwindow_mode: None,
            graphics_exposures: None,
            clip_x_origin: None,
            clip_y_origin: None,
            clip_mask: None,
            dash_offset: None,
            dashes: None,
            arc_mode: None,
        },
    );
    let clip = x11::ClipRectangles {
        ordering: 0,
        x_origin: 0,
        y_origin: 0,
        rectangles: [
            0i16.to_le_bytes(),
            1i16.to_le_bytes(),
            4u16.to_le_bytes(),
            1u16.to_le_bytes(),
        ]
        .concat(),
    };
    state.resources.set_clip_rectangles(
        ClientId(CLIENT),
        x11::SetClipRectanglesRequest {
            gc: ResourceId(GC),
            clip: clip.clone(),
        },
    );

    let fd = unsafe { libc::memfd_create(c"mit-shm-put-image-test".as_ptr(), libc::MFD_CLOEXEC) };
    assert!(fd >= 0, "memfd_create failed");
    assert_eq!(unsafe { libc::ftruncate(fd, 64) }, 0, "ftruncate failed");
    let mut segment = crate::server::MitShmSegment::from_fd(ClientId(CLIENT), fd, false)
        .expect("map test SHM segment");
    segment
        .as_mut_slice()
        .expect("writable SHM segment")
        .fill(0xff);
    state.mit_shm_segments.insert(SHMSEG, segment);

    handle_mit_shm_put_image(
        &mut state,
        &mut backend,
        None,
        ClientId(CLIENT),
        SequenceNumber(1),
        yserver_protocol::x11::mit_shm::PutImageRequest {
            drawable: PIXMAP,
            gc: GC,
            total_width: 4,
            total_height: 4,
            src_x: 0,
            src_y: 0,
            src_width: 4,
            src_height: 4,
            dst_x: 0,
            dst_y: 0,
            depth: 32,
            format: 2,
            send_event: false,
            shmseg: SHMSEG,
            offset: 0,
        },
    )
    .expect("MIT-SHM PutImage succeeds");

    assert!(backend.calls.lock().expect("calls lock").contains(
        &crate::backend::recording::RecordedCall::ApplyClipState(
            crate::backend::ClipState::Rectangles {
                origin: (0, 0),
                rects: clip,
            },
        ),
    ));
}

fn d32_pixel(b: u8, g: u8, r: u8, a: u8) -> [u8; 4] {
    // Wire byte order for a depth-32 ZPixmap is [B, G, R, A] —
    // see the comment in `try_vk_put_image`.
    [b, g, r, a]
}

#[test]
fn extract_d32_tight_packing_returns_input() {
    // total == src, src_xy == 0 → output is exactly the input.
    let mut bytes = Vec::new();
    for y in 0..4u8 {
        for x in 0..4u8 {
            bytes.extend_from_slice(&d32_pixel(x, y, x ^ y, 0xFF));
        }
    }
    let out = extract_shm_zpixmap_region(&bytes, 0, 4, 4, 0, 0, 4, 4, 32)
        .expect("tight extraction succeeds");
    assert_eq!(out, bytes);
}

/// 2026-05-28 perf-cliff fix: full-frame extracts MUST return
/// `Cow::Borrowed` (no allocation, no memcpy). On cinnamon the
/// hot-path PutImage of the 28MB compositor stage was spending
/// 8-10ms in the legacy per-row Vec::extend_from_slice loop.
/// Borrowing the SHM slice directly drops it to a bounds-check.
/// Tested both depth=24 and depth=32 at the boundary (full frame,
/// no offset, row_bytes == src_stride) and against a partial /
/// padded case that MUST still allocate.
#[test]
fn extract_full_frame_d32_returns_borrowed_slice() {
    // 8×4 full-frame extract, depth 32 → row_bytes == src_stride.
    let mut bytes = Vec::new();
    for y in 0..4u8 {
        for x in 0..8u8 {
            bytes.extend_from_slice(&d32_pixel(x, y, x ^ y, 0xFF));
        }
    }
    let out = extract_shm_zpixmap_region(&bytes, 0, 8, 4, 0, 0, 8, 4, 32)
        .expect("full-frame d32 extraction succeeds");
    assert!(
        matches!(out, std::borrow::Cow::Borrowed(_)),
        "full-frame extract must zero-copy (Cow::Borrowed)",
    );
    assert_eq!(out.as_ref(), bytes.as_slice());
}

#[test]
fn extract_full_frame_d24_returns_borrowed_slice() {
    // 8×4 full-frame, depth 24 (4 bytes per pixel — same as d32).
    let mut bytes = Vec::new();
    for y in 0..4u8 {
        for x in 0..8u8 {
            bytes.extend_from_slice(&d32_pixel(x, y, x ^ y, 0xFF));
        }
    }
    let out = extract_shm_zpixmap_region(&bytes, 0, 8, 4, 0, 0, 8, 4, 24)
        .expect("full-frame d24 extraction succeeds");
    assert!(
        matches!(out, std::borrow::Cow::Borrowed(_)),
        "full-frame d24 extract must zero-copy (Cow::Borrowed)",
    );
    assert_eq!(out.as_ref(), bytes.as_slice());
}

#[test]
fn extract_partial_region_returns_owned_vec() {
    // Same data but ask for a sub-region — the borrow fast path
    // MUST NOT trigger.
    let mut bytes = Vec::new();
    for y in 0..8u8 {
        for x in 0..8u8 {
            bytes.extend_from_slice(&d32_pixel(x, y, 0, 0xFF));
        }
    }
    let out = extract_shm_zpixmap_region(&bytes, 0, 8, 8, 2, 2, 4, 4, 32)
        .expect("partial extraction succeeds");
    assert!(
        matches!(out, std::borrow::Cow::Owned(_)),
        "partial extract must allocate (Cow::Owned)",
    );
}

#[test]
fn extract_d8_padded_full_frame_does_not_borrow() {
    // Depth-8 with src_width=5 → src_stride=8 (pad-to-32-bit).
    // row_bytes (5) < src_stride (8) — the borrow fast path's
    // `row_bytes == src_stride` precondition excludes this so
    // the slow padding path runs.
    let bytes: Vec<u8> = (0..16).map(|i| i as u8).collect();
    let out = extract_shm_zpixmap_region(&bytes, 0, 5, 2, 0, 0, 5, 2, 8)
        .expect("padded d8 extraction succeeds");
    assert!(
        matches!(out, std::borrow::Cow::Owned(_)),
        "padded full-frame d8 must NOT borrow (per-row padding required)",
    );
}

#[test]
fn extract_d32_subregion_of_larger_buffer() {
    // 8×8 total, take 4×4 at (2, 2). The handler must read row 2
    // bytes 8..24, row 3 bytes 8..24, etc. — NOT 64 contiguous
    // bytes from offset 0, which is what the pre-fix handler
    // did.
    let mut bytes = Vec::new();
    for y in 0..8u8 {
        for x in 0..8u8 {
            bytes.extend_from_slice(&d32_pixel(x, y, 0, 0xFF));
        }
    }
    let out = extract_shm_zpixmap_region(&bytes, 0, 8, 8, 2, 2, 4, 4, 32)
        .expect("subregion extraction succeeds");
    // Output must be the (2,2)..(6,6) tile, tightly packed.
    let mut want = Vec::new();
    for y in 2..6u8 {
        for x in 2..6u8 {
            want.extend_from_slice(&d32_pixel(x, y, 0, 0xFF));
        }
    }
    assert_eq!(out, want);
}

#[test]
fn extract_d32_total_wider_than_src() {
    // 16×4 total, take 4×4 at (0, 0). Each src row must skip the
    // unused 12 trailing pixels of the total row.
    let mut bytes = Vec::new();
    for y in 0..4u8 {
        for x in 0..16u8 {
            bytes.extend_from_slice(&d32_pixel(x, y, 0, 0xFF));
        }
    }
    let out = extract_shm_zpixmap_region(&bytes, 0, 16, 4, 0, 0, 4, 4, 32)
        .expect("wide-total extraction succeeds");
    let mut want = Vec::new();
    for y in 0..4u8 {
        for x in 0..4u8 {
            want.extend_from_slice(&d32_pixel(x, y, 0, 0xFF));
        }
    }
    assert_eq!(out, want);
}

#[test]
fn extract_honors_offset() {
    // Image starts partway into the shm segment.
    let mut bytes = vec![0xAAu8; 16];
    for y in 0..2u8 {
        for x in 0..2u8 {
            bytes.extend_from_slice(&d32_pixel(x, y, 0xFF, 0xFF));
        }
    }
    let out = extract_shm_zpixmap_region(&bytes, 16, 2, 2, 0, 0, 2, 2, 32)
        .expect("offset extraction succeeds");
    assert_eq!(out, &bytes[16..]);
}

#[test]
fn extract_d8_padding_total_wider_than_src() {
    // 6-wide total → 8 bytes/row (padded to 32-bit). Take a 5×2
    // tile. Output stride is `((5 + 3) & ~3) = 8` bytes/row so
    // we get 16 bytes; first 5 are image data, next 3 are pad.
    let bytes: Vec<u8> = (0..16).map(|i| i as u8).collect();
    let out =
        extract_shm_zpixmap_region(&bytes, 0, 6, 2, 0, 0, 5, 2, 8).expect("d8 extraction succeeds");
    assert_eq!(out.len(), 16);
    // Row 0 image bytes: 0..5 of the input; row 0 trailing pad: zero.
    assert_eq!(&out[..5], &bytes[..5]);
    assert_eq!(&out[5..8], &[0, 0, 0]);
    // Row 1 image bytes: 8..13 of the input.
    assert_eq!(&out[8..13], &bytes[8..13]);
    assert_eq!(&out[13..16], &[0, 0, 0]);
}

#[test]
fn extract_d8_src_x_offset() {
    // 8-wide total → 8 bytes/row. Take a 4×1 tile at src_x=2.
    let bytes: Vec<u8> = (0..8).map(|i| i as u8).collect();
    let out = extract_shm_zpixmap_region(&bytes, 0, 8, 1, 2, 0, 4, 1, 8)
        .expect("d8 src_x extraction succeeds");
    // src_width=4 → src_stride=4 (already 32-bit aligned). Output is bytes 2..6.
    assert_eq!(out, vec![2u8, 3, 4, 5]);
}

#[test]
fn extract_rejects_region_outside_total() {
    let bytes = vec![0u8; 64];
    // src_x + src_width > total_width
    assert!(extract_shm_zpixmap_region(&bytes, 0, 4, 4, 2, 0, 4, 4, 32).is_none());
    // src_y + src_height > total_height
    assert!(extract_shm_zpixmap_region(&bytes, 0, 4, 4, 0, 2, 4, 4, 32).is_none());
}

#[test]
fn extract_rejects_negative_src_xy() {
    let bytes = vec![0u8; 64];
    assert!(extract_shm_zpixmap_region(&bytes, 0, 4, 4, -1, 0, 4, 4, 32).is_none());
    assert!(extract_shm_zpixmap_region(&bytes, 0, 4, 4, 0, -1, 4, 4, 32).is_none());
}

#[test]
fn extract_rejects_offset_beyond_buffer() {
    let bytes = vec![0u8; 16];
    // 4×4 d32 = 64 bytes; only 16 available.
    assert!(extract_shm_zpixmap_region(&bytes, 0, 4, 4, 0, 0, 4, 4, 32).is_none());
}

#[test]
fn extract_rejects_unsupported_depth() {
    let bytes = vec![0u8; 64];
    assert!(extract_shm_zpixmap_region(&bytes, 0, 4, 4, 0, 0, 4, 4, 16).is_none());
}

#[test]
fn extract_d24_treats_padding_byte_as_image_data() {
    // d24 on the wire is 4 bytes per pixel like d32; the 4th
    // byte is undefined but transmitted. Extraction must copy
    // all 4 bytes — the put_image backend stamps 0xFF later.
    let mut bytes = Vec::new();
    for y in 0..2u8 {
        for x in 0..2u8 {
            bytes.extend_from_slice(&d32_pixel(x, y, 0xCC, 0x77));
        }
    }
    let out = extract_shm_zpixmap_region(&bytes, 0, 2, 2, 0, 0, 2, 2, 24)
        .expect("d24 extraction succeeds");
    assert_eq!(out, bytes);
}

/// DRI3::BufferFromPixmap: when the backend's `dri3_export_pixmap`
/// returns Err (e.g. no exportable backing, Vulkan unavailable, or
/// any promotion failure), the dispatcher must emit `BadPixmap`
/// (error code 4) — matching Xorg `dri3/dri3_request.c:277`.
/// Previously this emitted `BadAlloc` (11).
#[test]
fn buffer_from_pixmap_export_failure_returns_bad_pixmap() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    // RecordingBackend does not override dri3_export_pixmap, so the
    // default trait implementation returns Err("DRI3 export unsupported
    // on this backend") — exactly the failure case we're testing.
    let mut backend = RecordingBackend::new();

    // Register a pixmap in the resource table with a valid host_xid so
    // the handler reaches the dri3_export_pixmap call (not the
    // BadDrawable path).
    let pixmap_xid: u32 = 0x1000;
    state.resources.create_pixmap(
        ClientId(1),
        CreatePixmapRequest {
            depth: 24,
            pixmap: ResourceId(pixmap_xid),
            drawable: ROOT_WINDOW,
            width: 16,
            height: 16,
        },
    );
    assert!(
        state.resources.set_pixmap_host_xid(
            ResourceId(pixmap_xid),
            crate::backend::PixmapHandle::from_raw(0xdead_cafe).unwrap(),
        ),
        "set_pixmap_host_xid must succeed"
    );

    // BufferFromPixmap body: 4-byte pixmap XID (little-endian).
    let body = pixmap_xid.to_le_bytes().to_vec();

    let outcome = process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(7),
        RequestHeader {
            opcode: 147, // DRI3_MAJOR_OPCODE
            data: 3,     // BUFFER_FROM_PIXMAP
            length_units: 2,
        },
        &body,
        None,
    )
    .expect("process_request should not hard-error");

    assert!(
        matches!(outcome, RequestOutcome::Handled),
        "expected Handled, got {outcome:?}"
    );

    // Read the 32-byte X error packet from the peer socket.
    peer.set_nonblocking(true).unwrap();
    let mut buf = [0u8; 32];
    peer.read_exact(&mut buf)
        .expect("BadPixmap error packet must be delivered to client");
    assert_eq!(buf[0], 0, "byte 0 must be 0 (Error class)");
    assert_eq!(
        buf[1],
        x11::error::BAD_PIXMAP,
        "expected BadPixmap ({}), got {}",
        x11::error::BAD_PIXMAP,
        buf[1]
    );
}

/// DRI3::BuffersFromPixmap (op 8): like op 3, a backend export
/// failure must emit `BadPixmap` (the modifier-aware multi-plane
/// reply shares op 3's error contract). The default RecordingBackend
/// returns Err from `dri3_export_pixmap_buffers`.
#[test]
fn buffers_from_pixmap_export_failure_returns_bad_pixmap() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();

    let pixmap_xid: u32 = 0x1000;
    state.resources.create_pixmap(
        ClientId(1),
        CreatePixmapRequest {
            depth: 24,
            pixmap: ResourceId(pixmap_xid),
            drawable: ROOT_WINDOW,
            width: 16,
            height: 16,
        },
    );
    assert!(state.resources.set_pixmap_host_xid(
        ResourceId(pixmap_xid),
        crate::backend::PixmapHandle::from_raw(0xdead_cafe).unwrap(),
    ));

    let body = pixmap_xid.to_le_bytes().to_vec();
    let outcome = process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(7),
        RequestHeader {
            opcode: 147, // DRI3_MAJOR_OPCODE
            data: 8,     // BUFFERS_FROM_PIXMAP
            length_units: 2,
        },
        &body,
        None,
    )
    .expect("process_request should not hard-error");

    assert!(
        matches!(outcome, RequestOutcome::Handled),
        "expected Handled, got {outcome:?}"
    );

    peer.set_nonblocking(true).unwrap();
    let mut buf = [0u8; 32];
    peer.read_exact(&mut buf)
        .expect("BadPixmap error packet must be delivered to client");
    assert_eq!(buf[0], 0, "byte 0 must be 0 (Error class)");
    assert_eq!(
        buf[1],
        x11::error::BAD_PIXMAP,
        "expected BadPixmap ({}), got {}",
        x11::error::BAD_PIXMAP,
        buf[1]
    );
}
