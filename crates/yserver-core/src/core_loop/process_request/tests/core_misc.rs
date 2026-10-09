use super::*;

fn named_color_body(name: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&0x2000_0001u32.to_le_bytes()); // cmap
    body.extend_from_slice(&u16::try_from(name.len()).unwrap().to_le_bytes());
    body.extend_from_slice(&[0, 0]); // pad
    body.extend_from_slice(name);
    body
}

fn free_colors_body(cmap: u32, plane_mask: u32, pixels: &[u32]) -> Vec<u8> {
    let mut body = Vec::with_capacity(8 + pixels.len() * 4);
    body.extend_from_slice(&cmap.to_le_bytes());
    body.extend_from_slice(&plane_mask.to_le_bytes());
    for pixel in pixels {
        body.extend_from_slice(&pixel.to_le_bytes());
    }
    body
}

fn dispatch_free_colors(cmap: u32, plane_mask: u32, pixels: &[u32]) -> Vec<u8> {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let body = free_colors_body(cmap, plane_mask, pixels);
    let outcome = process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(0x1234),
        RequestHeader {
            opcode: 88,
            data: 0,
            length_units: u32::try_from(1 + body.len() / 4).unwrap(),
        },
        &body,
        None,
    )
    .expect("process_request");
    assert!(
        matches!(outcome, RequestOutcome::Handled),
        "unexpected outcome: {outcome:?}"
    );
    read_all_available(&mut peer)
}

fn assert_free_colors_error(error: &[u8], code: u8, bad_value: u32) {
    assert_eq!(error.len(), 32, "one fixed-size X11 error");
    assert_eq!(error[0], 0, "Error");
    assert_eq!(error[1], code);
    assert_eq!(&error[2..4], &0x1234u16.to_le_bytes());
    assert_eq!(&error[4..8], &bad_value.to_le_bytes());
    assert_eq!(error[10], 88, "major opcode");
}

#[test]
fn free_colors_is_dispatched_as_a_successful_core_request() {
    let output = dispatch_free_colors(
        crate::resources::ROOT_COLORMAP.0,
        0,
        &[0x00aa_bbcc, 0x0011_2233],
    );
    assert!(output.is_empty(), "successful FreeColors is a void request");
}

#[test]
fn free_colors_accepts_valid_nonzero_plane_mask() {
    let output = dispatch_free_colors(
        crate::resources::ROOT_COLORMAP.0,
        0x0000_000f,
        &[0x0011_2230],
    );
    assert!(output.is_empty());
}

#[test]
fn free_colors_empty_pixel_list_ignores_outside_plane_mask() {
    let output = dispatch_free_colors(crate::resources::ROOT_COLORMAP.0, 0xff00_0000, &[]);
    assert!(output.is_empty());
}

#[test]
fn free_colors_argb_visual_accepts_alpha_bits() {
    let output = dispatch_free_colors(
        crate::resources::ARGB_COLORMAP.0,
        0x0f00_0000,
        &[0xf011_2233],
    );
    assert!(output.is_empty());
}

#[test]
fn free_colors_outside_plane_mask_returns_bad_value() {
    let error = dispatch_free_colors(
        crate::resources::ROOT_COLORMAP.0,
        0xff00_0000,
        &[0x0011_2233],
    );
    assert_free_colors_error(&error, x11::error::BAD_VALUE, 0xff11_2233);
}

#[test]
fn free_colors_outside_pixel_bits_return_bad_value() {
    let error = dispatch_free_colors(crate::resources::ROOT_COLORMAP.0, 0, &[0x8011_2233]);
    assert_free_colors_error(&error, x11::error::BAD_VALUE, 0x8011_2233);
}

#[test]
fn free_colors_unknown_colormap_returns_bad_colormap() {
    let error = dispatch_free_colors(0xdead_beef, 0, &[0]);
    assert_free_colors_error(&error, x11::error::BAD_COLORMAP, 0xdead_beef);
}

#[test]
fn alloc_named_color_unknown_name_returns_bad_name() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let body = named_color_body(b"definitelynotacolor");
    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode: 85,
            data: 0,
            length_units: u32::try_from(1 + body.len().div_ceil(4)).unwrap(),
        },
        &body,
        None,
    )
    .expect("process_request");
    let bytes = read_all_available(&mut peer);
    assert!(bytes.len() >= 32, "expected error reply, got {bytes:02x?}");
    assert_eq!(bytes[1], x11::error::BAD_NAME, "code");
    assert_eq!(bytes[10], 85, "major opcode");
}

#[test]
fn lookup_color_unknown_name_returns_bad_name() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let body = named_color_body(b"definitelynotacolor");
    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode: 92,
            data: 0,
            length_units: u32::try_from(1 + body.len().div_ceil(4)).unwrap(),
        },
        &body,
        None,
    )
    .expect("process_request");
    let bytes = read_all_available(&mut peer);
    assert!(bytes.len() >= 32, "expected error reply, got {bytes:02x?}");
    assert_eq!(bytes[1], x11::error::BAD_NAME, "code");
    assert_eq!(bytes[10], 92, "major opcode");
}

#[test]
fn x_resource_query_clients_lists_connected_clients() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let _peer2 = install_client(&mut state, 2);
    {
        let c = state.clients.get_mut(&1).unwrap();
        c.resource_id_base = 0x0040_0000;
        c.resource_id_mask = 0x001f_ffff;
    }
    {
        let c = state.clients.get_mut(&2).unwrap();
        c.resource_id_base = 0x0080_0000;
        c.resource_id_mask = 0x001f_ffff;
    }
    let mut backend = RecordingBackend::new();
    // X-Resource major op 149, QueryClients minor 1.
    let header = RequestHeader {
        opcode: 149,
        data: 1,
        length_units: 1,
    };
    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        header,
        &[],
        None,
    )
    .expect("process_request");
    let bytes = read_all_available(&mut peer);
    // 32-byte header + 2 clients × 8 bytes.
    assert_eq!(bytes.len(), 48, "two clients listed: {bytes:02x?}");
    assert_eq!(&bytes[8..12], &2u32.to_le_bytes(), "num_clients");
    // Sorted by client id: client 1 then client 2.
    assert_eq!(&bytes[32..36], &0x0040_0000u32.to_le_bytes(), "c1 base");
    assert_eq!(&bytes[36..40], &0x001f_ffffu32.to_le_bytes(), "c1 mask");
    assert_eq!(&bytes[40..44], &0x0080_0000u32.to_le_bytes(), "c2 base");
    assert_eq!(&bytes[44..48], &0x001f_ffffu32.to_le_bytes(), "c2 mask");
}

#[test]
fn x_resource_query_client_resources_counts_by_type() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    {
        let c = state.clients.get_mut(&1).unwrap();
        c.resource_id_base = 0x0040_0000;
        c.resource_id_mask = 0x001f_ffff;
    }
    // Client 1 owns 2 GCs + 1 font.
    state
        .resources
        .seed_gc_for_test(ClientId(1), ResourceId(0x0040_0001));
    state
        .resources
        .seed_gc_for_test(ClientId(1), ResourceId(0x0040_0002));
    state
        .resources
        .seed_font_for_test(ClientId(1), ResourceId(0x0040_0003));

    let mut backend = RecordingBackend::new();
    // QueryClientResources (minor 2), xid = the client's resource base.
    let xid = 0x0040_0000u32;
    let header = RequestHeader {
        opcode: 149,
        data: 2,
        length_units: 2,
    };
    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        header,
        &xid.to_le_bytes(),
        None,
    )
    .expect("process_request");
    let bytes = read_all_available(&mut peer);
    assert!(bytes.len() >= 32, "got {bytes:02x?}");
    let num_types = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    assert_eq!(
        bytes.len(),
        32 + num_types * 8,
        "reply size matches num_types"
    );
    // Resolve each returned type atom back to its name — proves the
    // canonical strings, not just the counts.
    let mut found = std::collections::HashMap::new();
    for i in 0..num_types {
        let off = 32 + i * 8;
        let atom = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap());
        let count = u32::from_le_bytes(bytes[off + 4..off + 8].try_into().unwrap());
        let name = state
            .atoms
            .name(x11::AtomId(atom))
            .unwrap_or("?")
            .to_string();
        found.insert(name, count);
    }
    assert_eq!(found.get("GC"), Some(&2), "GC count; found={found:?}");
    assert_eq!(found.get("FONT"), Some(&1), "FONT count; found={found:?}");
    assert!(
        !found.contains_key("WINDOW"),
        "zero-count types omitted; found={found:?}"
    );
}

