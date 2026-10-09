use super::*;

#[test]
fn xkb_get_kbd_by_name_also_broadcasts_legacy_mapping_notify() {
    // Real `setxkbmap` loads a layout via XkbGetKeyboardByName (minor
    // 23), not by writing `_XKB_RULES_NAMES`. A plain-X11 client (any
    // WM that never calls XkbSelectEvents and only understands core
    // events — e.g. one resolving keybinds via XKeysymToKeycode once
    // at startup) relies on the legacy MappingNotify event to know
    // when to re-resolve its keysym-based key grabs. Without it, the
    // WM's shortcuts stay bound to the pre-switch keycodes forever
    // even though per-keystroke translation (and thus typing) already
    // reflects the new layout.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    // Stale ChangeKeyboardMapping row from before the switch — must
    // not keep shadowing the freshly loaded keymap either.
    state.keymap_overrides.insert(38, vec![0x0071]);

    let mut reply = vec![0u8; 32];
    reply[0] = 1; // X_Reply
    assert!(crate::core_loop::xkb_select::use_extension(
        &mut state,
        ClientId(1),
        &[1, 0, 0, 0]
    ));
    let mut backend = RecordingBackend::new().with_kbd_by_name_result(
        reply,
        Some(crate::backend::XkbNewKeyboardInfo {
            min_keycode: 8,
            max_keycode: 255,
            old_min_keycode: 8,
            old_max_keycode: 255,
            changed: 0x0001,
        }),
    );

    handle_xkb_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode: 136,
            data: 23,
            length_units: 2,
        },
        &[],
    )
    .expect("XkbGetKbdByName");

    assert!(
        state.keymap_overrides.is_empty(),
        "a full XkbGetKeyboardByName load must drop stale ChangeKeyboardMapping rows too"
    );

    let bytes = read_all_available(&mut peer);
    let mapping_notify_count = bytes
        .chunks(32)
        .filter(
            |chunk| chunk.len() == 32 && chunk[0] == 34, /* MappingNotify */
        )
        .count();
    assert!(
        mapping_notify_count >= 2,
        "expected legacy MappingNotify(Keyboard) + MappingNotify(Modifier) \
             alongside XkbNewKeyboardNotify, got {mapping_notify_count} in {bytes:02x?}"
    );
    let keyboard_mapping_notify = bytes
        .chunks(32)
        .find(|chunk| chunk.len() == 32 && chunk[0] == 34 && chunk[4] == 1);
    assert!(
        keyboard_mapping_notify.is_some(),
        "expected a MappingNotify with request=Keyboard(1): {bytes:02x?}"
    );
}

fn xkb_request(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    client: u32,
    minor: u8,
    body: &[u8],
) {
    handle_xkb_request(
        state,
        backend,
        None,
        ClientId(client),
        SequenceNumber(1),
        RequestHeader {
            opcode: 136,
            data: minor,
            length_units: u32::try_from(1 + body.len().div_ceil(4)).unwrap(),
        },
        body,
    )
    .expect("XKB request");
}

/// Xvfb 21.1.24, XkbUseExtension(1, 0) at sequence 2 from a
/// little-endian client: `01010200 00000000 01000000 00…`
/// (supported, server 1.0).
#[test]
fn xkb_use_extension_replies_like_xorg() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    handle_xkb_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(2),
        RequestHeader {
            opcode: 136,
            data: 0,
            length_units: 2,
        },
        &[1, 0, 0, 0],
    )
    .expect("XkbUseExtension");
    let reply = read_all_available(&mut peer);
    let mut expected = [0u8; 32];
    expected[..10].copy_from_slice(&[1, 1, 2, 0, 0, 0, 0, 0, 1, 0]);
    assert_eq!(reply, expected);
    assert!(crate::core_loop::xkb_select::xkb_initialized(
        &state,
        ClientId(1)
    ));
}

/// yserver's XKB request parsers and reply encoders are little-endian
/// only, so a big-endian client is refused the way Xorg refuses a
/// client it can't serve: XkbUseExtension answers supported=False
/// (the bytes are Xvfb's big-endian answer to an unsupported version,
/// `01000002 00000000 00010000 00…`), and every other XKB request is
/// then BadAccess, as Xorg answers an uninitialised client (Xvfb:
/// GetState → BadAccess, value 0, minor 4). All of it in the client's
/// byte order.
#[test]
fn xkb_refuses_big_endian_clients_cleanly() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    state.clients.get_mut(&1).unwrap().byte_order = ClientByteOrder::BigEndian;
    // XKB request bodies are not swapped: wantedMajor=1, wantedMinor=0
    // as a big-endian client sends them.
    handle_xkb_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(2),
        RequestHeader {
            opcode: 136,
            data: 0,
            length_units: 2,
        },
        &[0, 1, 0, 0],
    )
    .expect("XkbUseExtension");
    let reply = read_all_available(&mut peer);
    let mut expected = [0u8; 32];
    expected[..10].copy_from_slice(&[1, 0, 0, 2, 0, 0, 0, 0, 0, 1]);
    assert_eq!(reply, expected);
    assert!(!crate::core_loop::xkb_select::xkb_initialized(
        &state,
        ClientId(1)
    ));
    handle_xkb_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(3),
        RequestHeader {
            opcode: 136,
            data: 4,
            length_units: 2,
        },
        &[1, 0, 0, 0],
    )
    .expect("XkbGetState");
    let err = read_all_available(&mut peer);
    assert_eq!(err.len(), 32);
    assert_eq!((err[0], err[1]), (0, x11::error::BAD_ACCESS));
    assert_eq!(&err[2..4], &[0, 3], "big-endian sequence");
    assert_eq!(&err[8..10], &[0, 4], "big-endian minor opcode");
}