#[test]
fn x_resource_query_client_pixmap_bytes_reports_padded_storage() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let _peer2 = install_client(&mut state, 2);
    state.clients.get_mut(&1).unwrap().resource_id_base = 0x0040_0000;
    state.clients.get_mut(&1).unwrap().resource_id_mask = 0x001f_ffff;
    state.clients.get_mut(&2).unwrap().resource_id_base = 0x0080_0000;
    state.clients.get_mut(&2).unwrap().resource_id_mask = 0x001f_ffff;
    state.resources.create_pixmap(
        ClientId(1),
        CreatePixmapRequest {
            pixmap: ResourceId(0x0040_0001),
            drawable: ROOT_WINDOW,
            width: 33,
            height: 2,
            depth: 1,
        },
    );
    state.resources.create_pixmap(
        ClientId(1),
        CreatePixmapRequest {
            pixmap: ResourceId(0x0040_0002),
            drawable: ROOT_WINDOW,
            width: 3,
            height: 2,
            depth: 24,
        },
    );
    state.resources.create_pixmap(
        ClientId(2),
        CreatePixmapRequest {
            pixmap: ResourceId(0x0080_0001),
            drawable: ROOT_WINDOW,
            width: 100,
            height: 100,
            depth: 32,
        },
    );
    let mut backend = RecordingBackend::new();
    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(7),
        RequestHeader {
            opcode: 149,
            data: 3,
            length_units: 2,
        },
        &0x0040_0000u32.to_le_bytes(),
        None,
    )
    .expect("QueryClientPixmapBytes");

    let reply = read_all_available(&mut peer);
    assert_eq!(reply.len(), 32);
    // depth-1: 33 bits -> 8-byte padded row * 2 = 16.
    // depth-24: 3 pixels * 4 bytes * 2 = 24. Total = 40.
    assert_eq!(u32::from_le_bytes(reply[8..12].try_into().unwrap()), 40);
    assert_eq!(u32::from_le_bytes(reply[12..16].try_into().unwrap()), 0);

    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(8),
        RequestHeader {
            opcode: 149,
            data: 3,
            length_units: 2,
        },
        &0x00f0_0000u32.to_le_bytes(),
        None,
    )
    .expect("reject unknown client range");
    let error = read_all_available(&mut peer);
    assert_eq!(error[1], x11::error::BAD_VALUE);
    assert_eq!(
        u32::from_le_bytes(error[4..8].try_into().unwrap()),
        0x00f0_0000
    );
}

#[test]
fn x_resource_query_client_ids_omits_pid_for_nonlocal_client() {
    use yserver_protocol::x11::x_resource as x11xres;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    state.clients.get_mut(&1).unwrap().is_local = false;
    let mut backend = RecordingBackend::new();
    let mut body = 1u32.to_le_bytes().to_vec();
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&x11xres::LOCAL_CLIENT_PID_MASK.to_le_bytes());
    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode: 149,
            data: x11xres::QUERY_CLIENT_IDS,
            length_units: 4,
        },
        &body,
        None,
    )
    .unwrap();
    let reply = read_all_available(&mut peer);
    assert_eq!(
        u32::from_le_bytes(reply[8..12].try_into().unwrap()),
        0,
        "nonlocal clients have no LocalClientPID even if the underlying socket supports credentials"
    );
    assert_eq!(reply.len(), 32);
}

#[test]
fn x_resource_query_client_ids_reports_xid_and_pid_identities() {
    use yserver_protocol::x11::x_resource as x11xres;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let _peer2 = install_client(&mut state, 2);
    state.clients.get_mut(&1).unwrap().resource_id_base = 0x0040_0000;
    state.clients.get_mut(&1).unwrap().resource_id_mask = 0x001f_ffff;
    state.clients.get_mut(&2).unwrap().resource_id_base = 0x0080_0000;
    state.clients.get_mut(&2).unwrap().resource_id_mask = 0x001f_ffff;
    let mut backend = RecordingBackend::new();
    let query = |mask: u32| {
        let mut body = Vec::with_capacity(12);
        body.extend_from_slice(&1u32.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes()); // all clients
        body.extend_from_slice(&mask.to_le_bytes());
        body
    };

    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(9),
        RequestHeader {
            opcode: 149,
            data: x11xres::QUERY_CLIENT_IDS,
            length_units: 4,
        },
        &query(x11xres::CLIENT_XID_MASK),
        None,
    )
    .expect("QueryClientIds XIDs");
    let reply = read_all_available(&mut peer);
    assert_eq!(reply.len(), 56);
    assert_eq!(u32::from_le_bytes(reply[8..12].try_into().unwrap()), 2);
    assert_eq!(
        u32::from_le_bytes(reply[32..36].try_into().unwrap()),
        0x0040_0000
    );
    assert_eq!(u32::from_le_bytes(reply[36..40].try_into().unwrap()), 1);
    assert_eq!(u32::from_le_bytes(reply[40..44].try_into().unwrap()), 0);
    assert_eq!(
        u32::from_le_bytes(reply[44..48].try_into().unwrap()),
        0x0080_0000
    );

    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(10),
        RequestHeader {
            opcode: 149,
            data: x11xres::QUERY_CLIENT_IDS,
            length_units: 4,
        },
        &query(x11xres::LOCAL_CLIENT_PID_MASK),
        None,
    )
    .expect("QueryClientIds PID-only");
    // A PID-only spec suppresses the XID identity and yields one
    // LocalClientPID entry per client. Sizes derived from Xorg's encoder
    // (Xext/xres.c `ConstructClientIdValue`): a pid entry is the 12-byte
    // header plus one value word, `rep.length = 4` counted in BYTES, and
    // `num_ids` counts ENTRIES. Two clients => 2*16 = 32 body bytes, so
    // reply length = 32/4 = 8 and the whole reply is 64 bytes.
    let pids = read_all_available(&mut peer);
    assert_eq!(pids.len(), 64);
    assert_eq!(u32::from_le_bytes(pids[4..8].try_into().unwrap()), 8);
    assert_eq!(u32::from_le_bytes(pids[8..12].try_into().unwrap()), 2);
    // Both test clients are socketpairs owned by this process, so
    // SO_PEERCRED reports our own pid for each.
    let self_pid = std::process::id();
    for (i, base) in [0x0040_0000u32, 0x0080_0000].into_iter().enumerate() {
        let at = 32 + i * 16;
        assert_eq!(
            u32::from_le_bytes(pids[at..at + 4].try_into().unwrap()),
            base,
            "entry {i} client",
        );
        assert_eq!(
            u32::from_le_bytes(pids[at + 4..at + 8].try_into().unwrap()),
            x11xres::LOCAL_CLIENT_PID_MASK,
            "entry {i} mask",
        );
        assert_eq!(
            u32::from_le_bytes(pids[at + 8..at + 12].try_into().unwrap()),
            4,
            "entry {i} length is in bytes",
        );
        assert_eq!(
            u32::from_le_bytes(pids[at + 12..at + 16].try_into().unwrap()),
            self_pid,
            "entry {i} pid",
        );
    }
}

#[test]
fn no_operation_returns_handled() {
    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    let outcome = process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(7),
        RequestHeader {
            opcode: 127,
            data: 0,
            length_units: 1,
        },
        &[],
        None,
    )
    .unwrap();
    assert!(matches!(outcome, RequestOutcome::Handled));
}

#[test]
fn get_input_focus_writes_reply_through_client_io() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let outcome = process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(3),
        RequestHeader {
            opcode: 43,
            data: 0,
            length_units: 1,
        },
        &[],
        None,
    )
    .unwrap();
    assert!(matches!(outcome, RequestOutcome::Handled));

    let mut buf = [0u8; 32];
    peer.read_exact(&mut buf).unwrap();
    // Reply opcode is 1 (Reply), sequence in bytes 2..4 little-endian.
    assert_eq!(buf[0], 1);
    assert_eq!(u16::from_le_bytes([buf[2], buf[3]]), 3);
}

/// Regression (codex review #1): a grab/ungrab focus transition
/// whose grab window is ALREADY the focus (from == to) must still
/// emit FocusOut+FocusIn(NotifyNonlinear, NotifyGrab) on that
/// window. Xorg DoFocusEvents only short-circuits same-window
/// moves for NON-grab modes (dix/enterleave.c:1557); pre-fix
/// yserver dropped these whenever the grab window held the focus.
#[test]
fn same_window_grab_focus_emits_nonlinear_pair() {
    use yserver_protocol::x11::CreateWindowRequest;

    const CLIENT: u32 = 71;
    const WIN: u32 = 0x0370_0006;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT);
    state.resources.create_window(
        ClientId(CLIENT),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(WIN),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 200,
            height: 100,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    state
        .clients
        .get_mut(&CLIENT)
        .unwrap()
        .event_masks
        .insert(ResourceId(WIN), FOCUS_CHANGE_MASK);
    state.core_focus.raw = WIN;

    // GrabKeyboard on WIN while WIN is already the focus → mode 1.
    emit_core_focus_transition(&mut state, WIN, WIN, 1);

    let bytes = read_all_available(&mut peer);
    assert!(
        bytes.len() >= 64,
        "expected FocusOut + FocusIn (got {} bytes)",
        bytes.len()
    );
    assert_eq!(bytes[0], 10, "first event FocusOut");
    assert_eq!(bytes[1], 3, "FocusOut detail NotifyNonlinear");
    assert_eq!(bytes[8], 1, "FocusOut mode NotifyGrab");
    assert_eq!(bytes[32], 9, "second event FocusIn");
    assert_eq!(bytes[33], 3, "FocusIn detail NotifyNonlinear");
    assert_eq!(bytes[40], 1, "FocusIn mode NotifyGrab");
}

/// GH #59 regression: a core SetInputFocus issued WHILE a keyboard
/// grab is active must report the focus transition as
/// NotifyWhileGrabbed (mode 3), not NotifyNormal (0) — Xorg
/// dix/events.c:4923. bspwm focuses a newly-mapped window while
/// sxhkd's synchronous passive key grab is still active; emitting
/// NotifyNormal there made GLFW/kitty believe it had genuinely taken
/// focus mid-grab and then ignore every typed key (the KeyPress
/// events still routed correctly to its window). Mirrors the gate
/// already present in `revert_core_focus_from`.
#[test]
fn set_input_focus_during_keyboard_grab_is_notify_while_grabbed() {
    use crate::server::{ActiveKeyboardGrab, ActiveKeyboardGrabSource};
    use yserver_protocol::x11::CreateWindowRequest;

    const CLIENT: u32 = 71;
    const WIN_A: u32 = 0x0071_0001; // currently focused
    const WIN_B: u32 = 0x0071_0002; // focus target

    let make = |state: &mut ServerState, win: u32| {
        state.resources.create_window(
            ClientId(CLIENT),
            CreateWindowRequest {
                depth: 24,
                window: ResourceId(win),
                parent: ROOT_WINDOW,
                x: 0,
                y: 0,
                width: 200,
                height: 100,
                border_width: 0,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
    };

    // mode 3 (NotifyWhileGrabbed) when a keyboard grab is active,
    // mode 0 (NotifyNormal) otherwise — run both in one test.
    for (grab_active, want_mode) in [(true, 3u8), (false, 0u8)] {
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, CLIENT);
        make(&mut state, WIN_A);
        make(&mut state, WIN_B);
        assert!(
            state
                .resources
                .map_window(ResourceId(WIN_B))
                .mapping_changed
        );
        {
            let c = state.clients.get_mut(&CLIENT).unwrap();
            c.event_masks.insert(ResourceId(WIN_A), FOCUS_CHANGE_MASK);
            c.event_masks.insert(ResourceId(WIN_B), FOCUS_CHANGE_MASK);
        }
        state.core_focus.raw = WIN_A;
        if grab_active {
            state.active_keyboard_grab = Some(ActiveKeyboardGrab {
                owner: ClientId(CLIENT),
                grab_window: ROOT_WINDOW,
                owner_events: false,
                source: ActiveKeyboardGrabSource::PassiveKey { keycode: 36 },
                via_xi2: false,
                xi2_mask: 0,
            });
        }

        let header = yserver_protocol::x11::RequestHeader {
            opcode: 42,
            data: 2, // RevertToParent
            length_units: 3,
        };
        let mut body = Vec::new();
        body.extend_from_slice(&WIN_B.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes()); // time = CurrentTime
        handle_set_input_focus(
            &mut state,
            ClientId(CLIENT),
            SequenceNumber(1),
            header,
            &body,
        )
        .expect("set input focus");

        let bytes = read_all_available(&mut peer);
        assert!(
            bytes.len() >= 32,
            "expected focus events (grab_active={grab_active}), got {} bytes",
            bytes.len()
        );
        // First event is the FocusOut on WIN_A; mode is byte 8.
        assert_eq!(bytes[0] & 0x7f, 10, "first event must be FocusOut");
        assert_eq!(
            bytes[8], want_mode,
            "SetInputFocus with grab_active={grab_active} must emit mode {want_mode}"
        );
    }
}

#[test]
fn focus_move_to_child_uses_inferior_detail_for_parent_focus_out() {
    use yserver_protocol::x11::CreateWindowRequest;

    const CLIENT: u32 = 52;
    const TOP: u32 = 0x0350_0006;
    const CHILD: u32 = 0x0350_0007;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT);

    state.resources.create_window(
        ClientId(CLIENT),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(TOP),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 800,
            height: 600,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    state.resources.create_window(
        ClientId(CLIENT),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(CHILD),
            parent: ResourceId(TOP),
            x: 10,
            y: 10,
            width: 100,
            height: 40,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    let client = state.clients.get_mut(&CLIENT).expect("client");
    client
        .event_masks
        .insert(ResourceId(TOP), FOCUS_CHANGE_MASK);
    client
        .event_masks
        .insert(ResourceId(CHILD), FOCUS_CHANGE_MASK);
    client
        .xi2_masks
        .insert((ResourceId(TOP), 3), (1 << 9) | (1 << 10));
    client
        .xi2_masks
        .insert((ResourceId(CHILD), 3), (1 << 9) | (1 << 10));

    state.core_focus = crate::server::CoreFocus {
        raw: TOP,
        revert_to: 0,
        time: 0,
    };
    emit_core_focus_transition(&mut state, TOP, CHILD, 0);
    state.core_focus.raw = CHILD;
    let bytes = read_all_available(&mut peer);

    assert_eq!(bytes.len(), 216, "expected core + XI2 focus out/in");
    // Xorg DoFocusEvents: the core sequence first, then the XI2 one.
    let word = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
    let half = |at: usize| u16::from_le_bytes([bytes[at], bytes[at + 1]]);
    assert_eq!(bytes[0], 10, "first event should be core FocusOut");
    assert_eq!(bytes[1], 2, "parent FocusOut should be NotifyInferior");
    assert_eq!(word(4), TOP);
    assert_eq!(bytes[32], 9, "second event should be core FocusIn");
    assert_eq!(bytes[33], 0, "child FocusIn should be NotifyAncestor");
    assert_eq!(word(36), CHILD);
    assert_eq!(bytes[64], 35, "third event should be XI2 GenericEvent");
    assert_eq!(half(72), 10, "XI2 FocusOut evtype");
    assert_eq!(bytes[83], 2, "XI2 parent FocusOut should be NotifyInferior");
    assert_eq!(word(88), TOP);
    assert_eq!(bytes[140], 35, "fourth event should be XI2 GenericEvent");
    assert_eq!(half(148), 9, "XI2 FocusIn evtype");
    assert_eq!(bytes[159], 0, "XI2 child FocusIn should be NotifyAncestor");
    assert_eq!(word(164), CHILD);
    assert_eq!(
        state.core_focus.raw, CHILD,
        "keyboard focus should track child"
    );
}

#[test]
fn window_host_xid_prefers_resolved_host_mapping() {
    use crate::backend::WindowHandle;
    use yserver_protocol::x11::CreateWindowRequest;

    const CLIENT: u32 = 55;
    const WIN: u32 = 0x0360_0008;
    const HOST: u32 = 0x0400_1234;

    let mut state = ServerState::new();
    state.resources.create_window(
        ClientId(CLIENT),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(WIN),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 64,
            height: 64,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    state
        .resources
        .window_mut(ResourceId(WIN))
        .unwrap()
        .host_xid = WindowHandle::from_raw(HOST);

    assert_eq!(window_host_xid(&state, ResourceId(WIN)), HOST);
}

#[test]
fn grab_server_tracks_owner_until_owner_ungrabs() {
    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(2),
        RequestHeader {
            opcode: 36,
            data: 0,
            length_units: 1,
        },
        &[],
        None,
    )
    .unwrap();
    assert_eq!(state.server_grab_owner, Some(ClientId(1)));

    // A direct non-owner call cannot steal or release the grab. In the
    // real loop these requests are parked before dispatch.
    handle_grab_server(&mut state, ClientId(2), SequenceNumber(3)).unwrap();
    handle_ungrab_server(&mut state, ClientId(2), SequenceNumber(4)).unwrap();
    assert_eq!(state.server_grab_owner, Some(ClientId(1)));

    handle_ungrab_server(&mut state, ClientId(1), SequenceNumber(5)).unwrap();
    assert_eq!(state.server_grab_owner, None);
}

#[test]
fn get_motion_events_filters_and_translates_history() {
    let request = |window: ResourceId, start: u32, stop: u32| {
        let mut body = vec![0u8; 12];
        body[0..4].copy_from_slice(&window.0.to_le_bytes());
        body[4..8].copy_from_slice(&start.to_le_bytes());
        body[8..12].copy_from_slice(&stop.to_le_bytes());
        body
    };

    let mut state = ServerState::new();
    state.start_instant = std::time::Instant::now() - std::time::Duration::from_secs(1);
    state.pointer_motion_history.extend([
        crate::server::PointerMotionRecord {
            time: 500,
            root_x: 100,
            root_y: 50,
        },
        crate::server::PointerMotionRecord {
            time: 550,
            root_x: 900,
            root_y: 50,
        },
    ]);
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(8),
        RequestHeader {
            opcode: 39,
            data: 0,
            length_units: 4,
        },
        &request(ROOT_WINDOW, 400, 600),
        None,
    )
    .unwrap();
    let mut reply = [0u8; 40];
    peer.read_exact(&mut reply).unwrap();
    assert_eq!(reply[0], 1);
    assert_eq!(u32::from_le_bytes(reply[8..12].try_into().unwrap()), 1);
    assert_eq!(u32::from_le_bytes(reply[32..36].try_into().unwrap()), 500);
    assert_eq!(i16::from_le_bytes(reply[36..38].try_into().unwrap()), 100);
    assert_eq!(i16::from_le_bytes(reply[38..40].try_into().unwrap()), 50);

    let missing = ResourceId(0x00ff_1234);
    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(9),
        RequestHeader {
            opcode: 39,
            data: 0,
            length_units: 4,
        },
        &request(missing, 400, 600),
        None,
    )
    .unwrap();
    let mut error = [0u8; 32];
    peer.read_exact(&mut error).unwrap();
    assert_eq!(error[1], x11::error::BAD_WINDOW);
    assert_eq!(
        u32::from_le_bytes(error[4..8].try_into().unwrap()),
        missing.0
    );
    assert_eq!(error[10], 39);
}