/// Golden (`xorg-xkb-setmap-errors.txt`, Xvfb 21.1.24: `access`,
/// `access-getmap`, `access-selectevents`): an XKB request from a client
/// that never called XkbUseExtension answers BadAccess (value 0, minor =
/// the request's), and changes nothing; after UseExtension the same
/// requests go through.
#[test]
fn xkb_requests_need_use_extension() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let select_all = [0x00, 0x01, 0xff, 0x0f, 0, 0, 0xff, 0x0f, 0xff, 0, 0xff, 0];
    for (minor, body) in [
        (8u8, vec![0u8; 24]),
        (1, select_all.to_vec()),
        (9, vec![0; 32]),
    ] {
        xkb_request(&mut state, &mut backend, 1, minor, &body);
        let err = read_all_available(&mut peer);
        assert_eq!(err.len(), 32, "minor {minor}: one error");
        assert_eq!(
            (err[0], err[1], &err[4..8], err[8], err[10]),
            (0, x11::error::BAD_ACCESS, &[0u8, 0, 0, 0][..], minor, 136),
            "minor {minor}"
        );
    }
    assert!(
        state
            .xkb_clients
            .get(&1)
            .is_none_or(|c| c.map_notify_mask == 0)
    );
    xkb_request(&mut state, &mut backend, 1, 0, &[1, 0, 0, 0]);
    assert!(crate::core_loop::xkb_select::xkb_initialized(
        &state,
        ClientId(1)
    ));
    let _ = read_all_available(&mut peer);
    xkb_request(&mut state, &mut backend, 1, 1, &select_all);
    assert!(
        read_all_available(&mut peer).is_empty(),
        "SelectEvents: no error"
    );
    assert_eq!(state.xkb_clients[&1].map_notify_mask, 0xff);
}

/// `ProcXkbUseExtension`: an unsupported version (2.0) leaves the client
/// uninitialised.
#[test]
fn xkb_use_extension_unsupported_version_stays_uninitialised() {
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    xkb_request(&mut state, &mut backend, 1, 0, &[2, 0, 0, 0]);
    assert!(!crate::core_loop::xkb_select::xkb_initialized(
        &state,
        ClientId(1)
    ));
}

/// `XkbSendLegacyMapNotify` (xkb/xkbEvents.c): a MapNotify's core
/// MappingNotify goes to non-XKB clients and to XKB clients whose map
/// details include a change; a NewKeyboardNotify's only to non-XKB
/// clients.
#[test]
fn legacy_map_notify_filters_xkb_clients() {
    use crate::core_loop::xkb_select::{
        LegacyCause, send_legacy_core_map_notify, xkb_select_events,
    };
    let mut state = ServerState::new();
    let mut plain = install_client(&mut state, 1);
    let mut xkb_keysyms = install_client(&mut state, 2);
    let mut xkb_none = install_client(&mut state, 3);
    xkb_select_events(&mut state, 2, 0x100, 0x0002);
    xkb_select_events(&mut state, 3, 0x100, 0x0004);
    let _ = send_legacy_core_map_notify(&mut state, LegacyCause::MapNotify, 0x0002, 8, 248);
    let count = |b: Vec<u8>| b.chunks(32).filter(|e| e[0] == 34).count();
    assert_eq!(count(read_all_available(&mut plain)), 1);
    assert_eq!(count(read_all_available(&mut xkb_keysyms)), 1);
    assert_eq!(count(read_all_available(&mut xkb_none)), 0);
    let _ = send_legacy_core_map_notify(&mut state, LegacyCause::NewKeyboardNotify, 0x0001, 8, 248);
    assert_eq!(
        count(read_all_available(&mut plain)),
        2,
        "Keyboard + Modifier"
    );
    assert_eq!(count(read_all_available(&mut xkb_keysyms)), 0);
    assert_eq!(count(read_all_available(&mut xkb_none)), 0);
}