#[test]
fn xtest_compare_cursor_uses_window_and_current_cursor_state() {
    const CURSOR_A: ResourceId = ResourceId(0x0010_1000);
    const CURSOR_B: ResourceId = ResourceId(0x0010_1001);
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    state.resources.create_cursor(ClientId(1), CURSOR_A);
    state.resources.create_cursor(ClientId(1), CURSOR_B);
    state.resources.window_mut(ROOT_WINDOW).unwrap().cursor = Some(CURSOR_A);
    let mut backend = RecordingBackend::new();
    let request = |cursor: ResourceId| {
        let mut body = Vec::with_capacity(8);
        body.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
        body.extend_from_slice(&cursor.0.to_le_bytes());
        body
    };
    let header = RequestHeader {
        opcode: 146,
        data: yserver_protocol::x11::xtest::COMPARE_CURSOR,
        length_units: 3,
    };

    for (sequence, cursor, expected) in [
        (1, CURSOR_A, true),
        (2, CURSOR_B, false),
        (3, ResourceId(1), true), // XTestCurrentCursor
    ] {
        handle_xtest_request(
            &mut state,
            &mut backend,
            ClientId(1),
            SequenceNumber(sequence),
            header,
            &request(cursor),
        )
        .unwrap();
        let reply = read_all_available(&mut peer);
        assert_eq!(reply[0], 1);
        assert_eq!(reply[1], u8::from(expected));
    }

    let missing = ResourceId(0x0010_dead);
    handle_xtest_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(4),
        header,
        &request(missing),
    )
    .unwrap();
    let error = read_all_available(&mut peer);
    assert_eq!(error[1], x11::error::BAD_CURSOR);
    assert_eq!(
        u32::from_le_bytes(error[4..8].try_into().unwrap()),
        missing.0
    );
}

#[test]
fn recolor_cursor_rejects_unknown_cursor() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let missing = ResourceId(0x00cc_1234);
    let mut body = vec![0u8; 16];
    body[0..4].copy_from_slice(&missing.0.to_le_bytes());

    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(10),
        RequestHeader {
            opcode: 96,
            data: 0,
            length_units: 5,
        },
        &body,
        None,
    )
    .unwrap();

    let mut error = [0u8; 32];
    peer.read_exact(&mut error).unwrap();
    assert_eq!(error[1], x11::error::BAD_CURSOR);
    assert_eq!(
        u32::from_le_bytes(error[4..8].try_into().unwrap()),
        missing.0
    );
    assert_eq!(error[10], 96);
}

#[test]
fn recolor_cursor_forwards_colors_to_backend() {
    const CURSOR: u32 = 0x0010_1234;
    const HOST_CURSOR: u32 = 0x00ab_cdef;
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    state
        .resources
        .create_cursor(ClientId(1), ResourceId(CURSOR));
    state.resources.set_cursor_host_xid(
        ResourceId(CURSOR),
        crate::backend::CursorHandle::from_raw_panicking(HOST_CURSOR),
    );
    let mut backend = RecordingBackend::new();
    let fore: (u16, u16, u16) = (0x1122, 0x3344, 0x5566);
    let back: (u16, u16, u16) = (0x7788, 0x99aa, 0xbbcc);
    let mut body = vec![0u8; 16];
    body[0..4].copy_from_slice(&CURSOR.to_le_bytes());
    for (offset, value) in [fore.0, fore.1, fore.2, back.0, back.1, back.2]
        .into_iter()
        .enumerate()
    {
        let start = 4 + offset * 2;
        body[start..start + 2].copy_from_slice(&value.to_le_bytes());
    }

    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(11),
        RequestHeader {
            opcode: 96,
            data: 0,
            length_units: 5,
        },
        &body,
        None,
    )
    .unwrap();

    assert_eq!(
        backend.calls.lock().unwrap().as_slice(),
        [RecordedCall::RecolorCursor {
            host_xid: HOST_CURSOR,
            fore,
            back,
        }]
    );
}

#[test]
fn list_hosts_writes_reply() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let outcome = process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(11),
        RequestHeader {
            opcode: 110,
            data: 0,
            length_units: 1,
        },
        &[],
        None,
    )
    .unwrap();
    assert!(matches!(outcome, RequestOutcome::Handled));

    let mut buf = [0u8; 32];
    peer.read_exact(&mut buf).unwrap();
    assert_eq!(buf[0], 1, "Reply opcode");
    assert_eq!(u16::from_le_bytes([buf[2], buf[3]]), 11);
}

#[test]
fn unknown_major_opcodes_return_bad_request() {
    // Xorg's ProcVector routes reserved core opcodes 120-126 and
    // unregistered extension slots to ProcBadRequest. Opcode 127 remains
    // NoOperation and the registered extension slots are matched above.
    for opcode in [120, 126, 255] {
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, 1);
        let mut backend = RecordingBackend::new();
        let outcome = process_request(
            &mut state,
            &mut backend,
            ClientId(1),
            SequenceNumber(0x1234),
            RequestHeader {
                opcode,
                data: 0,
                length_units: 1,
            },
            &[],
            None,
        )
        .unwrap();
        assert!(matches!(outcome, RequestOutcome::Handled));

        let mut error = [0u8; 32];
        peer.read_exact(&mut error).unwrap();
        assert_eq!(error[0], 0, "wire packet must be an X11 error");
        assert_eq!(error[1], x11::error::BAD_REQUEST);
        assert_eq!(u16::from_le_bytes([error[2], error[3]]), 0x1234);
        assert_eq!(
            error[10], opcode,
            "error must identify the unknown major opcode"
        );
        assert_eq!(u16::from_le_bytes([error[8], error[9]]), 0);
    }
}

#[test]
fn no_operation_remains_a_successful_noop() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let outcome = process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(7),
        RequestHeader {
            opcode: 127,
            data: 0,
            length_units: 1,
        },
        &[],
        None,
    )
    .unwrap();
    assert!(matches!(outcome, RequestOutcome::Handled));

    peer.set_nonblocking(true).unwrap();
    let mut byte = [0u8; 1];
    assert_eq!(
        peer.read(&mut byte).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
}

#[test]
fn unknown_extension_minors_return_bad_request() {
    // Each locally-dispatched extension below has an Xorg dispatcher that
    // returns core BadRequest when the minor is outside its request table.
    // GLX is intentionally absent: its dispatcher uses GLXBadRequest and
    // has separate coverage for that extension-specific error.
    let extensions = [
        (128, "RANDR"),
        (130, "MIT-SHM"),
        (133, "RENDER"),
        (134, "DPMS"),
        (135, "BIG-REQUESTS"),
        (136, "XKEYBOARD"),
        (137, "XInputExtension"),
        (138, "Generic Event Extension"),
        (140, "XFIXES"),
        (141, "SHAPE"),
        (142, "SYNC"),
        (143, "DAMAGE"),
        (144, "Composite"),
        (145, "Present"),
        (146, "XTEST"),
        (147, "DRI3"),
        (149, "X-Resource"),
        (150, "MIT-SCREEN-SAVER"),
        (151, "XINERAMA"),
        (152, "XC-MISC"),
        (153, "XFree86-VidModeExtension"),
        (154, "RECORD"),
    ];

    for (opcode, name) in extensions {
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, 1);
        let mut backend = RecordingBackend::new();
        let outcome = process_request(
            &mut state,
            &mut backend,
            ClientId(1),
            SequenceNumber(0x4321),
            RequestHeader {
                opcode,
                data: u8::MAX,
                length_units: 1,
            },
            &[],
            None,
        )
        .unwrap();
        assert!(matches!(outcome, RequestOutcome::Handled), "{name}");

        let mut error = [0u8; 32];
        peer.read_exact(&mut error)
            .unwrap_or_else(|e| panic!("{name}: missing error: {e}"));
        assert_eq!(error[0], 0, "{name}: packet must be an X11 error");
        assert_eq!(error[1], x11::error::BAD_REQUEST, "{name}");
        assert_eq!(
            u16::from_le_bytes([error[2], error[3]]),
            0x4321,
            "{name}: sequence"
        );
        assert_eq!(
            u16::from_le_bytes([error[8], error[9]]),
            u16::from(u8::MAX),
            "{name}: minor opcode"
        );
        assert_eq!(error[10], opcode, "{name}: major opcode");
    }
}

/// A `SendEvent` with an empty `event_mask` is addressed to the
/// client that *created* the destination window (X11 SendEvent
/// semantics). That is the XEmbed input-forwarding contract: a
/// systray manager (cinnamon) forwards the synthetic core
/// ButtonPress of a user click on the tray icon to the embedded
/// client, which also holds an XI2 selection on the window; dropping
/// it left pamac's tray icon unclickable (HW air 2026-06-22).
#[test]
fn send_event_empty_mask_delivers_synthetic_button_to_owner_despite_xi2_selection() {
    use std::io::Read;

    const SENDER: u32 = 1; // cinnamon
    const OWNER: u32 = 2; // pamac
    const WIN: u32 = 0x0020_0001; // pamac's embedded icon window

    let mut state = ServerState::new();
    let _sender_peer = install_client(&mut state, SENDER);
    let mut owner_peer = install_client(&mut state, OWNER);

    // Window created by (owned by) the pamac client.
    state.resources.create_window(
        yserver_protocol::x11::ClientId(OWNER),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(WIN),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 24,
            height: 24,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    // pamac (GTK3) selects XI2 ButtonPress (bit 4) on its own window —
    // exactly what makes the guard fire.
    state
        .clients
        .get_mut(&OWNER)
        .unwrap()
        .xi2_masks
        .insert((ResourceId(WIN), 1), 1 << 4);

    // SendEvent: destination=WIN, event_mask=0, template = core
    // ButtonPress (type 4).
    let mut body = Vec::with_capacity(40);
    body.extend_from_slice(&WIN.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // empty event-mask
    let mut tmpl = [0u8; 32];
    tmpl[0] = 4; // ButtonPress
    body.extend_from_slice(&tmpl);
    let header = yserver_protocol::x11::RequestHeader {
        opcode: 25,
        data: 0, // propagate = false
        length_units: 11,
    };
    handle_send_event(
        &mut state,
        ClientId(SENDER),
        SequenceNumber(1),
        header,
        &body,
    )
    .expect("SendEvent");

    owner_peer.set_nonblocking(true).unwrap();
    let mut wire = Vec::new();
    let mut tmp = [0u8; 256];
    loop {
        match owner_peer.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => wire.extend_from_slice(&tmp[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }
    // The owner must receive the synthetic ButtonPress (type 4 with
    // the sent-event bit 0x80 set → 0x84).
    assert!(
        !wire.is_empty() && wire[0] == 0x84,
        "owner must receive the XEmbed-forwarded ButtonPress; got {} bytes: {:?}",
        wire.len(),
        &wire[..wire.len().min(40)],
    );
}

/// Windows for the `SendEvent` tests: W (owned by `owner`, child of the
/// root) and its child C, neither mapped, so the pointer sprite is the
/// root.
fn send_event_tree(state: &mut ServerState, owner: u32, w: u32, c: u32) {
    for (id, parent) in [(w, ROOT_WINDOW.0), (c, w)] {
        state.resources.create_window(
            yserver_protocol::x11::ClientId(owner),
            yserver_protocol::x11::CreateWindowRequest {
                depth: 24,
                window: ResourceId(id),
                parent: ResourceId(parent),
                width: 100,
                height: 100,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
    }
}

fn send_event_body(destination: u32, mask: u32, event_type: u8) -> Vec<u8> {
    let mut body = Vec::with_capacity(40);
    body.extend_from_slice(&destination.to_le_bytes());
    body.extend_from_slice(&mask.to_le_bytes());
    let mut tmpl = [0u8; 32];
    tmpl[0] = event_type;
    body.extend_from_slice(&tmpl);
    body
}

fn run_send_event(state: &mut ServerState, sender: u32, propagate: u8, body: &[u8]) {
    let header = yserver_protocol::x11::RequestHeader {
        opcode: 25,
        data: propagate,
        length_units: 11,
    };
    handle_send_event(state, ClientId(sender), SequenceNumber(1), header, body).expect("SendEvent");
}

fn drain_wire(peer: &mut UnixStream) -> Vec<u8> {
    use std::io::Read;
    peer.set_nonblocking(true).unwrap();
    let mut wire = Vec::new();
    let mut tmp = [0u8; 256];
    while let Ok(n) = peer.read(&mut tmp) {
        if n == 0 {
            break;
        }
        wire.extend_from_slice(&tmp[..n]);
    }
    wire
}

/// The first byte of each 32-byte packet on the wire.
fn wire_types(wire: &[u8]) -> Vec<u8> {
    wire.chunks(32).map(|p| p[0]).collect()
}

/// #212: a synthetic core Motion/Press/Release reaches the client whose
/// CORE mask matches even when it also selected the XI2 form of the
/// event on the window (master or all-devices) and on the root. Xorg
/// `GetClientsForDelivery` (`dix/events.c:2241`) takes the core
/// `OtherClients` for a core type; XI2 masks never enter. Measured:
/// goldens/sendevent-xi2.txt, the "XI2 AllMasterDevices" steps.
#[test]
fn send_event_core_mask_delivers_despite_xi2_selection() {
    const SENDER: u32 = 1;
    const OWNER: u32 = 2;
    const W: u32 = 0x0020_0001;
    const C: u32 = 0x0020_0002;
    let mut state = ServerState::new();
    let mut sender_peer = install_client(&mut state, SENDER);
    let mut owner_peer = install_client(&mut state, OWNER);
    send_event_tree(&mut state, OWNER, W, C);
    let owner = state.clients.get_mut(&OWNER).unwrap();
    owner.event_masks.insert(ResourceId(W), 0x4C); // ButtonPress|Release|PointerMotion
    owner.xi2_masks.insert((ResourceId(W), 1), 0x70); // XI_ButtonPress|Release|Motion
    owner.xi2_masks.insert((ROOT_WINDOW, 0), 0x70);
    for (event_type, mask) in [(6u8, 0x40u32), (4, 0x0C), (5, 0x0C)] {
        run_send_event(&mut state, SENDER, 0, &send_event_body(W, mask, event_type));
    }
    assert_eq!(
        wire_types(&drain_wire(&mut owner_peer)),
        vec![0x86, 0x84, 0x85]
    );
    assert!(drain_wire(&mut sender_peer).is_empty());
}

/// Propagation (Xorg `ProcSendEvent`, `dix/events.c:5622-5636`): no
/// selector on C, so propagate=True carries the event to W; False stops
/// at C; C's do-not-propagate mask strips its bits, and an empty
/// remainder ends the walk. Measured: goldens/sendevent-xi2.txt.
#[test]
fn send_event_propagates_by_core_masks_and_do_not_propagate() {
    const SENDER: u32 = 1;
    const OWNER: u32 = 2;
    const W: u32 = 0x0020_0001;
    const C: u32 = 0x0020_0002;
    let mut state = ServerState::new();
    let _sender_peer = install_client(&mut state, SENDER);
    let mut owner_peer = install_client(&mut state, OWNER);
    send_event_tree(&mut state, OWNER, W, C);
    let owner = state.clients.get_mut(&OWNER).unwrap();
    owner.event_masks.insert(ResourceId(W), 0x0C); // ButtonPress|ButtonRelease
    owner.xi2_masks.insert((ResourceId(W), 1), 0x70);

    run_send_event(&mut state, SENDER, 0, &send_event_body(C, 0x04, 4));
    assert!(
        drain_wire(&mut owner_peer).is_empty(),
        "propagate False stops at C"
    );

    run_send_event(&mut state, SENDER, 1, &send_event_body(C, 0x04, 4));
    assert_eq!(wire_types(&drain_wire(&mut owner_peer)), vec![0x84]);

    state
        .resources
        .window_mut(ResourceId(C))
        .unwrap()
        .do_not_propagate_mask = 0x04;
    run_send_event(&mut state, SENDER, 1, &send_event_body(C, 0x04, 4));
    assert!(
        drain_wire(&mut owner_peer).is_empty(),
        "DNP strips the only bit"
    );
    // ButtonPress|ButtonRelease minus DNP ButtonPress still matches W.
    run_send_event(&mut state, SENDER, 1, &send_event_body(C, 0x0C, 4));
    assert_eq!(wire_types(&drain_wire(&mut owner_peer)), vec![0x84]);
}

/// InputFocus (`dix/events.c:5594-5611`): with the pointer outside the
/// focus window the walk starts AND ends at the focus window, so W's
/// selection is never reached from focus C; focus W gets it. Focus None
/// delivers nothing and raises no error. Measured:
/// goldens/sendevent-xi2.txt, the "focus" steps.
#[test]
fn send_event_input_focus_stops_at_the_focus_window() {
    const SENDER: u32 = 1;
    const OWNER: u32 = 2;
    const W: u32 = 0x0020_0001;
    const C: u32 = 0x0020_0002;
    let mut state = ServerState::new();
    let mut sender_peer = install_client(&mut state, SENDER);
    let mut owner_peer = install_client(&mut state, OWNER);
    send_event_tree(&mut state, OWNER, W, C);
    state
        .clients
        .get_mut(&OWNER)
        .unwrap()
        .event_masks
        .insert(ResourceId(W), 0x01); // KeyPress

    state.core_focus.raw = C;
    run_send_event(&mut state, SENDER, 1, &send_event_body(1, 0x01, 2));
    assert!(
        drain_wire(&mut owner_peer).is_empty(),
        "walk ends at focus C"
    );

    state.core_focus.raw = W;
    run_send_event(&mut state, SENDER, 1, &send_event_body(1, 0x01, 2));
    assert_eq!(wire_types(&drain_wire(&mut owner_peer)), vec![0x82]);

    state.core_focus.raw = 0;
    run_send_event(&mut state, SENDER, 1, &send_event_body(1, 0x01, 2));
    assert!(drain_wire(&mut owner_peer).is_empty());
    assert!(
        drain_wire(&mut sender_peer).is_empty(),
        "focus None is not an error"
    );
}

/// `ProcSendEvent` errors (`dix/events.c:5563-5619`), each measured on
/// Xorg (goldens/sendevent-xi2.txt): an unknown destination is
/// BadWindow; event types outside 2..=34 and 64.. (GenericEvent too),
/// a ClientMessage format other than 8/16/32, and mask bits above
/// `AllEventMasks` are BadValue carrying the offending value.
#[test]
fn send_event_rejects_like_xorg() {
    const SENDER: u32 = 1;
    const OWNER: u32 = 2;
    const W: u32 = 0x0020_0001;
    const C: u32 = 0x0020_0002;
    let mut state = ServerState::new();
    let mut sender_peer = install_client(&mut state, SENDER);
    let mut owner_peer = install_client(&mut state, OWNER);
    send_event_tree(&mut state, OWNER, W, C);
    let mut client_message = send_event_body(W, 0, 33);
    client_message[9] = 7;
    let cases: [(Vec<u8>, u8, u32); 6] = [
        (send_event_body(0x7fff_fff0, 0, 4), 3, 0x7fff_fff0),
        (send_event_body(W, 0x0200_0000, 4), 2, 0x0200_0000),
        (send_event_body(W, 0, 1), 2, 1),
        (send_event_body(W, 0, 35), 2, 35),
        (send_event_body(W, 0, 40), 2, 40),
        (client_message, 2, 7),
    ];
    for (body, code, value) in cases {
        run_send_event(&mut state, SENDER, 0, &body);
        let wire = drain_wire(&mut sender_peer);
        assert_eq!(wire.len(), 32, "one error for {body:?}");
        assert_eq!((wire[0], wire[1]), (0, code));
        assert_eq!(
            u32::from_le_bytes([wire[4], wire[5], wire[6], wire[7]]),
            value
        );
    }
    assert!(drain_wire(&mut owner_peer).is_empty());
}

fn assert_window_removal_releases_pointer_grab(opcode: u8) {
    use crate::server::ActivePointerGrab;

    const OWNER: u32 = 1;
    let grab_window = ResourceId(0x0010_0001);

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    let _peer = install_client(&mut state, OWNER);

    state.resources.create_window(
        ClientId(OWNER),
        CreateWindowRequest {
            depth: 24,
            window: grab_window,
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 100,
            height: 100,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(grab_window);
    state.active_pointer_grab = Some(ActivePointerGrab {
        owner: ClientId(OWNER),
        grab_window,
        event_mask: 0xFFFF,
        cursor: ResourceId(0),
        time: 0,
        owner_events: true,
        via_xi2: true,
        implicit: false,
        passive: false,
        xi2_mask: u64::MAX,
    });

    process_request(
        &mut state,
        &mut backend,
        ClientId(OWNER),
        SequenceNumber(1),
        RequestHeader {
            opcode,
            data: 0,
            length_units: 2,
        },
        &grab_window.0.to_le_bytes(),
        None,
    )
    .expect("window removal request");

    assert!(
        state.active_pointer_grab.is_none(),
        "opcode {opcode} must deactivate a grab held on the removed window",
    );
}

/// Destroying a grab window deactivates the grab, matching Xorg's
/// DeleteWindowFromAnyEvents teardown.
#[test]
fn destroying_a_grab_window_releases_the_pointer_grab() {
    assert_window_removal_releases_pointer_grab(4);
}

/// An unmapped grab window is no longer viewable and cannot retain an
/// active grab.
#[test]
fn unmapping_a_grab_window_releases_the_pointer_grab() {
    assert_window_removal_releases_pointer_grab(10);
}

/// Per X11 spec (Xorg `dix/enterleave.c:606` →
/// `CoreEnterLeaveEvents` → `CoreEnterNotifies` →
/// `DeliverEventsToWindow`): grab-activation crossing events
/// flow through the normal event-delivery path, which filters
/// by the client's per-window event-mask selection. xts's
/// `GrabPointer` test opens a fresh connection, creates a
/// grab window WITHOUT selecting `EnterWindowMask` (0x10), and
/// expects the next thing on the wire after sending GrabPointer
/// to be the reply (opcode 1). Pre-fix yserver's
/// `emit_core_grab_activation_crossings` fanned `EnterNotify`
/// (event type 7) unconditionally to the grabber via
/// `fanout_event_to_clients(&[client_id], …)` regardless of
/// mask, so the event landed on the wire BEFORE the reply
/// and the test reported `wanted REPLY - X_GrabPointer , got
/// EVENT - EnterNotify` (xts Xproto run 2026-05-31-11:13:07:
/// 1 FAIL on `GrabPointer` + 1 FAIL on `GrabKeyboard` + 8
/// UNRESOLVED on `AllowEvents` / `ChangeActivePointerGrab`).
/// Marco / GTK3 popups still receive the crossings because
/// they select `EnterWindowMask` on their popup windows.
#[test]
fn grab_pointer_skips_synthesised_crossing_when_mask_unselected() {
    const CLIENT_ID: u32 = 1;
    const GRAB_WIN: u32 = 0x100_0007;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT_ID);
    let mut backend = RecordingBackend::new();

    // Create the grab window WITHOUT EnterWindowMask selected
    // (mirrors xts's fresh CreateWindow).
    state.resources.create_window(
        yserver_protocol::x11::ClientId(CLIENT_ID),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(GRAB_WIN),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 100,
            height: 100,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(ResourceId(GRAB_WIN));

    // GrabPointer body: window(4) event-mask(2) pointer-mode(1)
    // keyboard-mode(1) confine-to(4) cursor(4) time(4) = 20.
    // event-mask=0, modes async, confine_to=None, cursor=None,
    // time=CurrentTime.
    let mut body = Vec::with_capacity(20);
    body.extend_from_slice(&GRAB_WIN.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes()); // event-mask
    body.push(0); // pointer-mode async
    body.push(0); // keyboard-mode async
    body.extend_from_slice(&0u32.to_le_bytes()); // confine-to None
    body.extend_from_slice(&0u32.to_le_bytes()); // cursor None
    body.extend_from_slice(&0u32.to_le_bytes()); // time CurrentTime

    handle_grab_pointer(
        &mut state,
        &mut backend,
        ClientId(CLIENT_ID),
        SequenceNumber(11),
        RequestHeader {
            opcode: 26,
            data: 0, // owner_events false
            length_units: 6,
        },
        &body,
    )
    .expect("handle_grab_pointer");

    peer.set_nonblocking(true).unwrap();
    let mut tmp = [0u8; 1024];
    let mut wire = Vec::new();
    loop {
        match peer.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => wire.extend_from_slice(&tmp[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }
    // First 32-byte chunk must be the reply (opcode 1), NOT an
    // EnterNotify (event type 7).
    assert!(
        wire.len() >= 32,
        "GrabPointer must produce at least the reply on the wire",
    );
    assert_ne!(
        wire[0] & 0x7f,
        7,
        "Pre-fix: EnterNotify (type 7) landed on the wire before \
             the GrabPointer reply because crossings were unconditionally \
             fanned to the grabber regardless of EnterWindowMask. xts \
             reports this as `wanted REPLY - X_GrabPointer , got EVENT - \
             EnterNotify` and marks dependent tests UNRESOLVED.",
    );
    assert_eq!(
        wire[0], 1,
        "GrabPointer must put the reply (opcode 1) first on the wire \
             when the grabber has not selected EnterWindowMask",
    );
    // No EnterNotify anywhere — the grabber didn't select it.
    let enter_notifies = wire.chunks_exact(32).filter(|c| c[0] & 0x7f == 7).count();
    assert_eq!(
        enter_notifies, 0,
        "EnterNotify must not be delivered to a grabber that didn't \
             select EnterWindowMask on the grab window",
    );
}

/// Positive companion to the test above: when the grabber HAS
/// selected `EnterWindowMask` on the grab window (the
/// marco / GTK3 popup-menu pattern), the synthesised crossing
/// must still fire. The grab fix is "respect the event-mask
/// filter", not "drop crossings entirely".
#[test]
fn grab_pointer_emits_crossing_when_grabber_selected_enter_mask() {
    const CLIENT_ID: u32 = 1;
    const GRAB_WIN: u32 = 0x100_0008;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT_ID);
    let mut backend = RecordingBackend::new();

    state.resources.create_window(
        yserver_protocol::x11::ClientId(CLIENT_ID),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(GRAB_WIN),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 100,
            height: 100,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(ResourceId(GRAB_WIN));
    // marco/GTK3 popup pattern: the pointer is over the panel (the
    // root here), NOT over the popup being grabbed — grab
    // activation must emit Enter(NotifyGrab) on the popup for
    // GTK3's hover machinery. (With the pointer already inside the
    // grab window Xorg emits nothing: DoEnterLeaveEvents from==to.)
    state.pointer_root = (500, 500);
    state
        .clients
        .get_mut(&CLIENT_ID)
        .unwrap()
        .event_masks
        .insert(ResourceId(GRAB_WIN), 0x10);

    let mut body = Vec::with_capacity(20);
    body.extend_from_slice(&GRAB_WIN.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes());
    body.push(0);
    body.push(0);
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());

    handle_grab_pointer(
        &mut state,
        &mut backend,
        ClientId(CLIENT_ID),
        SequenceNumber(13),
        RequestHeader {
            opcode: 26,
            data: 0,
            length_units: 6,
        },
        &body,
    )
    .expect("handle_grab_pointer");

    peer.set_nonblocking(true).unwrap();
    let mut tmp = [0u8; 1024];
    let mut wire = Vec::new();
    loop {
        match peer.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => wire.extend_from_slice(&tmp[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }
    let enter_notifies = wire.chunks_exact(32).filter(|c| c[0] & 0x7f == 7).count();
    assert!(
        enter_notifies >= 1,
        "EnterNotify must still fire to a grabber that selected \
             EnterWindowMask (marco/GTK3 popup-menu pattern)",
    );
}

/// Symmetric to the activation test: xts case 116
/// (`pUngrabPointer-1.(A)`) opens a connection with NO event
/// mask selected, grabs+ungrabs, and verifies the server
/// sends NOTHING. Pre-fix `emit_core_grab_deactivation_crossing`
/// fanned `LeaveNotify` unconditionally to the ungrabber; the
/// test reported `wanted NOTHING, got EVENT - LeaveNotify`.
/// Mirror fix: the deactivation crossing must respect
/// `LeaveWindowMask` (0x20) on the prior grab window.
#[test]
fn ungrab_pointer_skips_synthesised_leave_when_mask_unselected() {
    const CLIENT_ID: u32 = 1;
    const GRAB_WIN: u32 = 0x100_0009;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT_ID);
    let mut backend = RecordingBackend::new();

    state.resources.create_window(
        yserver_protocol::x11::ClientId(CLIENT_ID),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(GRAB_WIN),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 100,
            height: 100,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(ResourceId(GRAB_WIN));

    // Seed an active grab so handle_ungrab_pointer has
    // something to deactivate (mirrors xts's GrabPointer →
    // UngrabPointer sequence).
    state.active_pointer_grab = Some(crate::server::ActivePointerGrab {
        owner: ClientId(CLIENT_ID),
        grab_window: ResourceId(GRAB_WIN),
        event_mask: 0,
        cursor: ResourceId(0),
        time: 0,
        owner_events: false,
        via_xi2: false,
        implicit: false,
        passive: false,
        xi2_mask: 0,
    });

    handle_ungrab_pointer(
        &mut state,
        &mut backend,
        ClientId(CLIENT_ID),
        SequenceNumber(17),
        &[0u8; 4],
    )
    .expect("handle_ungrab_pointer");

    peer.set_nonblocking(true).unwrap();
    let mut tmp = [0u8; 1024];
    let mut wire = Vec::new();
    loop {
        match peer.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => wire.extend_from_slice(&tmp[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }
    let leave_notifies = wire.chunks_exact(32).filter(|c| c[0] & 0x7f == 8).count();
    assert_eq!(
        leave_notifies, 0,
        "UngrabPointer from a client that did not select \
             LeaveWindowMask must not produce a LeaveNotify on the \
             wire (xts case 116 / `pUngrabPointer-1.(A)`)",
    );
    assert!(
        wire.is_empty(),
        "UngrabPointer with no event mask selected must produce \
             NOTHING on the wire; got {} bytes: {:?}",
        wire.len(),
        &wire[..wire.len().min(64)],
    );
}

// NOTE: the 3rd argument is the SEQUENCE number, not a client id —
// every request in these tests comes from ClientId(1).
fn send_xcmisc(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    seq: u16,
    minor: u8,
    body: &[u8],
) {
    process_request(
        state,
        backend,
        ClientId(1),
        SequenceNumber(seq),
        RequestHeader {
            opcode: 152,
            data: minor,
            length_units: u32::try_from(1 + body.len().div_ceil(4)).unwrap(),
        },
        body,
        None,
    )
    .expect("process_request");
}

#[test]
fn xcmisc_get_version_replies_1_1() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    // GetVersion body: client major(2), minor(2) — values ignored.
    let body = [0u8, 0, 0, 0];
    send_xcmisc(&mut state, &mut backend, 1, 0, &body);
    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes.len(), 32, "fixed 32-byte reply, got {bytes:02x?}");
    assert_eq!(bytes[0], 1, "X_Reply");
    assert_eq!(&bytes[2..4], &1u16.to_le_bytes(), "sequence");
    assert_eq!(&bytes[8..10], &1u16.to_le_bytes(), "major=1");
    assert_eq!(&bytes[10..12], &1u16.to_le_bytes(), "minor=1");
}

#[test]
fn xcmisc_bad_length_and_unknown_minor() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    // GetVersion with wrong body size → BadLength.
    send_xcmisc(&mut state, &mut backend, 1, 0, &[0u8; 8]);
    let bytes = read_all_available(&mut peer);
    assert!(bytes.len() >= 32);
    assert_eq!(bytes[1], x11::error::BAD_LENGTH);
    assert_eq!(bytes[10], 152, "major opcode");
    assert_eq!(&bytes[8..10], &0u16.to_le_bytes(), "minor 0 echoed");
    // Unknown minor 3 → BadRequest (Xorg dispatcher default).
    send_xcmisc(&mut state, &mut backend, 2, 3, &[]);
    let bytes = read_all_available(&mut peer);
    assert!(bytes.len() >= 32);
    assert_eq!(bytes[1], x11::error::BAD_REQUEST);
    assert_eq!(&bytes[8..10], &3u16.to_le_bytes(), "minor 3 echoed");
}

#[test]
fn xcmisc_advertised_in_query_and_list_extensions() {
    // QueryExtension "XC-MISC" → present with major 152; covered via
    // extension_query_reply + advertised_extension_names directly
    // (cheaper than wire round-trips; they are what the handlers use).
    let mut backend = RecordingBackend::new();
    let reply = extension_query_reply("XC-MISC", &mut backend);
    assert_eq!(reply, Some((152, 0, 0)));
    assert!(advertised_extension_names(&mut backend).contains(&"XC-MISC"));
}

#[test]
fn xcmisc_get_xid_range_skips_used_ids() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    // Give the test client a realistic base/mask (install_client
    // defaults to base=0, mask=u32::MAX).
    {
        let c = state.clients.get_mut(&1).unwrap();
        c.resource_id_base = 0x0010_0000;
        c.resource_id_mask = 0x000F_FFFF;
    }
    // Occupy the low end of the range.
    for i in 0..4u32 {
        state
            .resources
            .create_cursor(ClientId(1), ResourceId(0x0010_0000 + i));
    }
    send_xcmisc(&mut state, &mut backend, 1, 1, &[]);
    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes.len(), 32);
    assert_eq!(bytes[0], 1);
    let start = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    let count = u32::from_le_bytes(bytes[12..16].try_into().unwrap());
    assert_eq!(start, 0x0010_0004, "range starts after the used prefix");
    assert_eq!(count, 0x000F_FFFF - 4 + 1);
    // Wrong length → BadLength.
    send_xcmisc(&mut state, &mut backend, 2, 1, &[0u8; 4]);
    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes[1], x11::error::BAD_LENGTH);
}

#[test]
fn xcmisc_get_xid_list_returns_free_ids() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    {
        let c = state.clients.get_mut(&1).unwrap();
        c.resource_id_base = 0x0010_0000;
        c.resource_id_mask = 0x000F_FFFF;
    }
    // Occupy ids 0,2 in the range; ask for 4 ids.
    state
        .resources
        .create_cursor(ClientId(1), ResourceId(0x0010_0000));
    state
        .resources
        .create_cursor(ClientId(1), ResourceId(0x0010_0002));
    send_xcmisc(&mut state, &mut backend, 1, 2, &4u32.to_le_bytes());
    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes.len(), 32 + 16, "32 header + 4 ids");
    let count = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    assert_eq!(count, 4);
    let length = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    assert_eq!(length, 4, "reply length in words = id count");
    let ids: Vec<u32> = bytes[32..]
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
        .collect();
    assert_eq!(
        ids,
        vec![0x0010_0001, 0x0010_0003, 0x0010_0004, 0x0010_0005],
        "skips occupied 0x...0 and 0x...2"
    );
    // Huge count is clamped to the RANGE size, not allocated. Shrink
    // the client's range first so the reply stays ~1 KiB — a 1M-id
    // reply would block the test's unread socketpair (write_or_buffer
    // does a synchronous write; OUTBOUND_CAP would trip the
    // disconnect path).
    state.clients.get_mut(&1).unwrap().resource_id_mask = 0xFF;
    send_xcmisc(&mut state, &mut backend, 2, 2, &u32::MAX.to_le_bytes());
    let bytes = read_all_available(&mut peer);
    let count = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    assert_eq!(count, 0x100 - 2, "256-id range minus 2 occupied");
    // BadLength on wrong size.
    send_xcmisc(&mut state, &mut backend, 3, 2, &[]);
    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes[1], x11::error::BAD_LENGTH);
}
