use super::*;

#[test]
fn xi_get_focus_returns_real_focus_window() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    // Master keyboard (device 3) focused on a real window. The master
    // keyboard's focus IS the core focus (Xorg: `inputInfo.keyboard->
    // focus`, which ProcSetInputFocus and ProcXIGetFocus share).
    let win = 0x4000_0005u32;
    state.core_focus = crate::server::CoreFocus {
        raw: win,
        revert_to: 0,
        time: 0,
    };
    // XIGetFocus { deviceid:CARD16=3 } + pad.
    let header = RequestHeader {
        opcode: 137,
        data: 50,
        length_units: 2,
    };
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        header,
        &[3u8, 0, 0, 0],
    )
    .expect("process");
    let bytes = read_all_available(&mut peer);
    assert_eq!(
        bytes.len(),
        32,
        "XIGetFocus reply is 32 bytes: {bytes:02x?}"
    );
    assert_eq!(bytes[0], 1, "X_Reply");
    assert_eq!(&bytes[8..12], &win.to_le_bytes(), "focus window @ byte 8");
}

#[test]
fn xi_get_focus_invalid_device_returns_bad_device() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    // deviceid 0 = XIAllDevices: not a specific device → BadDevice.
    let header = RequestHeader {
        opcode: 137,
        data: 50,
        length_units: 2,
    };
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        header,
        &[0u8, 0, 0, 0],
    )
    .expect("process");
    let bytes = read_all_available(&mut peer);
    assert!(bytes.len() >= 32, "expected error reply: {bytes:02x?}");
    assert_eq!(bytes[0], 0, "error packet");
    assert_eq!(bytes[1], XI1_ERROR_BAD_DEVICE, "BadDevice");
    assert_eq!(&bytes[8..10], &50u16.to_le_bytes(), "minor echoed");
}

// ── XIWarpPointer / XISetFocus / XIChangeHierarchy / XISelectEvents ──
//
// Expected values below are Xvfb 21.1.24 captures (raw-socket probe,
// little-endian client unless noted), cross-checked against
// Xi/xiwarppointer.c, Xi/xisetdevfocus.c, Xi/xichangehierarchy.c and
// Xi/xiselectev.c.

/// Send one XInput request (major 137) from client 1 and return what
/// the client received.
fn send_xi_request(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    peer: &mut UnixStream,
    minor: u8,
    body: &[u8],
) -> Vec<u8> {
    let header = RequestHeader {
        opcode: 137,
        data: minor,
        length_units: u32::try_from(1 + body.len().div_ceil(4)).unwrap(),
    };
    handle_xi2_request(
        state,
        backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        header,
        body,
    )
    .expect("xi request");
    read_all_available(peer)
}

fn assert_xi_error(bytes: &[u8], code: u8, value: u32, minor: u16) {
    assert_eq!(bytes.len(), 32, "one error packet: {bytes:02x?}");
    assert_eq!(bytes[0], 0, "error packet: {bytes:02x?}");
    assert_eq!(bytes[1], code, "error code: {bytes:02x?}");
    assert_eq!(
        u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
        value,
        "errorValue: {bytes:02x?}"
    );
    assert_eq!(
        u16::from_le_bytes(bytes[8..10].try_into().unwrap()),
        minor,
        "minor opcode: {bytes:02x?}"
    );
    assert_eq!(bytes[10], XI2_MAJOR_OPCODE, "major opcode");
}

/// A mapped 100×100 child of the root at (100, 100), owned by client 1.
fn seed_mapped_window_at_100(state: &mut ServerState, xid: u32) {
    state.resources.create_window(
        ClientId(1),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(xid),
            parent: ROOT_WINDOW,
            x: 100,
            y: 100,
            width: 100,
            height: 100,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(ResourceId(xid));
}

fn fp1616(v: f64) -> i32 {
    #[allow(clippy::cast_possible_truncation)]
    let raw = (v * 65536.0).round() as i32;
    raw
}

#[allow(clippy::too_many_arguments)]
fn xi_warp_body(
    src: u32,
    dst: u32,
    src_x: f64,
    src_y: f64,
    src_w: u16,
    src_h: u16,
    dst_x: f64,
    dst_y: f64,
    deviceid: u16,
) -> Vec<u8> {
    let mut b = Vec::with_capacity(32);
    b.extend_from_slice(&src.to_le_bytes());
    b.extend_from_slice(&dst.to_le_bytes());
    b.extend_from_slice(&fp1616(src_x).to_le_bytes());
    b.extend_from_slice(&fp1616(src_y).to_le_bytes());
    b.extend_from_slice(&src_w.to_le_bytes());
    b.extend_from_slice(&src_h.to_le_bytes());
    b.extend_from_slice(&fp1616(dst_x).to_le_bytes());
    b.extend_from_slice(&fp1616(dst_y).to_le_bytes());
    b.extend_from_slice(&deviceid.to_le_bytes());
    b.extend_from_slice(&[0, 0]);
    b
}

#[allow(clippy::too_many_arguments)]
fn send_core_warp(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    src: u32,
    dst: u32,
    src_x: i16,
    src_y: i16,
    src_w: u16,
    src_h: u16,
    dst_x: i16,
    dst_y: i16,
) {
    let mut b = Vec::with_capacity(20);
    b.extend_from_slice(&src.to_le_bytes());
    b.extend_from_slice(&dst.to_le_bytes());
    b.extend_from_slice(&src_x.to_le_bytes());
    b.extend_from_slice(&src_y.to_le_bytes());
    b.extend_from_slice(&src_w.to_le_bytes());
    b.extend_from_slice(&src_h.to_le_bytes());
    b.extend_from_slice(&dst_x.to_le_bytes());
    b.extend_from_slice(&dst_y.to_le_bytes());
    process_request(
        state,
        backend,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode: 41,
            data: 0,
            length_units: 6,
        },
        &b,
        None,
    )
    .expect("WarpPointer");
}

#[test]
fn xi_warp_pointer_moves_to_truncated_fp1616_destination() {
    // Xvfb: XIWarpPointer(dst=root, 10.75, 20.25) → pointer (10, 20);
    // then relative (-0.5, -1.5) → (10, 19); relative (+0.99, +2.5)
    // → (10, 21). ProcXIWarpPointer stores the FP16.16 values in
    // ints, so the fraction truncates toward zero.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let body = xi_warp_body(0, ROOT_WINDOW.0, 0.0, 0.0, 0, 0, 10.75, 20.25, 2);
    let bytes = send_xi_request(&mut state, &mut backend, &mut peer, 41, &body);
    assert!(bytes.is_empty(), "XIWarpPointer has no reply: {bytes:02x?}");
    assert_eq!(backend.warped_to, Some((10, 20)));

    state.pointer_root = (10, 20);
    let body = xi_warp_body(0, 0, 0.0, 0.0, 0, 0, -0.5, -1.5, 2);
    let _ = send_xi_request(&mut state, &mut backend, &mut peer, 41, &body);
    assert_eq!(backend.warped_to, Some((10, 19)));

    state.pointer_root = (10, 19);
    let body = xi_warp_body(0, 0, 0.0, 0.0, 0, 0, 0.99, 2.5, 2);
    let _ = send_xi_request(&mut state, &mut backend, &mut peer, 41, &body);
    assert_eq!(backend.warped_to, Some((10, 21)));
}

#[test]
fn xi_warp_pointer_to_window_and_clamped_to_the_screen() {
    // Xvfb (1024×768): dst=W(100,100) (5, 6) → (105, 106);
    // (-5.5, -6.5) → (95, 94); dst=root (5000, 5000) → (w-1, h-1);
    // (-3, -3) → (0, 0). The test root is 800×600.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    const W: u32 = 0x0040_0001;
    seed_mapped_window_at_100(&mut state, W);
    for (dx, dy, want) in [(5.0, 6.0, (105, 106)), (-5.5, -6.5, (95, 94))] {
        let body = xi_warp_body(0, W, 0.0, 0.0, 0, 0, dx, dy, 2);
        let _ = send_xi_request(&mut state, &mut backend, &mut peer, 41, &body);
        assert_eq!(backend.warped_to, Some(want), "dst=W ({dx}, {dy})");
    }
    for (dx, dy, want) in [
        (5000.0, 5000.0, (799, 599)),
        (800.0, 600.0, (799, 599)),
        (-3.0, -3.0, (0, 0)),
    ] {
        let body = xi_warp_body(0, ROOT_WINDOW.0, 0.0, 0.0, 0, 0, dx, dy, 2);
        let _ = send_xi_request(&mut state, &mut backend, &mut peer, 41, &body);
        assert_eq!(backend.warped_to, Some(want), "dst=root ({dx}, {dy})");
    }
}

#[test]
fn xi_warp_pointer_rejects_everything_but_the_master_pointer() {
    // ProcXIWarpPointer: BadDevice unless the device is a master
    // pointer or a floating slave (yserver has no floating slaves).
    // errorValue = deviceid. Xvfb: devices 0,1,3,4,5,99 → BadDevice.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    for dev in [0u16, 1, 3, 4, 5, 99] {
        let body = xi_warp_body(0, ROOT_WINDOW.0, 0.0, 0.0, 0, 0, 5.0, 5.0, dev);
        let bytes = send_xi_request(&mut state, &mut backend, &mut peer, 41, &body);
        assert_xi_error(&bytes, XI1_ERROR_BAD_DEVICE, u32::from(dev), 41);
    }
    // The device is checked before the windows.
    let body = xi_warp_body(0, 0x00de_ad00, 0.0, 0.0, 0, 0, 5.0, 5.0, 3);
    let bytes = send_xi_request(&mut state, &mut backend, &mut peer, 41, &body);
    assert_xi_error(&bytes, XI1_ERROR_BAD_DEVICE, 3, 41);
    assert_eq!(backend.warped_to, None, "no rejected request may warp");
}

#[test]
fn xi_warp_pointer_bad_windows_report_dst_before_src() {
    // Xvfb: dst bad → BadWindow(dst); src bad → BadWindow(src); both
    // bad → BadWindow(dst).
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let body = xi_warp_body(0, 0x00de_ad00, 0.0, 0.0, 0, 0, 5.0, 5.0, 2);
    let bytes = send_xi_request(&mut state, &mut backend, &mut peer, 41, &body);
    assert_xi_error(&bytes, x11::error::BAD_WINDOW, 0x00de_ad00, 41);
    let body = xi_warp_body(0x00be_ef00, ROOT_WINDOW.0, 0.0, 0.0, 0, 0, 5.0, 5.0, 2);
    let bytes = send_xi_request(&mut state, &mut backend, &mut peer, 41, &body);
    assert_xi_error(&bytes, x11::error::BAD_WINDOW, 0x00be_ef00, 41);
    let body = xi_warp_body(0x00be_ef00, 0x00de_ad00, 0.0, 0.0, 0, 0, 5.0, 5.0, 2);
    let bytes = send_xi_request(&mut state, &mut backend, &mut peer, 41, &body);
    assert_xi_error(&bytes, x11::error::BAD_WINDOW, 0x00de_ad00, 41);
    assert_eq!(backend.warped_to, None);
}

#[test]
fn xi_warp_pointer_source_rectangle_follows_xorgs_xi_arithmetic() {
    // Xorg keeps two copies of the source-rectangle test. The XI one
    // compares the right edge against 0 instead of the pointer x
    // (`winX + src_x + src_width < 0`), so with W at (100,100) 100×100
    // and src rect (0,0,10,10):
    //   pointer (160,105): XIWarpPointer WARPS, core WarpPointer doesn't;
    //   pointer (105,160): neither warps (the bottom edge is checked);
    //   pointer (105,110): XI warps (bottom edge inclusive).
    // Both also require the pointer to be visible in the source window:
    //   pointer (50,50), src rect (-100,-100,0,0): no warp.
    // The src coordinates truncate: pointer (104,104) with src_x 4.9
    // warps, with 5.0 doesn't; pointer (99,99) with -0.9 doesn't.
    const W: u32 = 0x0040_0001;
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    seed_mapped_window_at_100(&mut state, W);
    let cases = [
        ((160_i16, 105_i16), (0.0, 0.0, 10_u16, 10_u16), true),
        ((105, 160), (0.0, 0.0, 10, 10), false),
        ((105, 110), (0.0, 0.0, 10, 10), true),
        ((50, 50), (-100.0, -100.0, 0, 0), false),
        ((104, 104), (4.9, 4.9, 0, 0), true),
        ((104, 104), (5.0, 5.0, 0, 0), false),
        ((99, 99), (-0.9, -0.9, 0, 0), false),
    ];
    for (pointer, (sx, sy, sw, sh), warps) in cases {
        let mut backend = RecordingBackend::new();
        state.pointer_root = pointer;
        let body = xi_warp_body(W, ROOT_WINDOW.0, sx, sy, sw, sh, 300.0, 300.0, 2);
        let bytes = send_xi_request(&mut state, &mut backend, &mut peer, 41, &body);
        assert!(bytes.is_empty(), "no error: {bytes:02x?}");
        assert_eq!(
            backend.warped_to,
            warps.then_some((300, 300)),
            "pointer {pointer:?} src ({sx},{sy},{sw},{sh})"
        );
    }
    // src = root: always inside.
    let mut backend = RecordingBackend::new();
    state.pointer_root = (50, 50);
    let body = xi_warp_body(ROOT_WINDOW.0, ROOT_WINDOW.0, 0.0, 0.0, 0, 0, 70.0, 70.0, 2);
    let _ = send_xi_request(&mut state, &mut backend, &mut peer, 41, &body);
    assert_eq!(backend.warped_to, Some((70, 70)));
}

#[test]
fn core_warp_pointer_source_rectangle_matches_xorg() {
    // ProcWarpPointer (Xvfb): W at (100,100) 100×100, src rect
    // (0,0,10,10): pointer (160,105) → no warp; (110,105) → warp (the
    // right edge is inclusive: `winX + srcX + srcWidth < x`);
    // (50,50) with src rect (-100,-100,0,0) → no warp (the pointer is
    // not visible in W). Bad windows report dst before src.
    const W: u32 = 0x0040_0001;
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    seed_mapped_window_at_100(&mut state, W);
    for (pointer, (sx, sy, sw, sh), warps) in [
        ((160, 105), (0, 0, 10, 10), false),
        ((110, 105), (0, 0, 10, 10), true),
        ((105, 110), (0, 0, 10, 10), true),
        ((50, 50), (-100, -100, 0, 0), false),
    ] {
        let mut backend = RecordingBackend::new();
        state.pointer_root = pointer;
        send_core_warp(
            &mut state,
            &mut backend,
            W,
            ROOT_WINDOW.0,
            sx,
            sy,
            sw,
            sh,
            300,
            300,
        );
        assert_eq!(
            backend.warped_to,
            warps.then_some((300, 300)),
            "pointer {pointer:?} src ({sx},{sy},{sw},{sh})"
        );
    }
    let mut backend = RecordingBackend::new();
    send_core_warp(
        &mut state,
        &mut backend,
        0x00be_ef00,
        0x00de_ad00,
        0,
        0,
        0,
        0,
        5,
        5,
    );
    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes[1], x11::error::BAD_WINDOW);
    assert_eq!(
        u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
        0x00de_ad00,
        "dst is looked up first"
    );
}

fn xi_set_focus_body(focus: u32, time: u32, deviceid: u16) -> Vec<u8> {
    let mut b = Vec::with_capacity(12);
    b.extend_from_slice(&focus.to_le_bytes());
    b.extend_from_slice(&time.to_le_bytes());
    b.extend_from_slice(&deviceid.to_le_bytes());
    b.extend_from_slice(&[0, 0]);
    b
}

/// Split a byte stream into X packets (GenericEvents carry a tail).
fn split_packets(bytes: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos + 32 <= bytes.len() {
        let mut len = 32;
        if bytes[pos] & 0x7f == 35 || bytes[pos] == 1 {
            len += 4 * u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        }
        out.push(&bytes[pos..pos + len]);
        pos += len;
    }
    out
}

#[test]
fn xi_set_focus_on_the_master_keyboard_is_core_set_input_focus() {
    // Xvfb: XISetFocus(dev 3, W) → GetInputFocus = W with revert_to
    // RevertToParent (ProcXISetFocus passes RevertToParent), XIGetFocus
    // (3) = W, and the core + XI2 FocusIn/FocusOut chain is delivered.
    const W: u32 = 0x0040_0001;
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    seed_mapped_window_at_100(&mut state, W);
    {
        let client = state.clients.get_mut(&1).unwrap();
        client.event_masks.insert(ResourceId(W), FOCUS_CHANGE_MASK);
        client
            .xi2_masks
            .insert((ResourceId(W), 0), (1 << 9) | (1 << 10));
    }
    let bytes = send_xi_request(
        &mut state,
        &mut backend,
        &mut peer,
        49,
        &xi_set_focus_body(W, 0, 3),
    );
    assert_eq!(state.core_focus.raw, W);
    assert_eq!(state.core_focus.revert_to, 2, "RevertToParent");
    let packets = split_packets(&bytes);
    assert!(
        packets
            .iter()
            .any(|p| p[0] == 9 && u32::from_le_bytes(p[4..8].try_into().unwrap()) == W),
        "core FocusIn on W: {packets:02x?}"
    );
    assert!(
        packets.iter().any(|p| p[0] == 35
            && u16::from_le_bytes(p[8..10].try_into().unwrap()) == 9
            && u32::from_le_bytes(p[24..28].try_into().unwrap()) == W),
        "XI2 FocusIn on W: {packets:02x?}"
    );
    let bytes = send_xi_request(&mut state, &mut backend, &mut peer, 50, &[3, 0, 0, 0]);
    assert_eq!(&bytes[8..12], &W.to_le_bytes(), "XIGetFocus(3) = W");

    // Xvfb: None and PointerRoot are accepted and reported back.
    for focus in [0u32, 1] {
        let _ = send_xi_request(
            &mut state,
            &mut backend,
            &mut peer,
            49,
            &xi_set_focus_body(focus, 0, 3),
        );
        assert_eq!(state.core_focus.raw, focus);
        let bytes = send_xi_request(&mut state, &mut backend, &mut peer, 50, &[3, 0, 0, 0]);
        let bytes = split_packets(&bytes).last().unwrap().to_vec();
        assert_eq!(&bytes[8..12], &focus.to_le_bytes());
    }

    // A time later than the server's clock is silently ignored.
    let _ = send_xi_request(
        &mut state,
        &mut backend,
        &mut peer,
        49,
        &xi_set_focus_body(W, 0x7fff_ffff, 3),
    );
    assert_eq!(state.core_focus.raw, 1, "future-timestamp request ignored");
}

#[test]
fn xi_set_focus_errors_match_xorg() {
    // Xvfb: devices without a focus class (the pointers) and unknown
    // ids → BadDevice, errorValue 0 (ProcXISetFocus sets none); a
    // missing window → BadWindow; an unviewable one → BadMatch with
    // the window as errorValue.
    const UNMAPPED: u32 = 0x0040_0002;
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    state.resources.create_window(
        ClientId(1),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(UNMAPPED),
            parent: ROOT_WINDOW,
            width: 10,
            height: 10,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    for dev in [0u16, 1, 2, 4, 99] {
        let bytes = send_xi_request(
            &mut state,
            &mut backend,
            &mut peer,
            49,
            &xi_set_focus_body(ROOT_WINDOW.0, 0, dev),
        );
        assert_xi_error(&bytes, XI1_ERROR_BAD_DEVICE, 0, 49);
    }
    let bytes = send_xi_request(
        &mut state,
        &mut backend,
        &mut peer,
        49,
        &xi_set_focus_body(0x00de_ad00, 0, 3),
    );
    assert_xi_error(&bytes, x11::error::BAD_WINDOW, 0x00de_ad00, 49);
    let bytes = send_xi_request(
        &mut state,
        &mut backend,
        &mut peer,
        49,
        &xi_set_focus_body(UNMAPPED, 0, 3),
    );
    assert_xi_error(&bytes, x11::error::BAD_MATCH, UNMAPPED, 49);
    // FollowKeyboard on the master keyboard itself would make it
    // follow itself (Xvfb segfaults on it); yserver rejects it.
    let bytes = send_xi_request(
        &mut state,
        &mut backend,
        &mut peer,
        49,
        &xi_set_focus_body(3, 0, 3),
    );
    assert_xi_error(&bytes, x11::error::BAD_VALUE, 3, 49);
    assert_eq!(state.core_focus.raw, 1, "focus untouched by errors");
}

#[test]
fn xi_set_focus_on_the_slave_keyboard_sets_its_own_focus() {
    // Xvfb: XISetFocus(dev 5, W) leaves the core focus alone and
    // XIGetFocus(5) = W; FollowKeyboard (3) is accepted on a slave
    // and reported back as 3.
    const W: u32 = 0x0040_0001;
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    seed_mapped_window_at_100(&mut state, W);
    let bytes = send_xi_request(
        &mut state,
        &mut backend,
        &mut peer,
        49,
        &xi_set_focus_body(W, 0, 5),
    );
    assert!(bytes.is_empty(), "{bytes:02x?}");
    assert_eq!(state.core_focus.raw, 1, "core focus unchanged");
    let f = crate::core_loop::xi1_focus::device_focus(&state, 5);
    assert_eq!((f.focus, f.revert_to), (W, 2));
    let bytes = send_xi_request(&mut state, &mut backend, &mut peer, 50, &[5, 0, 0, 0]);
    assert_eq!(&bytes[8..12], &W.to_le_bytes());
    let _ = send_xi_request(
        &mut state,
        &mut backend,
        &mut peer,
        49,
        &xi_set_focus_body(3, 0, 5),
    );
    let bytes = send_xi_request(&mut state, &mut backend, &mut peer, 50, &[5, 0, 0, 0]);
    assert_eq!(&bytes[8..12], &3u32.to_le_bytes(), "FollowKeyboard");
}

#[test]
fn xi_get_focus_rejects_pointer_devices() {
    // Xvfb: XIGetFocus on devices without a focus class (2, 4) →
    // BadDevice, errorValue 0.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    for dev in [2u8, 4] {
        let bytes = send_xi_request(&mut state, &mut backend, &mut peer, 50, &[dev, 0, 0, 0]);
        assert_xi_error(&bytes, XI1_ERROR_BAD_DEVICE, 0, 50);
    }
}

fn xi_hierarchy_body(num_changes: u8, changes: &[&[u8]]) -> Vec<u8> {
    let mut b = vec![num_changes, 0, 0, 0];
    for c in changes {
        b.extend_from_slice(c);
    }
    b
}

fn le_u16s(values: &[u16]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

fn remove_master_change(dev: u16, mode: u8) -> Vec<u8> {
    let mut c = le_u16s(&[2, 3, dev]);
    c.extend_from_slice(&[mode, 0]);
    c.extend_from_slice(&le_u16s(&[2, 3]));
    c
}

#[test]
fn xi_change_hierarchy_answers_like_xorg_for_the_fixed_devices() {
    // Every row is an Xvfb 21.1.24 capture. Xvfb's devices 4/5 are the
    // XTest slaves, which Xorg refuses to attach/detach ("these are
    // fixed"); yserver's 4/5 are fixed too, so the answers match
    // device-for-device. `None` = Success (no reply, no event).
    const BAD_DEVICE: u8 = XI1_ERROR_BAD_DEVICE;
    let attach = |dev: u16, master: u16| le_u16s(&[3, 2, dev, master]);
    let detach = |dev: u16| le_u16s(&[4, 2, dev, 0]);
    // (label, request body, expected error (code, errorValue)).
    type Case = (&'static str, Vec<u8>, Option<(u8, u32)>);
    let cases: Vec<Case> = vec![
        ("empty", xi_hierarchy_body(0, &[]), None),
        (
            "remove 2 float",
            xi_hierarchy_body(1, &[&remove_master_change(2, 2)]),
            Some((BAD_DEVICE, 0)),
        ),
        (
            "remove 3 attach",
            xi_hierarchy_body(1, &[&remove_master_change(3, 1)]),
            Some((BAD_DEVICE, 0)),
        ),
        (
            "remove 2 mode 5",
            xi_hierarchy_body(1, &[&remove_master_change(2, 5)]),
            Some((x11::error::BAD_VALUE, 0)),
        ),
        (
            "remove 4",
            xi_hierarchy_body(1, &[&remove_master_change(4, 2)]),
            Some((BAD_DEVICE, 4)),
        ),
        (
            "remove 99",
            xi_hierarchy_body(1, &[&remove_master_change(99, 2)]),
            Some((BAD_DEVICE, 0)),
        ),
        (
            "remove 0",
            xi_hierarchy_body(1, &[&remove_master_change(0, 2)]),
            Some((BAD_DEVICE, 0)),
        ),
        (
            "attach 4->2",
            xi_hierarchy_body(1, &[&attach(4, 2)]),
            Some((BAD_DEVICE, 4)),
        ),
        (
            "attach 4->3",
            xi_hierarchy_body(1, &[&attach(4, 3)]),
            Some((BAD_DEVICE, 4)),
        ),
        (
            "attach 5->3",
            xi_hierarchy_body(1, &[&attach(5, 3)]),
            Some((BAD_DEVICE, 5)),
        ),
        (
            "attach 2->3",
            xi_hierarchy_body(1, &[&attach(2, 3)]),
            Some((BAD_DEVICE, 2)),
        ),
        (
            "attach 99->2",
            xi_hierarchy_body(1, &[&attach(99, 2)]),
            Some((BAD_DEVICE, 0)),
        ),
        (
            "detach 4",
            xi_hierarchy_body(1, &[&detach(4)]),
            Some((BAD_DEVICE, 4)),
        ),
        (
            "detach 5",
            xi_hierarchy_body(1, &[&detach(5)]),
            Some((BAD_DEVICE, 5)),
        ),
        (
            "detach 2",
            xi_hierarchy_body(1, &[&detach(2)]),
            Some((BAD_DEVICE, 2)),
        ),
        (
            "detach 99",
            xi_hierarchy_body(1, &[&detach(99)]),
            Some((BAD_DEVICE, 0)),
        ),
        (
            "unknown type skipped",
            xi_hierarchy_body(1, &[&le_u16s(&[9, 1])]),
            None,
        ),
        (
            "unknown then remove 2",
            xi_hierarchy_body(2, &[&le_u16s(&[9, 1]), &remove_master_change(2, 2)]),
            Some((BAD_DEVICE, 0)),
        ),
        (
            "remove with length 2",
            xi_hierarchy_body(1, &[&le_u16s(&[2, 2, 2, 2, 0, 3])]),
            Some((x11::error::BAD_LENGTH, 0)),
        ),
        (
            "attach with length 3",
            xi_hierarchy_body(1, &[&le_u16s(&[3, 3, 4, 2, 0, 0])]),
            Some((x11::error::BAD_LENGTH, 0)),
        ),
        (
            "num_changes 2, one change",
            xi_hierarchy_body(2, &[&remove_master_change(2, 2)]),
            Some((BAD_DEVICE, 0)),
        ),
        (
            "change length past the request",
            xi_hierarchy_body(1, &[&le_u16s(&[3, 9, 4, 2])]),
            Some((x11::error::BAD_LENGTH, 0)),
        ),
        (
            "add master, name longer than the request",
            xi_hierarchy_body(1, &[&[1, 0, 2, 0, 4, 0, 1, 1]]),
            Some((x11::error::BAD_LENGTH, 0)),
        ),
    ];
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    for (label, body, want) in cases {
        let bytes = send_xi_request(&mut state, &mut backend, &mut peer, 43, &body);
        match want {
            None => assert!(bytes.is_empty(), "{label}: {bytes:02x?}"),
            Some((code, value)) => {
                assert!(!bytes.is_empty(), "{label}: expected an error");
                assert_eq!(bytes[1], code, "{label}: {bytes:02x?}");
                assert_xi_error(&bytes, code, value, 43);
            }
        }
    }
}

#[test]
fn xi_change_hierarchy_add_master_is_refused_with_bad_alloc() {
    // Xorg creates the pair; yserver's device set is fixed, so it
    // answers the way Xorg does when it cannot allocate a device
    // (AllocDevicePair → BadAlloc) and sends no HierarchyChanged.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    state
        .clients
        .get_mut(&1)
        .unwrap()
        .xi2_masks
        .insert((ROOT_WINDOW, 0), 1 << 11);
    let mut add = le_u16s(&[1, 3, 3]);
    add.extend_from_slice(&[1, 1]);
    add.extend_from_slice(b"foo\0");
    let bytes = send_xi_request(
        &mut state,
        &mut backend,
        &mut peer,
        43,
        &xi_hierarchy_body(1, &[&add]),
    );
    assert_xi_error(&bytes, x11::error::BAD_ALLOC, 0, 43);
}

#[test]
fn xi_change_hierarchy_reads_changes_in_client_byte_order() {
    // The change list is opaque to the request swapper; a big-endian
    // client's AttachSlave(4 → 2) must still name device 4.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    state.clients.get_mut(&1).unwrap().byte_order = ClientByteOrder::BigEndian;
    let change: Vec<u8> = [3u16, 2, 4, 2]
        .iter()
        .flat_map(|v| v.to_be_bytes())
        .collect();
    let bytes = send_xi_request(
        &mut state,
        &mut backend,
        &mut peer,
        43,
        &xi_hierarchy_body(1, &[&change]),
    );
    assert_eq!(bytes[1], XI1_ERROR_BAD_DEVICE, "{bytes:02x?}");
    assert_eq!(&bytes[4..8], &4u32.to_be_bytes(), "errorValue = device 4");
}

#[test]
fn xi_unknown_minor_is_bad_request() {
    // Xvfb: XI minors 0, 62, 200, 255 → BadRequest (ProcIDispatch).
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    for minor in [0u8, 62, 200, 255] {
        let bytes = send_xi_request(&mut state, &mut backend, &mut peer, minor, &[0; 8]);
        assert_xi_error(&bytes, x11::error::BAD_REQUEST, 0, u16::from(minor));
    }
}

fn xi_select_body(window: u32, masks: &[(u16, &[u32])]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&window.to_le_bytes());
    b.extend_from_slice(&u16::try_from(masks.len()).unwrap().to_le_bytes());
    b.extend_from_slice(&[0, 0]);
    for (dev, words) in masks {
        b.extend_from_slice(&dev.to_le_bytes());
        b.extend_from_slice(&u16::try_from(words.len()).unwrap().to_le_bytes());
        for w in *words {
            // Event masks are bit arrays: byte i holds bits 8i..8i+7.
            b.extend_from_slice(&w.to_le_bytes());
        }
    }
    b
}

/// Send a client-order XI request through the request-body swap used by
/// `client_reader`, then dispatch it through the production
/// `process_request` entry point.
fn send_xi_wire_request(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    peer: &mut impl TestPeer,
    client_id: u32,
    sequence: u16,
    minor: u8,
    byte_order: ClientByteOrder,
    mut body: Vec<u8>,
) -> Vec<u8> {
    yserver_protocol::x11::request_swap::swap_request_body(137, minor, byte_order, &mut body);
    process_request(
        state,
        backend,
        ClientId(client_id),
        SequenceNumber(sequence),
        RequestHeader {
            opcode: 137,
            data: minor,
            length_units: u32::try_from(1 + body.len() / 4).unwrap(),
        },
        &body,
        None,
    )
    .expect("XI request dispatch");
    let mut reply = read_all_available(peer);
    reply.extend(
        state
            .clients
            .get_mut(&client_id)
            .expect("test client")
            .outbound
            .drain(..),
    );
    reply
}

fn xi_dynamic_select_wire_body(
    byte_order: ClientByteOrder,
    window: u32,
    masks: &[(u16, [u8; 4])],
) -> Vec<u8> {
    let mut body = WireBody(byte_order, Vec::new())
        .u32(window)
        .u16(u16::try_from(masks.len()).unwrap())
        .bytes(&[0, 0]);
    for (device_id, mask) in masks {
        body = body.u16(*device_id).u16(1).bytes(mask);
    }
    body.1
}

fn xi_dynamic_passive_grab_wire_body(
    byte_order: ClientByteOrder,
    window: u32,
    detail: u32,
    device_id: u16,
    event_mask: &[u8],
    modifiers: &[u32],
) -> Vec<u8> {
    assert!(event_mask.len().is_multiple_of(4));
    let mut body = WireBody(byte_order, Vec::new())
        .u32(0)
        .u32(window)
        .u32(0)
        .u32(detail)
        .u16(device_id)
        .u16(u16::try_from(modifiers.len()).unwrap())
        .u16(u16::try_from(event_mask.len() / 4).unwrap())
        .bytes(&[1, 1, 1, 0, 0, 0])
        .bytes(event_mask);
    for modifier in modifiers {
        body = body.u32(*modifier);
    }
    body.1
}

fn xi_dynamic_passive_ungrab_wire_body(
    byte_order: ClientByteOrder,
    window: u32,
    detail: u32,
    device_id: u16,
    modifiers: &[u32],
) -> Vec<u8> {
    let mut body = WireBody(byte_order, Vec::new())
        .u32(window)
        .u32(detail)
        .u16(device_id)
        .u16(u16::try_from(modifiers.len()).unwrap())
        .bytes(&[1, 0, 0, 0]);
    for modifier in modifiers {
        body = body.u32(*modifier);
    }
    body.1
}

#[test]
fn xi_dynamic_request_swap_select_events_big_endian_matches_little_endian() {
    const CLIENT_ID: u32 = 1;
    let mut states = Vec::new();
    for byte_order in [ClientByteOrder::LittleEndian, ClientByteOrder::BigEndian] {
        let mut state = ServerState::new();
        let mut peer = install_capture_client(&mut state, CLIENT_ID);
        state.clients.get_mut(&CLIENT_ID).unwrap().byte_order = byte_order;
        let mut backend = RecordingBackend::new();
        let _ = xi_dynamic_grab_source(&mut state, 91, false, true, "select-a");
        let _ = xi_dynamic_grab_source(&mut state, 92, false, true, "select-b");
        let body = xi_dynamic_select_wire_body(
            byte_order,
            ROOT_WINDOW.0,
            &[
                (6, [0x04, 0, 0, 0]),
                (7, [0x08, 0, 0, 0]),
                (1, [0x10, 0, 0, 0]),
            ],
        );
        let reply = send_xi_wire_request(
            &mut state,
            &mut backend,
            &mut peer,
            CLIENT_ID,
            1,
            46,
            byte_order,
            body,
        );
        assert!(reply.is_empty(), "selection should not reply: {reply:02x?}");
        states.push(state.clients[&CLIENT_ID].xi2_masks.clone());
    }
    assert_eq!(
        states[0], states[1],
        "BE selections must match LE selections"
    );
    assert_eq!(states[1].get(&(ROOT_WINDOW, 6)), Some(&0x04));
    assert_eq!(states[1].get(&(ROOT_WINDOW, 7)), Some(&0x08));
    assert_eq!(states[1].get(&(ROOT_WINDOW, 1)), Some(&0x10));
}

#[test]
fn xi_dynamic_request_swap_passive_grab_big_endian_matches_little_endian() {
    const CLIENT_ID: u32 = 1;
    const WINDOW: u32 = 0x0010_0061;
    let mut grab_records = Vec::new();
    for byte_order in [ClientByteOrder::LittleEndian, ClientByteOrder::BigEndian] {
        let mut state = ServerState::new();
        let mut peer = install_capture_client(&mut state, CLIENT_ID);
        state.clients.get_mut(&CLIENT_ID).unwrap().byte_order = byte_order;
        let mut backend = RecordingBackend::new();
        let (_, keyboard_id) = xi_dynamic_grab_source(&mut state, 93, true, false, "grab-keyboard");
        let body = xi_dynamic_passive_grab_wire_body(
            byte_order,
            WINDOW,
            67,
            keyboard_id,
            &[0x02, 0, 0, 0],
            &[4, 1],
        );
        let reply = send_xi_wire_request(
            &mut state,
            &mut backend,
            &mut peer,
            CLIENT_ID,
            1,
            54,
            byte_order,
            body,
        );
        assert_eq!(reply.len(), 32, "passive grab reply: {reply:02x?}");
        let records: Vec<_> = state
            .key_grabs
            .iter()
            .map(|grab| (grab.device_id, grab.keycode, grab.modifiers, grab.xi2_mask))
            .collect();
        grab_records.push(records);
    }
    assert_eq!(grab_records[0], grab_records[1]);
    assert_eq!(
        grab_records[1],
        vec![(6, 67, 4, 2), (6, 67, 1, 2)],
        "both declared modifiers and byte mask must retain their LE values"
    );
}

#[test]
fn xi_dynamic_request_swap_passive_ungrab_big_endian_matches_little_endian() {
    const CLIENT_ID: u32 = 1;
    const WINDOW: u32 = 0x0010_0062;
    let mut remaining_grabs = Vec::new();
    for byte_order in [ClientByteOrder::LittleEndian, ClientByteOrder::BigEndian] {
        let mut state = ServerState::new();
        let mut peer = install_capture_client(&mut state, CLIENT_ID);
        state.clients.get_mut(&CLIENT_ID).unwrap().byte_order = byte_order;
        let mut backend = RecordingBackend::new();
        let (_, keyboard_id) =
            xi_dynamic_grab_source(&mut state, 94, true, false, "ungrab-keyboard");
        let (_, other_keyboard_id) =
            xi_dynamic_grab_source(&mut state, 95, true, false, "other-keyboard");
        for device_id in [keyboard_id, other_keyboard_id] {
            let body = xi_dynamic_passive_grab_wire_body(
                byte_order,
                WINDOW,
                67,
                device_id,
                &[0x02, 0, 0, 0],
                &[4, 1],
            );
            let _ = send_xi_wire_request(
                &mut state,
                &mut backend,
                &mut peer,
                CLIENT_ID,
                1,
                54,
                byte_order,
                body,
            );
        }
        let body =
            xi_dynamic_passive_ungrab_wire_body(byte_order, WINDOW, 67, keyboard_id, &[4, 1]);
        let reply = send_xi_wire_request(
            &mut state,
            &mut backend,
            &mut peer,
            CLIENT_ID,
            2,
            55,
            byte_order,
            body,
        );
        assert!(reply.is_empty(), "passive ungrab is void: {reply:02x?}");
        remaining_grabs.push(
            state
                .key_grabs
                .iter()
                .map(|grab| (grab.device_id, grab.keycode, grab.modifiers))
                .collect::<Vec<_>>(),
        );
    }
    assert_eq!(remaining_grabs[0], remaining_grabs[1]);
    assert_eq!(remaining_grabs[1], vec![(7, 67, 4), (7, 67, 1)]);
}

#[test]
fn xi_dynamic_request_swap_rejects_select_record_overrun_without_mutation() {
    const CLIENT_ID: u32 = 1;
    let mut state = ServerState::new();
    let mut peer = install_capture_client(&mut state, CLIENT_ID);
    state.clients.get_mut(&CLIENT_ID).unwrap().byte_order = ClientByteOrder::BigEndian;
    let mut backend = RecordingBackend::new();
    let _ = xi_dynamic_grab_source(&mut state, 96, false, true, "malformed-select");

    let valid = xi_dynamic_select_wire_body(
        ClientByteOrder::BigEndian,
        ROOT_WINDOW.0,
        &[(7, [0x08, 0, 0, 0])],
    );
    let _ = send_xi_wire_request(
        &mut state,
        &mut backend,
        &mut peer,
        CLIENT_ID,
        1,
        46,
        ClientByteOrder::BigEndian,
        valid,
    );
    let before = state.clients[&CLIENT_ID].xi2_masks.clone();

    // mask_len=2 words, but only one word remains in the request body.
    let malformed = WireBody(ClientByteOrder::BigEndian, Vec::new())
        .u32(ROOT_WINDOW.0)
        .u16(1)
        .bytes(&[0, 0])
        .u16(6)
        .u16(2)
        .bytes(&[0x04, 0, 0, 0])
        .1;
    let reply = send_xi_wire_request(
        &mut state,
        &mut backend,
        &mut peer,
        CLIENT_ID,
        2,
        46,
        ClientByteOrder::BigEndian,
        malformed,
    );
    assert_eq!(reply.get(1), Some(&x11::error::BAD_LENGTH), "{reply:02x?}");
    assert_eq!(state.clients[&CLIENT_ID].xi2_masks, before);
}

#[test]
fn xi_dynamic_request_swap_rejects_modifier_overrun_without_grab() {
    const CLIENT_ID: u32 = 1;
    const WINDOW: u32 = 0x0010_0063;
    let mut state = ServerState::new();
    let mut peer = install_capture_client(&mut state, CLIENT_ID);
    state.clients.get_mut(&CLIENT_ID).unwrap().byte_order = ClientByteOrder::BigEndian;
    let mut backend = RecordingBackend::new();
    let (_, keyboard_id) = xi_dynamic_grab_source(&mut state, 97, true, false, "malformed-grab");

    let mut malformed = WireBody(ClientByteOrder::BigEndian, Vec::new())
        .u32(0)
        .u32(WINDOW)
        .u32(0)
        .u32(67)
        .u16(keyboard_id)
        .u16(2)
        .u16(0)
        .bytes(&[1, 1, 1, 0, 0, 0])
        .u32(4)
        .1;
    assert_eq!(malformed.len(), 32);
    let reply = send_xi_wire_request(
        &mut state,
        &mut backend,
        &mut peer,
        CLIENT_ID,
        1,
        54,
        ClientByteOrder::BigEndian,
        std::mem::take(&mut malformed),
    );
    assert_eq!(reply.get(1), Some(&x11::error::BAD_LENGTH), "{reply:02x?}");
    assert!(
        state.key_grabs.is_empty(),
        "malformed modifiers install no grab"
    );
}

#[test]
fn xi_select_events_keeps_bit_32_and_rejects_bits_past_the_last_event() {
    // Xvfb: select dev 2 = {2, 30, 31, 32} (mask_len 2) + dev 3 = {3};
    // XIGetSelectedEvents returns exactly
    //   02000200 040000c0 01000000 03000100 08000000
    // (masks in device order, trailing zero words trimmed). Selecting
    // bit 33 → BadValue(33), bit 69 → BadValue(69), and the rejected
    // request changes nothing.
    const W: u32 = 0x0040_0001;
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    seed_mapped_window_at_100(&mut state, W);
    let lo = (1u32 << 2) | (1 << 30) | (1 << 31);
    let body = xi_select_body(W, &[(2, &[lo, 1]), (3, &[1 << 3])]);
    let bytes = send_xi_request(&mut state, &mut backend, &mut peer, 46, &body);
    assert!(bytes.is_empty(), "{bytes:02x?}");
    let expected_tail: [u8; 20] = [
        0x02, 0x00, 0x02, 0x00, 0x04, 0x00, 0x00, 0xc0, 0x01, 0x00, 0x00, 0x00, 0x03, 0x00, 0x01,
        0x00, 0x08, 0x00, 0x00, 0x00,
    ];
    let reply = send_xi_request(&mut state, &mut backend, &mut peer, 60, &W.to_le_bytes());
    assert_eq!(reply.len(), 52, "{reply:02x?}");
    assert_eq!(&reply[4..8], &5u32.to_le_bytes(), "reply length");
    assert_eq!(&reply[8..10], &2u16.to_le_bytes(), "num_masks");
    assert_eq!(&reply[32..], &expected_tail);

    let body = xi_select_body(W, &[(2, &[0, 1 << 1])]);
    let bytes = send_xi_request(&mut state, &mut backend, &mut peer, 46, &body);
    assert_xi_error(&bytes, x11::error::BAD_VALUE, 33, 46);
    let body = xi_select_body(W, &[(2, &[1 << 2, 0, 1 << 5])]);
    let bytes = send_xi_request(&mut state, &mut backend, &mut peer, 46, &body);
    assert_xi_error(&bytes, x11::error::BAD_VALUE, 69, 46);
    let reply = send_xi_request(&mut state, &mut backend, &mut peer, 60, &W.to_le_bytes());
    assert_eq!(
        &reply[32..],
        &expected_tail,
        "rejected selects applied nothing"
    );
}

#[test]
fn xi_get_selected_events_header_fields_use_client_byte_order() {
    // Xvfb, big-endian client, same selection as above:
    //   00020002 040000c0 01000000 00030001 08000000
    // deviceid/mask_len/num_masks swap; the mask bytes don't.
    const W: u32 = 0x0040_0001;
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    seed_mapped_window_at_100(&mut state, W);
    state.clients.get_mut(&1).unwrap().byte_order = ClientByteOrder::BigEndian;
    let lo = (1u32 << 2) | (1 << 30) | (1 << 31);
    // Request bodies reach the handler already swapped to LE.
    let body = xi_select_body(W, &[(2, &[lo, 1]), (3, &[1 << 3])]);
    let _ = send_xi_request(&mut state, &mut backend, &mut peer, 46, &body);
    let reply = send_xi_request(&mut state, &mut backend, &mut peer, 60, &W.to_le_bytes());
    assert_eq!(&reply[4..8], &5u32.to_be_bytes(), "reply length");
    assert_eq!(&reply[8..10], &2u16.to_be_bytes(), "num_masks");
    assert_eq!(
        &reply[32..],
        &[
            0x00, 0x02, 0x00, 0x02, 0x04, 0x00, 0x00, 0xc0, 0x01, 0x00, 0x00, 0x00, 0x00, 0x03,
            0x00, 0x01, 0x08, 0x00, 0x00, 0x00,
        ]
    );
}

#[test]
fn xi_get_device_key_mapping_matches_core_keyboard_mapping() {
    // yserver models a single keyboard, so XI1 GetDeviceKeyMapping on
    // a key-class device must surface the same keysyms a client reads
    // via core GetKeyboardMapping (Xorg drives both from
    // XkbGetCoreMap). Inject deterministic ChangeKeyboardMapping rows
    // and assert the two replies carry identical map data.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    // RecordingBackend reports keysyms-per-keycode = 2; match it.
    state.keymap_overrides.insert(10, vec![0x0061, 0x0041]);
    state.keymap_overrides.insert(11, vec![0x0062, 0x0042]);

    // Core GetKeyboardMapping { first=10, count=2 }.
    handle_get_keyboard_mapping(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        &[10u8, 2],
    )
    .expect("core");
    let core = read_all_available(&mut peer);
    // byte[1] = keysyms-per-keycode; payload from byte 32.
    let core_kpc = core[1];
    let core_syms = core[32..].to_vec();

    // XI1 GetDeviceKeyMapping on master keyboard (device 3),
    // { deviceid=3, first=10, count=2 }.
    let header = RequestHeader {
        opcode: 137,
        data: 24,
        length_units: 2,
    };
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(2),
        header,
        &[3u8, 10, 2, 0],
    )
    .expect("xi1");
    let xi = read_all_available(&mut peer);

    assert_eq!(xi[0], 1, "X_Reply");
    assert_eq!(xi[1], 24, "RepType = X_GetDeviceKeyMapping");
    // byte[8] = keysyms-per-keycode for XI1 (vs byte[1] for core).
    assert_eq!(xi[8], core_kpc, "XI1 kpc matches core kpc");
    assert_eq!(
        &xi[32..],
        &core_syms[..],
        "XI1 device keymap payload matches core keyboard mapping"
    );
    // And the injected keysyms actually came through.
    let expect: Vec<u8> = [0x0061u32, 0x0041, 0x0062, 0x0042]
        .iter()
        .flat_map(|k| k.to_le_bytes())
        .collect();
    assert_eq!(&xi[32..], &expect[..], "injected keysyms surfaced");
}

#[test]
fn xi_get_device_key_mapping_on_pointer_device_is_bad_match() {
    // Device 2 is the master pointer — no key class → BadMatch
    // (Xorg getkmap.c: dev->key == NULL → BadMatch).
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let header = RequestHeader {
        opcode: 137,
        data: 24,
        length_units: 2,
    };
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        header,
        &[2u8, 8, 1, 0],
    )
    .expect("process");
    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes[0], 0, "error packet");
    assert_eq!(bytes[1], x11::error::BAD_MATCH, "BadMatch");
}

#[test]
fn xi_change_device_key_mapping_round_trips_via_get() {
    // XI1 ChangeDeviceKeyMapping (minor 25) must write the same
    // `keymap_overrides` rows that GetDeviceKeyMapping (minor 24) /
    // core GetKeyboardMapping read back — Xorg drives both off the
    // shared XkbGetCoreMap. Regression for the XTS
    // `XChangeDeviceKeyMapping-3` round-trip that used to fail
    // because minor 25 emitted MappingNotify but never stored.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();

    // ChangeDeviceKeyMapping { deviceid=3, first=10, kpk=2, count=2 }
    // with fresh keysyms (distinct from any read-path defaults).
    let mut body: Vec<u8> = vec![3, 10, 2, 2];
    for k in [0x0078u32, 0x0058, 0x0079, 0x0059] {
        body.extend_from_slice(&k.to_le_bytes());
    }
    let header = RequestHeader {
        opcode: 137,
        data: 25,
        length_units: 2 + 2 * 2,
    };
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        header,
        &body,
    )
    .expect("change");
    // Drain the MappingNotify the change fans out.
    let _ = read_all_available(&mut peer);

    // The override store now reflects the write.
    assert_eq!(
        state.keymap_overrides.get(&10),
        Some(&vec![0x0078u32, 0x0058])
    );
    assert_eq!(
        state.keymap_overrides.get(&11),
        Some(&vec![0x0079u32, 0x0059])
    );

    // GetDeviceKeyMapping { deviceid=3, first=10, count=2 } surfaces it.
    let header = RequestHeader {
        opcode: 137,
        data: 24,
        length_units: 2,
    };
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(2),
        header,
        &[3u8, 10, 2, 0],
    )
    .expect("get");
    let xi = read_all_available(&mut peer);
    assert_eq!(xi[0], 1, "X_Reply");
    assert_eq!(xi[1], 24, "RepType = X_GetDeviceKeyMapping");
    let expect: Vec<u8> = [0x0078u32, 0x0058, 0x0079, 0x0059]
        .iter()
        .flat_map(|k| k.to_le_bytes())
        .collect();
    assert_eq!(
        &xi[32..32 + expect.len()],
        &expect[..],
        "written keysyms read back"
    );
}

#[test]
fn xi_get_device_motion_events_pointer_returns_history() {
    let mut state = ServerState::new();
    state.start_instant = std::time::Instant::now() - std::time::Duration::from_secs(1);
    state
        .pointer_motion_history
        .push_back(crate::server::PointerMotionRecord {
            time: 500,
            root_x: 120,
            root_y: 45,
        });
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let header = RequestHeader {
        opcode: 137,
        data: 10,
        length_units: 4,
    };
    // { start:CARD32, stop:CARD32, deviceid@8 }.
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        header,
        &[144, 1, 0, 0, 88, 2, 0, 0, 2, 0, 0, 0], // 400..600
    )
    .expect("process");
    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes.len(), 44);
    assert_eq!(bytes[0], 1, "X_Reply");
    assert_eq!(bytes[1], 10, "RepType = X_GetDeviceMotionEvents");
    assert_eq!(&bytes[8..12], &1u32.to_le_bytes(), "nEvents");
    assert_eq!(bytes[12], 2, "master pointer reports two valuators");
    assert_eq!(bytes[13], 1, "mode = Absolute");
    assert_eq!(u32::from_le_bytes(bytes[32..36].try_into().unwrap()), 500);
    assert_eq!(i32::from_le_bytes(bytes[36..40].try_into().unwrap()), 120);
    assert_eq!(i32::from_le_bytes(bytes[40..44].try_into().unwrap()), 45);
}

#[test]
fn xi_get_device_motion_events_keyboard_is_bad_match() {
    // Device 3 = master keyboard (no valuators) → BadMatch
    // (Xorg gtmotion.c: v == NULL → BadMatch).
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let header = RequestHeader {
        opcode: 137,
        data: 10,
        length_units: 4,
    };
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        header,
        &[0, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0],
    )
    .expect("process");
    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes[0], 0, "error packet");
    assert_eq!(bytes[1], x11::error::BAD_MATCH, "BadMatch");
}

#[test]
fn xi_get_feedback_control_kbd_mirrors_keyboard_control() {
    // The XI1 KbdFeedbackState must carry the same bell/click/LED
    // state as core GetKeyboardControl (Xorg: the KbdFeedback ctrl
    // *is* the keyboard control).
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    state.keyboard_control.bell_pitch = 0x0321;
    state.keyboard_control.bell_duration = 0x0654;
    state.keyboard_control.bell_percent = 77;
    state.keyboard_control.key_click_percent = 42;
    state.keyboard_control.led_mask = 0x0000_0003;
    state.keyboard_control.global_auto_repeat = true;

    // GetFeedbackControl { deviceid=3 } (master keyboard).
    let header = RequestHeader {
        opcode: 137,
        data: 22,
        length_units: 2,
    };
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        header,
        &[3u8, 0, 0, 0],
    )
    .expect("process");
    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes[0], 1, "X_Reply");
    assert_eq!(bytes[1], 22, "RepType");
    assert_eq!(&bytes[8..10], &1u16.to_le_bytes(), "num_feedbacks");
    // KbdFeedbackState begins at byte 32.
    let kbd = &bytes[32..];
    assert_eq!(kbd[0], 0, "KbdFeedbackClass");
    assert_eq!(&kbd[4..6], &0x0321u16.to_le_bytes(), "pitch == bell_pitch");
    assert_eq!(&kbd[6..8], &0x0654u16.to_le_bytes(), "duration");
    assert_eq!(&kbd[8..12], &0x0000_0003u32.to_le_bytes(), "led_mask");
    assert_eq!(kbd[16], 1, "global_auto_repeat");
    assert_eq!(kbd[17], 42, "click == key_click_percent");
    assert_eq!(kbd[18], 77, "percent == bell_percent");
}

#[test]
fn xi_get_feedback_control_ptr_mirrors_pointer_control() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    state.pointer_control.accel_numerator = 5;
    state.pointer_control.accel_denominator = 3;
    state.pointer_control.threshold = 9;

    // GetFeedbackControl { deviceid=2 } (master pointer).
    let header = RequestHeader {
        opcode: 137,
        data: 22,
        length_units: 2,
    };
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        header,
        &[2u8, 0, 0, 0],
    )
    .expect("process");
    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes[1], 22, "RepType");
    assert_eq!(&bytes[8..10], &1u16.to_le_bytes(), "num_feedbacks");
    let ptr = &bytes[32..];
    assert_eq!(ptr[0], 1, "PtrFeedbackClass");
    assert_eq!(&ptr[6..8], &5u16.to_le_bytes(), "accelNum");
    assert_eq!(&ptr[8..10], &3u16.to_le_bytes(), "accelDenom");
    assert_eq!(&ptr[10..12], &9u16.to_le_bytes(), "threshold");
}

#[test]
fn xi_change_keyboard_feedback_updates_shared_control_atomically() {
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let mut body = vec![0; 28];
    body[0..4].copy_from_slice(&0x1fu32.to_le_bytes());
    body[4] = 3; // key device
    body[5] = 0; // KbdFeedbackClass
    body[9] = 0; // feedback id
    body[10..12].copy_from_slice(&20u16.to_le_bytes());
    body[14] = 42;
    body[15] = 77;
    body[16..18].copy_from_slice(&801i16.to_le_bytes());
    body[18..20].copy_from_slice(&321i16.to_le_bytes());
    body[20..24].copy_from_slice(&3u32.to_le_bytes());
    body[24..28].copy_from_slice(&2u32.to_le_bytes());

    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        xi2_header_for_body(23, &body),
        &body,
    )
    .unwrap();
    assert_eq!(state.keyboard_control.key_click_percent, 42);
    assert_eq!(state.keyboard_control.bell_percent, 77);
    assert_eq!(state.keyboard_control.bell_pitch, 801);
    assert_eq!(state.keyboard_control.bell_duration, 321);
    assert_eq!(state.keyboard_control.led_mask, 2);

    let before = state.keyboard_control.clone();
    body[14] = 10;
    body[15] = 101;
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(2),
        xi2_header_for_body(23, &body),
        &body,
    )
    .unwrap();
    assert_eq!(
        state.keyboard_control.key_click_percent,
        before.key_click_percent
    );
    assert_eq!(state.keyboard_control.bell_percent, before.bell_percent);
}

#[test]
fn xi_change_pointer_feedback_updates_shared_control() {
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let mut body = vec![0; 20];
    body[0..4].copy_from_slice(&7u32.to_le_bytes());
    body[4] = 2; // pointer device
    body[5] = 1; // PtrFeedbackClass
    body[8] = 1;
    body[9] = 0;
    body[10..12].copy_from_slice(&12u16.to_le_bytes());
    body[14..16].copy_from_slice(&5i16.to_le_bytes());
    body[16..18].copy_from_slice(&3i16.to_le_bytes());
    body[18..20].copy_from_slice(&9i16.to_le_bytes());

    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        xi2_header_for_body(23, &body),
        &body,
    )
    .unwrap();
    assert_eq!(state.pointer_control.accel_numerator, 5);
    assert_eq!(state.pointer_control.accel_denominator, 3);
    assert_eq!(state.pointer_control.threshold, 9);
}

#[test]
fn xi_change_device_resolution_zero_round_trips() {
    // A fresh master pointer has two valuators in Xorg's CorePointerProc
    // (`dix/devices.c:662-690`), so DEVICE_RESOLUTION returns two axes.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let mut change = vec![0; 16];
    change[0..2].copy_from_slice(&1u16.to_le_bytes());
    change[2] = 2;
    change[4..6].copy_from_slice(&1u16.to_le_bytes());
    change[6..8].copy_from_slice(&12u16.to_le_bytes());
    change[8] = 0;
    change[9] = 1;

    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        xi2_header_for_body(35, &change),
        &change,
    )
    .unwrap();
    let change_reply = read_all_available(&mut peer);
    assert_eq!(change_reply[0], 1);
    assert_eq!(change_reply[8], 0, "Success");

    let get = [1, 0, 2, 0];
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(2),
        xi2_header_for_body(34, &get),
        &get,
    )
    .unwrap();
    let wire = read_all_available(&mut peer);
    assert_eq!(wire.len(), 64);
    assert_eq!(u32::from_le_bytes(wire[36..40].try_into().unwrap()), 2);
    for value in wire[40..].chunks_exact(4) {
        assert_eq!(u32::from_le_bytes(value.try_into().unwrap()), 0);
    }
}

#[test]
fn xi_query_version_negotiates_up_to_server_24() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let header = RequestHeader {
        opcode: 131,
        data: 47,
        length_units: 2,
    };

    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        header,
        &[2, 0, 4, 0],
    )
    .expect("XIQueryVersion 2.4");
    let wire = read_all_available(&mut peer);
    assert_eq!(wire.len(), 32, "XIQueryVersion reply size");
    assert_eq!(u16::from_le_bytes([wire[8], wire[9]]), 2);
    assert_eq!(u16::from_le_bytes([wire[10], wire[11]]), 4);

    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(2),
        header,
        &[2, 0, 3, 0],
    )
    .expect("XIQueryVersion 2.3");
    let wire = read_all_available(&mut peer);
    assert_eq!(wire.len(), 32, "XIQueryVersion reply size");
    assert_eq!(u16::from_le_bytes([wire[8], wire[9]]), 2);
    assert_eq!(u16::from_le_bytes([wire[10], wire[11]]), 3);
}

/// XI1 GetExtensionVersion answers the server's XI version, as Xorg's
/// `ProcXGetExtensionVersion` (Xi/getvers.c: `XIVersion` = 2.4; Xvfb's
/// `xinput --version` reports "XI version on server: 2.4"), with
/// RepType = X_GetExtensionVersion. libXi decides from this reply
/// whether XI 2.2 fields such as a raw event's `sourceid` are valid:
/// the old hard-coded 2.0 made `xinput test-xi2 --root` print every
/// raw key event as `device: 3 (0)` (#173; Xorg: `3 (5)`). A length
/// that doesn't match `nbytes` is BadLength.
#[test]
fn xi_get_extension_version_reports_the_server_version_like_xorg() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let name = b"XInputExtension";
    let mut body = vec![name.len() as u8, 0, 0, 0];
    body.extend_from_slice(name);
    body.push(0);
    let header = RequestHeader {
        opcode: 131,
        data: 1,
        length_units: 6,
    };
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        header,
        &body,
    )
    .expect("GetExtensionVersion");
    let wire = read_all_available(&mut peer);
    assert_eq!(wire.len(), 32, "reply size");
    assert_eq!((wire[0], wire[1]), (1, 1), "X_Reply, RepType");
    assert_eq!(u16::from_le_bytes([wire[8], wire[9]]), 2, "major");
    assert_eq!(u16::from_le_bytes([wire[10], wire[11]]), 4, "minor");
    assert_eq!(wire[12], 1, "present");

    let short = RequestHeader {
        length_units: 5,
        ..header
    };
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(2),
        short,
        &body[..16],
    )
    .expect("GetExtensionVersion, wrong length");
    let wire = read_all_available(&mut peer);
    assert_eq!((wire[0], wire[1]), (0, x11::error::BAD_LENGTH), "BadLength");
}

/// The XI version a client announced is remembered the way Xorg's
/// `ProcXIQueryVersion` stores it, because `FilterRawEvents` reads it.
/// Xvfb capture (probe mon:…:0,2 / 2,0 / 3,2, then a keyboard grab):
/// 2.0 then 2.2 stays 2.0 (still filtered as XI 2.0); 2.2 then 2.0
/// keeps 2.2 (Xorg answers the 2.0 query with BadValue); 2.3 then 2.2
/// keeps 2.3; a client that never asks has no version (not filtered).
#[test]
fn xi_query_version_records_client_version_like_xorg() {
    let header = RequestHeader {
        opcode: 131,
        data: 47,
        length_units: 2,
    };
    let stored_after = |minors: &[u16]| {
        let mut state = ServerState::new();
        let _peer = install_client(&mut state, 1);
        let mut backend = RecordingBackend::new();
        for (i, minor) in minors.iter().enumerate() {
            let m = minor.to_le_bytes();
            handle_xi2_request(
                &mut state,
                &mut backend,
                None,
                ClientId(1),
                SequenceNumber(u16::try_from(i + 1).unwrap()),
                header,
                &[2, 0, m[0], m[1]],
            )
            .expect("XIQueryVersion");
        }
        state.xi2_client_versions.get(&ClientId(1)).copied()
    };
    assert_eq!(stored_after(&[]), None);
    assert_eq!(stored_after(&[0]), Some((2, 0)));
    assert_eq!(stored_after(&[0, 2]), Some((2, 0)));
    assert_eq!(stored_after(&[2, 0]), Some((2, 2)));
    assert_eq!(stored_after(&[3, 2]), Some((2, 3)));
    assert_eq!(stored_after(&[2, 3]), Some((2, 3)));
    // Capped at the server's version, as the reply is.
    assert_eq!(stored_after(&[9]), Some((2, 4)));
}

/// XIGrabDevice(keyboard) and an XI2 passive key grab keep the grab's
/// event mask: Xorg delivers an XI2 raw key event to the grab owner
/// only when that mask selects it (DeliverOneGrabbedEvent reads
/// `grab->xi2mask`).
#[test]
fn xi2_keyboard_grabs_keep_their_event_mask() {
    const MASK: u32 = (1 << 2) | (1 << 3) | (1 << 13) | (1 << 14);
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();

    let mut body = Vec::new();
    body.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes()); // window
    body.extend_from_slice(&0u32.to_le_bytes()); // time
    body.extend_from_slice(&0u32.to_le_bytes()); // cursor
    body.extend_from_slice(&3u16.to_le_bytes()); // master keyboard
    body.extend_from_slice(&[1, 1, 0, 0]); // async, async, owner_events=0, pad
    body.extend_from_slice(&1u16.to_le_bytes()); // mask_len
    body.extend_from_slice(&MASK.to_le_bytes());
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode: 131,
            data: 51,
            length_units: 7,
        },
        &body,
    )
    .expect("XIGrabDevice keyboard");
    assert_eq!(state.active_keyboard_grab.map(|g| g.xi2_mask), Some(MASK));

    let mut body = Vec::new();
    body.extend_from_slice(&0u32.to_le_bytes()); // time
    body.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes()); // grab_window
    body.extend_from_slice(&0u32.to_le_bytes()); // cursor
    body.extend_from_slice(&38u32.to_le_bytes()); // detail
    body.extend_from_slice(&3u16.to_le_bytes()); // deviceid
    body.extend_from_slice(&1u16.to_le_bytes()); // num_modifiers
    body.extend_from_slice(&1u16.to_le_bytes()); // mask_len
    body.extend_from_slice(&[1, 1, 1, 0]); // Keycode, async, async, owner_events
    body.extend_from_slice(&0u16.to_le_bytes()); // pad
    body.extend_from_slice(&MASK.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // modifier 0
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(2),
        RequestHeader {
            opcode: 131,
            data: 54,
            length_units: 9,
        },
        &body,
    )
    .expect("XIPassiveGrabDevice keycode");
    assert_eq!(state.key_grabs.last().map(|g| g.xi2_mask), Some(MASK));
}

#[test]
fn xi_select_events_on_root_bootstraps_device_changed() {
    // Xorg initializes the core pair with CorePointerProc (devices.c:724-730,
    // :655-694), so a fresh master bootstrap has 3 classes, not the old
    // unconditional physical-pointer set of 7.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let header = RequestHeader {
        opcode: 131,
        data: 46,
        length_units: 5,
    };
    let mut body = Vec::new();
    body.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes()); // XIAllDevices
    body.extend_from_slice(&1u16.to_le_bytes()); // 1 mask word
    body.extend_from_slice(&XI2_DEVICE_CHANGED_MASK.to_le_bytes());

    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        header,
        &body,
    )
    .expect("XISelectEvents");

    let wire = read_all_available(&mut peer);
    let event_offset = (0..wire.len().saturating_sub(32))
        .find(|&i| wire[i] == 35 && u16::from_le_bytes([wire[i + 8], wire[i + 9]]) == 1)
        .expect("bootstrap XI_DeviceChanged event");
    assert_eq!(wire[event_offset + 10], 2, "deviceid low byte");
    assert_eq!(
        wire[event_offset + 16],
        3,
        "the initial master uses CorePointerProc's three classes"
    );
    assert_eq!(
        wire[event_offset + 18],
        2,
        "sourceid falls back to master pointer"
    );
}

#[test]
fn xi_slave_switch_bootstrap_uses_last_pointer_slave_or_master_classes() {
    use crate::{
        backend::recording::RecordingBackend,
        core_loop::{DeviceInfo, InputOrigin, pointer_fanout::pointer_event_fanout_to_state},
        host_x11::{HostPointerEvent, HostXidMap, PointerEventKind},
        xinput::{InputCapabilities, InputSourceId, XiFacetKind},
    };

    fn select_master_pointer(
        state: &mut ServerState,
        backend: &mut RecordingBackend,
        client: u32,
        sequence: u16,
    ) {
        let mut body = Vec::new();
        body.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
        body.extend_from_slice(&1u16.to_le_bytes());
        body.extend_from_slice(&0u16.to_le_bytes());
        body.extend_from_slice(&2u16.to_le_bytes());
        body.extend_from_slice(&1u16.to_le_bytes());
        body.extend_from_slice(&XI2_DEVICE_CHANGED_MASK.to_le_bytes());
        handle_xi2_request(
            state,
            backend,
            None,
            ClientId(client),
            SequenceNumber(sequence),
            RequestHeader {
                opcode: 131,
                data: 46,
                length_units: 5,
            },
            &body,
        )
        .expect("XISelectEvents on master pointer");
    }

    fn device_changed(bytes: &[u8]) -> (u16, u16, u8, u16, Option<i32>) {
        let offset = (0..bytes.len().saturating_sub(32))
            .find(|&offset| {
                bytes[offset] == 35
                    && u16::from_le_bytes([bytes[offset + 8], bytes[offset + 9]]) == 1
            })
            .unwrap_or_else(|| panic!("bootstrap XI_DeviceChanged event missing: {bytes:?}"));
        let deviceid = u16::from_le_bytes([bytes[offset + 10], bytes[offset + 11]]);
        let num_classes = u16::from_le_bytes([bytes[offset + 16], bytes[offset + 17]]);
        let sourceid = u16::from_le_bytes([bytes[offset + 18], bytes[offset + 19]]);
        let reason = bytes[offset + 20];
        let mut class_offset = offset + 32;
        let mut valuator_two = None;
        for _ in 0..num_classes {
            let class_type = u16::from_le_bytes([bytes[class_offset], bytes[class_offset + 1]]);
            let units =
                u16::from_le_bytes([bytes[class_offset + 2], bytes[class_offset + 3]]) as usize;
            let class_source =
                u16::from_le_bytes([bytes[class_offset + 4], bytes[class_offset + 5]]);
            if class_type == 2
                && u16::from_le_bytes([bytes[class_offset + 6], bytes[class_offset + 7]]) == 2
            {
                valuator_two = Some((
                    class_source,
                    i32::from_le_bytes(
                        bytes[class_offset + 28..class_offset + 32]
                            .try_into()
                            .unwrap(),
                    ),
                ));
            }
            class_offset += units * 4;
        }
        let scroll_value = valuator_two.map(|(class_source, value)| {
            assert_eq!(class_source, sourceid, "classes use the reported source");
            value
        });
        (deviceid, sourceid, reason, num_classes, scroll_value)
    }

    let mut state = ServerState::new();
    let mut bootstrap_peer = install_capture_client(&mut state, 101);
    let mut backend = RecordingBackend::new();

    // Before: this expected the physical pointer shape on a fresh master.
    // Xorg initializes the core master pair through CorePointerProc
    // (`dix/devices.c:724-730, 655-694`), so the initial master has three
    // classes and no valuator 2; XIQueryDevice serializes stored source IDs
    // (`Xi/xiquerydevice.c:278,326`).
    select_master_pointer(&mut state, &mut backend, 101, 1);
    assert_eq!(
        state
            .clients
            .get(&101)
            .unwrap()
            .xi2_masks
            .get(&(ROOT_WINDOW, 2)),
        Some(&u64::from(XI2_DEVICE_CHANGED_MASK)),
        "master pointer DeviceChanged selection was installed"
    );
    let fallback = read_all_available(&mut bootstrap_peer);
    assert_eq!(
        device_changed(&fallback),
        (2, 2, 1, 3, None),
        "the initial class set has CorePointerProc shape and sourceid 2"
    );

    let source = InputSourceId(0xA71);
    let info = DeviceInfo {
        source_id: source,
        enabled: true,
        resume_key: None,
        capabilities: InputCapabilities {
            keyboard: false,
            pointer: true,
            touch: false,
        },
        name: "Razer mouse".to_owned(),
        device_node: "/dev/input/event-razer".to_owned(),
        sysname: "event-razer".to_owned(),
        vendor_id: 1,
        product_id: 2,
        is_touchpad: false,
        config: Default::default(),
    };
    let source_id = state.xi_register_source(&info)[0];
    let wheel = HostPointerEvent {
        origin: InputOrigin::Physical(source),
        kind: PointerEventKind::ButtonPress,
        detail: 5,
        host_xid: 0,
        time: 2,
        root_x: 10,
        root_y: 20,
        event_x: 10,
        event_y: 20,
        state: 0,
        crossing_mode: 0,
        child: 0,
        raw_dx: 0,
        raw_dy: 0,
        tree_change: false,
    };
    let _dropped = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &HostXidMap::new(),
        wheel,
        true,
        false,
    );
    let release = HostPointerEvent {
        kind: PointerEventKind::ButtonRelease,
        time: 3,
        ..wheel
    };
    let _dropped = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &HostXidMap::new(),
        release,
        true,
        false,
    );
    let _ = read_all_available(&mut bootstrap_peer);

    let mut late_peer = install_capture_client(&mut state, 102);
    select_master_pointer(&mut state, &mut backend, 102, 2);
    assert_eq!(
        device_changed(&read_all_available(&mut late_peer)),
        (2, source_id, 1, 7, Some(1)),
        "a late selector bootstraps from the stored physical class set and scroll state"
    );
    assert_eq!(state.xi_last_slave(2), Some(source_id));
    assert_eq!(
        state
            .xi_devices
            .device(source_id)
            .unwrap()
            .scroll_axis_values,
        [1, 0]
    );
    assert_eq!(state.xi_devices.devices().len(), 5);
    assert!(state.xi_devices.source(source).is_some());
    assert_eq!(
        state.xi_devices.facet(source, XiFacetKind::PointerTouch),
        Some(source_id)
    );
    assert_eq!(state.buttons_down, 0);
    assert!(state.sync_pending.is_empty());
    assert!(state.unpublished_pointer_buttons_down.is_empty());
}

#[test]
fn xi_device_changed_device_change_reason_uses_existing_facet_classes() {
    use crate::{
        core_loop::DeviceInfo,
        xinput::{InputCapabilities, InputSourceId, XiFacetKind},
    };

    let mut state = ServerState::new();
    let mut selected = install_client(&mut state, 111);
    let mut unrelated = install_client(&mut state, 112);
    let source = InputSourceId(0xA72);
    let info = DeviceInfo {
        source_id: source,
        enabled: true,
        resume_key: None,
        capabilities: InputCapabilities {
            keyboard: false,
            pointer: true,
            touch: false,
        },
        name: "HyperX mouse".to_owned(),
        device_node: "/dev/input/event-hyperx".to_owned(),
        sysname: "event-hyperx".to_owned(),
        vendor_id: 3,
        product_id: 4,
        is_touchpad: false,
        config: Default::default(),
    };
    let device_id = state.xi_register_source(&info)[0];
    state
        .clients
        .get_mut(&111)
        .unwrap()
        .xi2_masks
        .insert((ROOT_WINDOW, device_id), u64::from(XI2_DEVICE_CHANGED_MASK));

    let dropped = crate::xinput::hotplug::emit_xi2_device_changed(
        &mut state,
        device_id,
        crate::xinput::hotplug::XiDeviceChangeReason::DeviceChange,
        device_id,
    );
    assert!(dropped.is_empty());
    let wire = read_all_available(&mut selected);
    let offset = (0..wire.len().saturating_sub(32))
        .find(|&offset| {
            wire[offset] == 35 && u16::from_le_bytes([wire[offset + 8], wire[offset + 9]]) == 1
        })
        .expect("XI_DeviceChanged class update");
    assert_eq!(
        u16::from_le_bytes([wire[offset + 10], wire[offset + 11]]),
        device_id
    );
    assert_eq!(
        u16::from_le_bytes([wire[offset + 18], wire[offset + 19]]),
        device_id
    );
    assert_eq!(wire[offset + 20], 2, "XI2.h XIDeviceChange");
    let num_classes = u16::from_le_bytes([wire[offset + 16], wire[offset + 17]]) as usize;
    assert_eq!(num_classes, 7);
    let mut class_offset = offset + 32;
    for _ in 0..num_classes {
        assert_eq!(
            u16::from_le_bytes([wire[class_offset + 4], wire[class_offset + 5]]),
            device_id,
            "class block identifies the changed facet"
        );
        let units = u16::from_le_bytes([wire[class_offset + 2], wire[class_offset + 3]]) as usize;
        class_offset += units * 4;
    }
    assert_eq!(
        class_offset,
        offset
            + 32
            + u32::from_le_bytes(wire[offset + 4..offset + 8].try_into().unwrap()) as usize * 4
    );
    assert!(read_all_available(&mut unrelated).is_empty());
    assert_eq!(
        state.xi_last_slave(2),
        None,
        "class-change emission is not a source switch"
    );
    assert_eq!(state.xi_devices.devices().len(), 5);
    assert!(state.xi_devices.source(source).is_some());
    assert_eq!(
        state.xi_devices.facet(source, XiFacetKind::PointerTouch),
        Some(device_id)
    );
    assert!(state.sync_pending.is_empty());
    assert!(state.unpublished_pointer_buttons_down.is_empty());
}

#[test]
fn xi_select_events_rejects_hierarchy_changed_outside_all_devices() {
    let mut state = ServerState::new();
    let mut peer = install_capture_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    for (sequence, device_id) in [1_u16, 2, 4].into_iter().enumerate() {
        let mut body = Vec::new();
        body.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
        body.extend_from_slice(&1_u16.to_le_bytes());
        body.extend_from_slice(&0_u16.to_le_bytes());
        body.extend_from_slice(&device_id.to_le_bytes());
        body.extend_from_slice(&1_u16.to_le_bytes());
        body.extend_from_slice(&(1_u32 << 11).to_le_bytes()); // XI_HierarchyChanged
        handle_xi2_request(
            &mut state,
            &mut backend,
            None,
            ClientId(1),
            SequenceNumber(u16::try_from(sequence + 1).unwrap()),
            xi2_header(46),
            &body,
        )
        .expect("XISelectEvents dispatch");

        let error = read_all_available(&mut peer);
        assert_eq!(
            error.len(),
            32,
            "BadValue error for deviceid {device_id}; client={:?}; outbound={}",
            state.clients[&1].xi2_masks,
            state.clients[&1].outbound.len(),
        );
        assert_eq!(error[0], 0, "an X11 error, not a reply");
        assert_eq!(error[1], yserver_protocol::x11::error::BAD_VALUE);
        assert_eq!(
            u32::from_le_bytes(error[4..8].try_into().unwrap()),
            11,
            "errorValue is XI_HierarchyChanged"
        );
        assert!(
            !state.clients[&1]
                .xi2_masks
                .contains_key(&(ROOT_WINDOW, device_id)),
            "the rejected selection leaves client masks unchanged"
        );
    }

    let mut all_devices = Vec::new();
    all_devices.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    all_devices.extend_from_slice(&1_u16.to_le_bytes());
    all_devices.extend_from_slice(&0_u16.to_le_bytes());
    all_devices.extend_from_slice(&0_u16.to_le_bytes()); // XIAllDevices
    all_devices.extend_from_slice(&1_u16.to_le_bytes());
    all_devices.extend_from_slice(&(1_u32 << 11).to_le_bytes());
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(8),
        xi2_header(46),
        &all_devices,
    )
    .expect("XIAllDevices may select hierarchy changes");
    assert_eq!(
        state.clients[&1].xi2_masks.get(&(ROOT_WINDOW, 0)),
        Some(&(1_u64 << 11))
    );
    assert!(read_all_available(&mut peer).is_empty());
}

// -----------------------------------------------------------------
// Tier 2 Task 4: device naming from the registry + XI1/XI2 sync
// -----------------------------------------------------------------

/// Drive XIQueryDevice (opcode 48) for XIAllDevices and parse out
/// `(id, name)` for each device in the little-endian reply. Validate
/// the reply length and every variable-sized device/class record.
fn query_device_ids_and_names(
    state: &mut ServerState,
    peer: &mut UnixStream,
) -> Vec<(u16, String)> {
    let mut backend = RecordingBackend::new();
    handle_xi2_request(
        state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        xi2_header(48),
        &[0, 0, 0, 0], // deviceid=XIAllDevices, pad
    )
    .expect("XIQueryDevice");
    let wire = read_all_available(peer);
    assert_eq!(wire[0], 1, "XIQueryDevice reply, not an X error: {wire:?}");
    let reply_length = u32::from_le_bytes(wire[4..8].try_into().unwrap()) as usize;
    assert_eq!(
        wire.len(),
        32 + reply_length * 4,
        "reply length covers all records"
    );
    let num_devices = u16::from_le_bytes([wire[8], wire[9]]) as usize;
    let mut off = 32;
    let mut out = Vec::new();
    for _ in 0..num_devices {
        assert!(
            off + 12 <= wire.len(),
            "device-info header is in reply bounds"
        );
        let id = u16::from_le_bytes([wire[off], wire[off + 1]]);
        let num_classes = u16::from_le_bytes([wire[off + 6], wire[off + 7]]) as usize;
        let name_len = u16::from_le_bytes([wire[off + 8], wire[off + 9]]) as usize;
        let name_start = off + 12;
        assert!(
            name_start + name_len <= wire.len(),
            "device name is in reply bounds"
        );
        let name = String::from_utf8(wire[name_start..name_start + name_len].to_vec()).unwrap();
        // Advance: 12-byte info header + name (padded to 4) + classes.
        let mut pos = name_start + name_len;
        while !pos.is_multiple_of(4) {
            pos += 1;
        }
        // Walk class blocks by their 4-byte-unit length field (u16 at
        // offset +2 of each class header).
        for _ in 0..num_classes {
            assert!(pos + 4 <= wire.len(), "class header is in reply bounds");
            let units = u16::from_le_bytes([wire[pos + 2], wire[pos + 3]]) as usize;
            assert!(units > 0, "XI2 class record has a nonzero length");
            pos += units * 4;
            assert!(
                pos <= wire.len(),
                "class record length stays in reply bounds"
            );
        }
        out.push((id, name));
        off = pos;
    }
    assert_eq!(off, wire.len(), "all reply bytes belong to device records");
    out
}

#[test]
fn xiquerydevice_short_body_returns_well_formed_badlength_error() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        xi2_header(48),
        &[],
    )
    .expect("malformed XIQueryDevice is handled as an X error");

    let wire = read_all_available(&mut peer);
    assert_eq!(wire.len(), 32);
    assert_eq!(&wire[0..4], &[0, x11::error::BAD_LENGTH, 1, 0]);
    assert_eq!(&wire[4..8], &[0, 0, 0, 0]);
    assert_eq!(&wire[8..11], &[48, 0, 137]);
    assert_eq!(&wire[11..], &[0; 21]);
}

/// Drive XListInputDevices (XI 1.x, major 131 minor 2) and parse out
/// `(id, name)` for each device.
fn list_input_devices_ids_and_names(
    state: &mut ServerState,
    peer: &mut UnixStream,
) -> Vec<(u16, String)> {
    let mut backend = RecordingBackend::new();
    let header = RequestHeader {
        opcode: 131,
        data: 2,
        // ListInputDevices (XI minor 2) is Fixed(1): just the 4-byte
        // request header = 1 unit. (Was 0 — pre-dated the length
        // gate, now BadLengths before the handler enumerates devices.)
        length_units: 1,
    };
    handle_xi2_request(
        state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        header,
        &[],
    )
    .expect("XListInputDevices");
    let wire = read_all_available(peer);
    let ndevices = wire[8] as usize;
    // Device-info array: 8 bytes each (type ATOM, id, num_classes,
    // use, pad). Track ids and per-device class counts.
    let mut ids = Vec::new();
    let mut class_counts = Vec::new();
    let mut off = 32;
    for _ in 0..ndevices {
        ids.push(u16::from(wire[off + 4]));
        class_counts.push(wire[off + 5] as usize);
        off += 8;
    }
    // Class-info blocks: walk by the BYTE length field (offset +1).
    for &nc in &class_counts {
        for _ in 0..nc {
            let len = wire[off + 1] as usize;
            off += len;
        }
    }
    // STR name list: 1 length byte + bytes, in device order.
    let mut out = Vec::new();
    for &id in &ids {
        let n = wire[off] as usize;
        let name = String::from_utf8(wire[off + 1..off + 1 + n].to_vec()).unwrap();
        out.push((id, name));
        off += 1 + n;
    }
    out
}

#[test]
fn xiquerydevice_reports_slave_pointer_registry_name_after_rename() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    seed_pointer_for_t3(&mut state);
    // Physical facets start at 6; 4 stays the virtual XTEST pointer.
    state
        .xi_devices
        .iter_mut()
        .find(|d| d.id == TEST_PHYSICAL_POINTER_ID)
        .unwrap()
        .name = "SynPS/2 Synaptics TouchPad".to_owned();

    let devs = query_device_ids_and_names(&mut state, &mut peer);
    let slave = devs
        .iter()
        .find(|(id, _)| *id == TEST_PHYSICAL_POINTER_ID)
        .expect("physical touchpad facet");
    assert_eq!(
        slave.1, "SynPS/2 Synaptics TouchPad",
        "XIQueryDevice must report the physical facet's registry name"
    );
    assert_eq!(
        devs.iter().find(|(id, _)| *id == 4).unwrap().1,
        crate::xinput::registry::NAME_XTEST_POINTER
    );
}

#[test]
fn xi1_and_xi2_report_same_device_ids_and_names() {
    // Default (no touchpad): both enumerations must agree.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let xi2 = query_device_ids_and_names(&mut state, &mut peer);
    let xi1 = list_input_devices_ids_and_names(&mut state, &mut peer);
    assert_eq!(xi2, xi1, "XI1 and XI2 must agree on (id, name) by default");

    // A physical touchpad gets its own ID; both enumerations must
    // still agree after that physical facet is renamed.
    seed_pointer_for_t3(&mut state);
    state
        .xi_devices
        .iter_mut()
        .find(|d| d.id == TEST_PHYSICAL_POINTER_ID)
        .unwrap()
        .name = "ETPS/2 Elantech Touchpad".to_owned();
    let xi2 = query_device_ids_and_names(&mut state, &mut peer);
    let xi1 = list_input_devices_ids_and_names(&mut state, &mut peer);
    assert_eq!(
        xi2, xi1,
        "XI1 and XI2 must agree on (id, name) after a touchpad rename"
    );
    assert_eq!(
        xi2.iter()
            .find(|(id, _)| *id == TEST_PHYSICAL_POINTER_ID)
            .unwrap()
            .1,
        "ETPS/2 Elantech Touchpad",
        "both enumerations carry the renamed physical touchpad facet"
    );
    assert_eq!(
        xi2.iter().find(|(id, _)| *id == 4).unwrap().1,
        crate::xinput::registry::NAME_XTEST_POINTER
    );
}

/// Device 4 must keep Xorg's CorePointerProc shape, while physical pointer
/// facets retain their GDK-compatible generic XI2 classes.
#[test]
fn xi_xtest_classes_query_device_4_has_core_pointer_shape() {
    // Kills routing XTEST 4 through the physical pointer class encoder:
    // that mutation returns seven classes, four axes, and scroll classes.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let physical_pointer = seed_pointer_for_t3(&mut state);
    let before = xi_query_side_effect_snapshot(&state, 1);

    // XIQueryDevice(4), not XIAllDevices, is sent through process_request.
    let (xtest_classes, xtest_num_classes) =
        query_device_class_block(&mut state, &mut peer, crate::xinput::DEVICEID_XTEST_POINTER);
    assert_eq!(xtest_num_classes, 3, "button + two valuator classes only");
    let xtest_classes_info = parse_xi2_class_headers(&xtest_classes, xtest_num_classes);
    assert_eq!(
        xtest_classes_info
            .iter()
            .map(|class| class.0)
            .collect::<Vec<_>>(),
        [1, 2, 2],
        "XTEST 4 has ButtonClass and exactly two ValuatorClass records"
    );

    let button = &xtest_classes_info[0];
    assert_eq!(button.1, 13, "10 buttons plus one state word");
    assert_eq!(u16::from_le_bytes([xtest_classes[6], xtest_classes[7]]), 10);
    assert_eq!(u16::from_le_bytes([xtest_classes[4], xtest_classes[5]]), 4);
    let expected_labels = [
        state.atoms.id_for("Button Left").unwrap().0,
        state.atoms.id_for("Button Middle").unwrap().0,
        state.atoms.id_for("Button Right").unwrap().0,
        state.atoms.id_for("Button Wheel Up").unwrap().0,
        state.atoms.id_for("Button Wheel Down").unwrap().0,
        state.atoms.id_for("Button Horiz Wheel Left").unwrap().0,
        state.atoms.id_for("Button Horiz Wheel Right").unwrap().0,
        0,
        0,
        0,
    ];
    let actual_labels = (0..10)
        .map(|index| {
            let start = 12 + index * 4;
            u32::from_le_bytes(xtest_classes[start..start + 4].try_into().unwrap())
        })
        .collect::<Vec<_>>();
    assert_eq!(
        actual_labels, expected_labels,
        "XTEST labels are the seven core labels followed by three None atoms"
    );

    for (index, expected_label) in ["Rel X", "Rel Y"].into_iter().enumerate() {
        let start = button.2 + button.1 * 4 + index * 44;
        let class = &xtest_classes[start..start + 44];
        assert_eq!(u16::from_le_bytes([class[0], class[1]]), 2);
        assert_eq!(u16::from_le_bytes([class[2], class[3]]), 11);
        assert_eq!(u16::from_le_bytes([class[4], class[5]]), 4);
        assert_eq!(
            u16::from_le_bytes([class[6], class[7]]),
            u16::try_from(index).unwrap()
        );
        assert_eq!(
            u32::from_le_bytes(class[8..12].try_into().unwrap()),
            state.atoms.id_for(expected_label).unwrap().0
        );
        assert_eq!(i32::from_le_bytes(class[12..16].try_into().unwrap()), -1);
        assert_eq!(i32::from_le_bytes(class[20..24].try_into().unwrap()), -1);
        assert_eq!(class[40], 0, "Relative mode");
    }

    // The physical pointer stays on the existing generic 7-class shape.
    let (physical_classes, physical_num_classes) =
        query_device_class_block(&mut state, &mut peer, physical_pointer);
    assert_eq!(
        physical_num_classes, 7,
        "physical pointer keeps its two scroll class declarations"
    );
    let physical_classes_info = parse_xi2_class_headers(&physical_classes, physical_num_classes);
    assert_eq!(
        physical_classes_info
            .iter()
            .map(|class| class.0)
            .collect::<Vec<_>>(),
        [1, 2, 2, 2, 2, 3, 3]
    );
    assert_eq!(
        u16::from_le_bytes([physical_classes[6], physical_classes[7]]),
        7
    );
    assert_eq!(xi_query_side_effect_snapshot(&state, 1), before);
}

/// Drive XIQueryDevice (opcode 48) and return the RAW class-block
/// bytes (and `num_classes`) for one exact device selector.
fn query_device_class_block(
    state: &mut ServerState,
    peer: &mut UnixStream,
    target_id: u16,
) -> (Vec<u8>, u16) {
    let mut body = target_id.to_le_bytes().to_vec();
    body.extend_from_slice(&[0, 0]); // XIQueryDevice pad
    let wire = dispatch_xi_request_wire(state, peer, 1, 48, &body);
    assert_eq!(wire[0], 1, "XIQueryDevice reply");
    let reply_length = u32::from_le_bytes(wire[4..8].try_into().unwrap()) as usize;
    assert_eq!(
        wire.len(),
        32 + reply_length * 4,
        "reply length covers all records"
    );
    let num_devices = u16::from_le_bytes([wire[8], wire[9]]) as usize;
    let mut off = 32;
    for _ in 0..num_devices {
        let id = u16::from_le_bytes([wire[off], wire[off + 1]]);
        let num_classes = u16::from_le_bytes([wire[off + 6], wire[off + 7]]);
        let name_len = u16::from_le_bytes([wire[off + 8], wire[off + 9]]) as usize;
        let mut pos = off + 12 + name_len;
        while !pos.is_multiple_of(4) {
            pos += 1;
        }
        let classes_start = pos;
        for _ in 0..num_classes {
            let units = u16::from_le_bytes([wire[pos + 2], wire[pos + 3]]) as usize;
            pos += units * 4;
        }
        if id == target_id {
            return (wire[classes_start..pos].to_vec(), num_classes);
        }
        off = pos;
    }
    panic!("requested device not present in XIQueryDevice reply");
}

fn parse_xi2_class_headers(classes: &[u8], num_classes: u16) -> Vec<(u16, usize, usize)> {
    let mut parsed = Vec::with_capacity(usize::from(num_classes));
    let mut offset = 0;
    for _ in 0..num_classes {
        let class_type = u16::from_le_bytes([classes[offset], classes[offset + 1]]);
        let units = usize::from(u16::from_le_bytes([
            classes[offset + 2],
            classes[offset + 3],
        ]));
        assert!(units > 0, "XI2 class record has a nonzero length");
        parsed.push((class_type, units, offset));
        offset += units * 4;
    }
    assert_eq!(offset, classes.len(), "all bytes belong to class records");
    parsed
}

/// XI1 reports `None` for masters and XTEST devices, while physical
/// pointer devices retain their MOUSE atom and class descriptors.
#[test]
fn xi_xtest_classes_xi1_list_and_open_device_match_xtest_pointer() {
    // Kills reverting ids 2..5 to MOUSE/KEYBOARD atoms, or reusing the
    // physical pointer's 7-button / four-axis ListInputDevices classes.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let physical_info = crate::core_loop::DeviceInfo {
        source_id: crate::xinput::InputSourceId(u64::from(line!())),
        enabled: true,
        resume_key: None,
        capabilities: crate::xinput::InputCapabilities {
            keyboard: false,
            pointer: true,
            touch: false,
        },
        name: "Logitech USB mouse".into(),
        device_node: "/dev/input/event-mouse".into(),
        sysname: "event-mouse".into(),
        vendor_id: 0x046d,
        product_id: 0xc52f,
        is_touchpad: false,
        config: Default::default(),
    };
    let physical_pointer = state.xi_register_source(&physical_info)[0];
    let mouse_atom = state.atoms.id_for(crate::xinput::XI_ATOM_MOUSE).unwrap().0;
    let touchpad_info = crate::core_loop::DeviceInfo {
        source_id: crate::xinput::InputSourceId(u64::from(line!())),
        enabled: true,
        resume_key: None,
        capabilities: crate::xinput::InputCapabilities {
            keyboard: false,
            pointer: true,
            touch: false,
        },
        name: "SynPS/2 Synaptics TouchPad".into(),
        device_node: "/dev/input/event-touchpad".into(),
        sysname: "event-touchpad".into(),
        vendor_id: 0x06cb,
        product_id: 0x00bd,
        is_touchpad: true,
        config: Default::default(),
    };
    let physical_touchpad = state.xi_register_source(&touchpad_info)[0];
    let touchpad_atom = state
        .atoms
        .id_for(crate::xinput::XI_ATOM_TOUCHPAD)
        .unwrap()
        .0;
    let keyboard_info = crate::core_loop::DeviceInfo {
        source_id: crate::xinput::InputSourceId(u64::from(line!())),
        enabled: true,
        resume_key: None,
        capabilities: crate::xinput::InputCapabilities {
            keyboard: true,
            pointer: false,
            touch: false,
        },
        name: "USB keyboard".into(),
        device_node: "/dev/input/event-keyboard".into(),
        sysname: "event-keyboard".into(),
        vendor_id: 0x046d,
        product_id: 0xc31c,
        is_touchpad: false,
        config: Default::default(),
    };
    let physical_keyboard = state.xi_register_source(&keyboard_info)[0];
    let keyboard_atom = state
        .atoms
        .id_for(crate::xinput::XI_ATOM_KEYBOARD)
        .unwrap()
        .0;
    let before = xi_query_side_effect_snapshot(&state, 1);

    let wire = dispatch_xi_request_wire(&mut state, &mut peer, 1, 2, &[]);
    assert_eq!(wire[0], 1, "XListInputDevices reply");
    let reply_length = u32::from_le_bytes(wire[4..8].try_into().unwrap()) as usize;
    assert_eq!(
        wire.len(),
        32 + reply_length * 4,
        "reply length covers records"
    );
    let count = usize::from(wire[8]);
    let mut device_records = Vec::with_capacity(count);
    let mut offset = 32;
    for _ in 0..count {
        let type_atom = u32::from_le_bytes(wire[offset..offset + 4].try_into().unwrap());
        let id = u16::from(wire[offset + 4]);
        let class_count = wire[offset + 5];
        device_records.push((id, type_atom, class_count));
        offset += 8;
    }
    assert_eq!(
        device_records
            .iter()
            .filter(|(id, _, _)| (2..=5).contains(id))
            .map(|(id, atom, _)| (*id, *atom))
            .collect::<Vec<_>>(),
        [(2, 0), (3, 0), (4, 0), (5, 0)],
        "master and XTEST types are None"
    );
    assert_eq!(
        device_records
            .iter()
            .find(|(id, _, _)| *id == physical_pointer)
            .unwrap()
            .1,
        mouse_atom,
        "physical pointer type remains MOUSE"
    );
    assert_eq!(
        device_records
            .iter()
            .find(|(id, _, _)| *id == physical_touchpad)
            .unwrap()
            .1,
        touchpad_atom,
        "physical touchpad type remains TOUCHPAD"
    );
    assert_eq!(
        device_records
            .iter()
            .find(|(id, _, _)| *id == physical_keyboard)
            .unwrap()
            .1,
        keyboard_atom,
        "physical keyboard type remains KEYBOARD"
    );

    let mut xtest_pointer_classes = Vec::new();
    let mut physical_pointer_classes = Vec::new();
    for (device_id, _, class_count) in &device_records {
        for _ in 0..*class_count {
            let class_type = wire[offset];
            let class_len = usize::from(wire[offset + 1]);
            assert!(class_len >= 4, "XI1 class descriptor has a valid length");
            if *device_id == crate::xinput::DEVICEID_XTEST_POINTER {
                xtest_pointer_classes.push(wire[offset..offset + class_len].to_vec());
            } else if *device_id == physical_pointer {
                physical_pointer_classes.push(wire[offset..offset + class_len].to_vec());
            }
            assert!(matches!(class_type, 0..=3 | 5..=6));
            offset += class_len;
        }
    }
    assert_eq!(
        xtest_pointer_classes
            .iter()
            .map(|class| class[0])
            .collect::<Vec<_>>(),
        [1, 2],
        "XTEST XI1 listing has ButtonClass and ValuatorClass"
    );
    assert_eq!(
        u16::from_le_bytes([xtest_pointer_classes[0][2], xtest_pointer_classes[0][3]]),
        10
    );
    assert_eq!(xtest_pointer_classes[1][2], 2, "XTEST has two valuators");
    assert_eq!(
        xtest_pointer_classes[1][3], 0,
        "XTEST valuators are relative"
    );
    for axis in 0..2 {
        let axis_start = 8 + axis * 12;
        assert_eq!(
            i32::from_le_bytes(
                xtest_pointer_classes[1][axis_start + 4..axis_start + 8]
                    .try_into()
                    .unwrap()
            ),
            -1
        );
        assert_eq!(
            i32::from_le_bytes(
                xtest_pointer_classes[1][axis_start + 8..axis_start + 12]
                    .try_into()
                    .unwrap()
            ),
            -1
        );
    }
    assert_eq!(
        physical_pointer_classes
            .iter()
            .map(|class| class[0])
            .collect::<Vec<_>>(),
        [1, 2],
        "physical XI1 pointer still has its existing classes"
    );
    assert_eq!(
        u16::from_le_bytes([
            physical_pointer_classes[0][2],
            physical_pointer_classes[0][3]
        ]),
        7
    );
    assert_eq!(physical_pointer_classes[1][2], 4);

    // XOpenDevice exposes class/event-base tags sourced from the same
    // button and valuator classes (`Xi/opendev.c:120-155`).
    let opened = dispatch_xi_request_wire(&mut state, &mut peer, 2, 3, &[4, 0, 0, 0]);
    assert_eq!(opened.len(), 40, "four class tags follow the reply header");
    assert_eq!(opened[0], 1, "XOpenDevice reply");
    assert_eq!(opened[8], 4, "button, valuator, feedback, and other");
    assert_eq!(&opened[32..40], &[1, 69, 2, 71, 3, 0, 6, 76]);
    assert_eq!(xi_query_side_effect_snapshot(&state, 1), before);
}

#[test]
fn xi2_device_changed_bootstrap_matches_initial_master_query_classes() {
    // Before: this asserted seven physical classes and two scroll classes
    // on a fresh master. Xorg's CorePointerProc creates 10 buttons and two
    // relative valuators with no scroll classes (dix/devices.c:655-700;
    // InitCoreDevices calls it at :724-730).
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut select_body = Vec::new();
    select_body.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    select_body.extend_from_slice(&1u16.to_le_bytes());
    select_body.extend_from_slice(&[0; 2]);
    select_body.extend_from_slice(&0u16.to_le_bytes()); // XIAllDevices
    select_body.extend_from_slice(&1u16.to_le_bytes());
    select_body.extend_from_slice(&XI2_DEVICE_CHANGED_MASK.to_le_bytes());
    let wire = dispatch_xi_request_wire(&mut state, &mut peer, 1, 46, &select_body);
    // Locate the XI_DeviceChanged event (opcode 35, evtype 1).
    let off = (0..wire.len().saturating_sub(32))
        .find(|&i| wire[i] == 35 && u16::from_le_bytes([wire[i + 8], wire[i + 9]]) == 1)
        .expect("XI_DeviceChanged event in wire");
    assert_eq!(wire[off + 1], XI2_MAJOR_OPCODE, "major opcode = XI2 ext");
    let num_classes = u16::from_le_bytes([wire[off + 16], wire[off + 17]]);
    assert_eq!(num_classes, 3, "initial CorePointerProc classes");
    let event_class_len =
        u32::from_le_bytes(wire[off + 4..off + 8].try_into().unwrap()) as usize * 4;
    let event_classes = &wire[off + 32..off + 32 + event_class_len];
    let (query_classes, query_num_classes) = query_device_class_block(&mut state, &mut peer, 2);
    assert_eq!(query_num_classes, 3);
    assert_eq!(event_classes, query_classes);
    let class_headers = parse_xi2_class_headers(event_classes, num_classes);
    assert_eq!(
        class_headers
            .iter()
            .map(|class| class.0)
            .collect::<Vec<_>>(),
        [1, 2, 2]
    );
    assert_eq!(
        u16::from_le_bytes([event_classes[6], event_classes[7]]),
        10,
        "CorePointerProc button class has ten buttons"
    );
    assert!(
        class_headers.iter().all(|class| class.0 != 3),
        "the fresh master has no scroll class or absent valuator reference"
    );
}

#[test]
fn legacy_device_changed_fanout_emits_for_selected_xtest_pointer() {
    // Kills routing XTEST 4 through the generic physical pointer encoder;
    // Xorg `CorePointerProc` defines 10 buttons and two relative axes.
    use crate::{
        core_loop::fanout::emit_xi2_device_changed_slave_pointer, xinput::DEVICEID_XTEST_POINTER,
    };

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let xtest_pointer = state
        .xi_devices
        .device(DEVICEID_XTEST_POINTER)
        .expect("XTEST pointer");
    assert_eq!(
        xtest_pointer.name,
        crate::xinput::registry::NAME_XTEST_POINTER
    );
    assert!(
        xtest_pointer
            .properties
            .contains_key(&state.xtest_device_atom)
    );
    let before = xi_query_side_effect_snapshot(&state, 1);
    // Install the real XISelectEvents mask through the core dispatcher.
    let mut select_body = Vec::new();
    select_body.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    select_body.extend_from_slice(&1u16.to_le_bytes()); // num_masks
    select_body.extend_from_slice(&0u16.to_le_bytes()); // pad
    select_body.extend_from_slice(&DEVICEID_XTEST_POINTER.to_le_bytes());
    select_body.extend_from_slice(&1u16.to_le_bytes()); // mask_len
    select_body.extend_from_slice(&XI2_DEVICE_CHANGED_MASK.to_le_bytes());
    assert!(dispatch_xi_request_wire(&mut state, &mut peer, 1, 46, &select_body).is_empty());
    assert!(
        state.clients[&1]
            .xi2_masks
            .contains_key(&(ROOT_WINDOW, DEVICEID_XTEST_POINTER))
    );

    // This helper emits only the bootstrapped XTEST pointer event; the
    // physical source registry owns all dynamically allocated facets.
    let dropped = emit_xi2_device_changed_slave_pointer(&mut state, 137);
    assert!(dropped.is_empty());
    let wire = read_all_available(&mut peer);
    let off = (0..wire.len().saturating_sub(32))
        .find(|&i| wire[i] == 35 && u16::from_le_bytes([wire[i + 8], wire[i + 9]]) == 1)
        .expect("legacy XI_DeviceChanged fanout");
    assert_eq!(wire[off + 10], 4, "deviceid = XTEST pointer");
    assert_eq!(wire[off + 18], 4, "sourceid = the XTEST pointer itself");
    assert_eq!(wire[off + 20], 2, "reason = XIDeviceChange");
    assert_eq!(wire[off + 16], 3, "Button + two Valuator classes");

    // A second helper call exercises the same selected-device fanout.
    let dropped = emit_xi2_device_changed_slave_pointer(&mut state, 137);
    assert!(dropped.is_empty());
    let wire = read_all_available(&mut peer);
    let off = (0..wire.len().saturating_sub(32))
        .find(|&i| wire[i] == 35 && u16::from_le_bytes([wire[i + 8], wire[i + 9]]) == 1)
        .expect("second legacy XI_DeviceChanged fanout");
    assert_eq!(wire[off + 10], 4, "deviceid = XTEST pointer");

    // Clear the selection through XISelectEvents and confirm no request
    // or event state remains after this scenario.
    let mut unselect_body = Vec::new();
    unselect_body.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    unselect_body.extend_from_slice(&1u16.to_le_bytes()); // num_masks
    unselect_body.extend_from_slice(&0u16.to_le_bytes()); // pad
    unselect_body.extend_from_slice(&DEVICEID_XTEST_POINTER.to_le_bytes());
    unselect_body.extend_from_slice(&0u16.to_le_bytes()); // mask_len = remove
    assert!(dispatch_xi_request_wire(&mut state, &mut peer, 4, 46, &unselect_body).is_empty());
    assert!(
        !state.clients[&1]
            .xi2_masks
            .contains_key(&(ROOT_WINDOW, DEVICEID_XTEST_POINTER))
    );
    assert_eq!(xi_query_side_effect_snapshot(&state, 1), before);
}

#[test]
fn xtest_device_changed_is_not_selected_by_all_master_devices() {
    use crate::core_loop::fanout::emit_xi2_device_changed_slave_pointer;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    state
        .clients
        .get_mut(&1)
        .unwrap()
        .xi2_masks
        .insert((ROOT_WINDOW, 1), u64::from(XI2_DEVICE_CHANGED_MASK));

    let dropped = emit_xi2_device_changed_slave_pointer(&mut state, 137);
    assert!(dropped.is_empty());
    assert!(
        read_all_available(&mut peer).is_empty(),
        "XIAllMasterDevices must not receive DeviceChanged for XTEST slave 4",
    );
    assert_eq!(
        state
            .xi_devices
            .device(crate::xinput::DEVICEID_XTEST_POINTER)
            .map(|d| d.id),
        Some(crate::xinput::DEVICEID_XTEST_POINTER),
        "only the static XTEST pointer is involved",
    );
    assert_eq!(
        state.xi_devices.source_ids().len(),
        0,
        "the event must not pretend a physical source is device 4",
    );
}

#[test]
fn device_changed_is_noop_when_no_client_selected() {
    use crate::core_loop::fanout::emit_xi2_device_changed_slave_pointer;
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    // No xi2_masks selection → no event.
    let dropped = emit_xi2_device_changed_slave_pointer(&mut state, 137);
    assert!(dropped.is_empty());
    assert!(
        read_all_available(&mut peer).is_empty(),
        "no DeviceChanged when nothing selected"
    );
}

#[test]
fn device_changed_not_sent_for_selection_on_non_root_window() {
    // DeviceChanged is a root-window hierarchy event. A client that
    // selected it on some unrelated (non-root) window must NOT get it
    // — otherwise the fanout spuriously delivers to the wrong window.
    use crate::{
        core_loop::fanout::emit_xi2_device_changed_slave_pointer, xinput::DEVICEID_XTEST_POINTER,
    };
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let non_root = ResourceId(0x4000_0001);
    state.clients.get_mut(&1).unwrap().xi2_masks.insert(
        (non_root, DEVICEID_XTEST_POINTER),
        u64::from(XI2_DEVICE_CHANGED_MASK),
    );
    let dropped = emit_xi2_device_changed_slave_pointer(&mut state, 137);
    assert!(dropped.is_empty());
    assert!(
        read_all_available(&mut peer).is_empty(),
        "selection on a non-root window must not receive DeviceChanged"
    );
}

#[test]
fn xi_list_properties_wire() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    seed_one_prop(&mut state, 100, 8, vec![1]);
    seed_one_prop(&mut state, 200, 8, vec![2]);

    // body: XTEST pointer deviceid + pad.
    let body = [
        u8::try_from(crate::xinput::DEVICEID_XTEST_POINTER).unwrap(),
        0,
        0,
        0,
    ];
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        xi2_header(56),
        &body,
    )
    .unwrap();
    let wire = read_all_available(&mut peer);
    assert_eq!(
        wire.len(),
        32 + 16,
        "32 header + XTEST marker + Device Enabled + 2 client atoms"
    );
    assert_eq!(wire[0], 1, "X_Reply");
    assert_eq!(
        u32::from_le_bytes(wire[4..8].try_into().unwrap()),
        4,
        "length"
    );
    assert_eq!(u16::from_le_bytes([wire[8], wire[9]]), 4, "num_properties");
    let mut atoms = wire[32..]
        .chunks_exact(4)
        .map(|chunk| u32::from_le_bytes(chunk.try_into().unwrap()))
        .collect::<Vec<_>>();
    let mut expected = vec![
        state.xtest_device_atom.0,
        state.xi_device_enabled_atom.0,
        100,
        200,
    ];
    atoms.sort_unstable();
    expected.sort_unstable();
    assert_eq!(atoms, expected, "the virtual XTEST Device marker is listed");
}

#[test]
fn xi_list_properties_bad_device() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let body = [99u8, 0, 0, 0]; // deviceid 99 — not present
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(7),
        xi2_header(56),
        &body,
    )
    .unwrap();
    let wire = read_all_available(&mut peer);
    assert_eq!(wire.len(), 32, "error is 32 bytes");
    assert_eq!(wire[0], 0, "error packet");
    assert_eq!(
        wire[1], XI2_FIRST_ERROR,
        "BadDevice = XI2_FIRST_ERROR (157)"
    );
    // sequence at bytes 2-3.
    assert_eq!(u16::from_le_bytes([wire[2], wire[3]]), 7);
}

#[test]
fn xi_get_property_full_read_wire() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    seed_one_prop(&mut state, 100, 8, vec![0xAB]);

    // body: XTEST pointer deviceid, delete(1)=0, pad(1), property(4)=100,
    //       type(4)=0(Any), offset(4)=0, len(4)=100.
    let mut body = Vec::new();
    body.extend_from_slice(&crate::xinput::DEVICEID_XTEST_POINTER.to_le_bytes());
    body.push(0); // delete
    body.push(0); // pad
    body.extend_from_slice(&100u32.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&100u32.to_le_bytes());
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        xi2_header(59),
        &body,
    )
    .unwrap();
    let wire = read_all_available(&mut peer);
    assert_eq!(wire.len(), 32 + 4, "header + 1 byte padded to 4");
    assert_eq!(
        u32::from_le_bytes(wire[8..12].try_into().unwrap()),
        19,
        "type=INTEGER"
    );
    assert_eq!(
        u32::from_le_bytes(wire[12..16].try_into().unwrap()),
        0,
        "bytes_after"
    );
    assert_eq!(
        u32::from_le_bytes(wire[16..20].try_into().unwrap()),
        1,
        "num_items"
    );
    assert_eq!(wire[20], 8, "format");
    assert_eq!(wire[32], 0xAB, "value byte");
}

#[test]
fn xi_get_property_bad_delete_value() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    // XTEST pointer 4 exists; delete = 2 (illegal) → BadValue.
    // Pre-register atom 100 so the BadAtom guard (T3) doesn't fire
    // before the bad-delete check we're actually exercising.
    state.atoms.register_for_test(AtomId(100), "test-prop-100");
    let mut body = Vec::new();
    body.extend_from_slice(&crate::xinput::DEVICEID_XTEST_POINTER.to_le_bytes());
    body.push(2); // delete = 2 (illegal)
    body.push(0);
    body.extend_from_slice(&100u32.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&1u32.to_le_bytes());
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(3),
        xi2_header(59),
        &body,
    )
    .unwrap();
    let wire = read_all_available(&mut peer);
    assert_eq!(wire.len(), 32);
    assert_eq!(wire[0], 0, "error packet");
    assert_eq!(wire[1], 2, "BadValue");
}

#[test]
fn xi_change_then_get_roundtrip_wire() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    // Pre-register atom 100 so the T3 BadAtom guard accepts it.
    state.atoms.register_for_test(AtomId(100), "test-prop-100");

    // XIChangeProperty: deviceid(2)=4, mode(1)=Replace, format(1)=8,
    // property(4)=100, type(4)=INTEGER(19), num_items(4)=3, data=[7,8,9]+pad.
    let mut body = Vec::new();
    body.extend_from_slice(&crate::xinput::DEVICEID_XTEST_POINTER.to_le_bytes());
    body.push(0); // mode Replace
    body.push(8); // format
    body.extend_from_slice(&100u32.to_le_bytes());
    body.extend_from_slice(&19u32.to_le_bytes());
    body.extend_from_slice(&3u32.to_le_bytes());
    body.extend_from_slice(&[7, 8, 9, 0]); // 3 data bytes + 1 pad
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        xi2_header(57),
        &body,
    )
    .unwrap();
    // ChangeProperty has no reply.
    assert!(read_all_available(&mut peer).is_empty());

    // GetProperty back.
    let mut gbody = Vec::new();
    gbody.extend_from_slice(&crate::xinput::DEVICEID_XTEST_POINTER.to_le_bytes());
    gbody.push(0);
    gbody.push(0);
    gbody.extend_from_slice(&100u32.to_le_bytes());
    gbody.extend_from_slice(&0u32.to_le_bytes());
    gbody.extend_from_slice(&0u32.to_le_bytes());
    gbody.extend_from_slice(&100u32.to_le_bytes());
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(2),
        xi2_header(59),
        &gbody,
    )
    .unwrap();
    let wire = read_all_available(&mut peer);
    assert_eq!(
        u32::from_le_bytes(wire[16..20].try_into().unwrap()),
        3,
        "num_items"
    );
    assert_eq!(&wire[32..35], &[7, 8, 9], "value round-trips");
}

#[test]
fn xi_change_property_bad_format_wire() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let mut body = Vec::new();
    body.extend_from_slice(&crate::xinput::DEVICEID_XTEST_POINTER.to_le_bytes());
    body.push(0); // mode
    body.push(7); // format = 7 (illegal)
    body.extend_from_slice(&100u32.to_le_bytes());
    body.extend_from_slice(&19u32.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(4),
        xi2_header(57),
        &body,
    )
    .unwrap();
    let wire = read_all_available(&mut peer);
    assert_eq!(wire.len(), 32);
    assert_eq!(wire[0], 0, "error packet");
    assert_eq!(wire[1], 2, "BadValue");
}

#[test]
fn xi_change_property_unknown_device_outranks_bad_format() {
    // Error-precedence: xserver ProcXIChangeProperty (xiproperty.c:1137)
    // does the device lookup before validating mode/format, so an
    // unknown device + illegal format must yield BadDevice, not BadValue.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let mut body = Vec::new();
    body.extend_from_slice(&99u16.to_le_bytes()); // deviceid 99 — absent
    body.push(0); // mode
    body.push(7); // format = 7 (illegal)
    body.extend_from_slice(&100u32.to_le_bytes());
    body.extend_from_slice(&19u32.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(5),
        xi2_header(57),
        &body,
    )
    .unwrap();
    let wire = read_all_available(&mut peer);
    assert_eq!(wire.len(), 32);
    assert_eq!(wire[0], 0, "error packet");
    assert_eq!(wire[1], XI2_FIRST_ERROR, "BadDevice outranks BadValue");
    assert_eq!(u16::from_le_bytes([wire[2], wire[3]]), 5, "sequence");
}

#[test]
fn xi_delete_property_wire() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    seed_one_prop(&mut state, 100, 8, vec![1]);

    // body: XTEST pointer deviceid, pad(2), property(4)=100.
    let mut body = Vec::new();
    body.extend_from_slice(&crate::xinput::DEVICEID_XTEST_POINTER.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes());
    body.extend_from_slice(&100u32.to_le_bytes());
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        xi2_header(58),
        &body,
    )
    .unwrap();
    // No reply for DeleteProperty.
    assert!(read_all_available(&mut peer).is_empty());
    let dev = state
        .xi_devices
        .device(crate::xinput::DEVICEID_XTEST_POINTER)
        .unwrap();
    assert!(
        !dev.properties.contains_key(&AtomId(100)),
        "property removed"
    );
}

/// GTK4 (gnome-text-editor, modern GTK apps) sets per-widget
/// cursors through XInput2's `XIChangeCursor` (opcode 42), not
/// core X11's `XDefineCursor`. Pre-fix the handler was a logging
/// no-op, so the I-beam cursor over a text widget never reached
/// the backend and the area kept showing the default arrow.
/// Treat it as `XDefineCursor` (per-device cursors aren't routed
/// yet; a single effective cursor on the window matches what
/// `AllMasterDevices`-style calls produce in practice).
#[test]
fn xi_change_cursor_propagates_define_cursor_to_backend() {
    use crate::backend::recording::RecordedCall;

    const CLIENT_ID: u32 = 1;
    const WINDOW_XID: u32 = 0x0010_0030;
    const HOST_XID: u32 = 0x0040_0030;
    const CURSOR_XID: u32 = 0x0090_0001;
    const CURSOR_HOST_XID: u32 = 0x00a0_0001;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, CLIENT_ID);
    let mut backend = RecordingBackend::new();

    state.resources.create_window(
        yserver_protocol::x11::ClientId(CLIENT_ID),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(WINDOW_XID),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 200,
            height: 80,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    {
        let w = state
            .resources
            .window_mut(ResourceId(WINDOW_XID))
            .expect("window installed");
        w.host_xid = Some(crate::backend::WindowHandle::from_raw_for_test(HOST_XID));
    }
    state
        .resources
        .create_cursor(ClientId(CLIENT_ID), ResourceId(CURSOR_XID));
    state.resources.set_cursor_host_xid(
        ResourceId(CURSOR_XID),
        crate::backend::CursorHandle::from_raw_panicking(CURSOR_HOST_XID),
    );

    // XIChangeCursor body: window(4), cursor(4), deviceid(2), pad(2).
    let mut body = Vec::with_capacity(12);
    body.extend_from_slice(&WINDOW_XID.to_le_bytes());
    body.extend_from_slice(&CURSOR_XID.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes());
    body.extend_from_slice(&[0u8; 2]);

    let header = yserver_protocol::x11::RequestHeader {
        opcode: 131, // doesn't matter — dispatcher is bypassed
        data: 42,    // XIChangeCursor minor
        // XIChangeCursor is Fixed(4): 4 header + window(4) + cursor(4)
        // + deviceid(2) + pad(2) = 16 bytes = 4 units. (Was 3 — pre-
        // dated the length gate, now BadLengths before the handler.)
        length_units: 4,
    };
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(CLIENT_ID),
        SequenceNumber(1),
        header,
        &body,
    )
    .expect("handle_xi2_request");

    let define_cursor_calls: Vec<_> = backend
        .calls()
        .into_iter()
        .filter(|c| matches!(c, RecordedCall::DefineCursor { .. }))
        .collect();
    assert_eq!(
        define_cursor_calls,
        vec![RecordedCall::DefineCursor {
            host_window_xid: HOST_XID,
            cursor_host_xid: CURSOR_HOST_XID,
        }],
        "XIChangeCursor must propagate to `backend.define_cursor` \
             so GTK4's per-widget cursor (I-beam on text areas, etc.) \
             becomes effective. Pre-fix the handler was a logging \
             no-op — gnome-text-editor and other GTK4 apps kept the \
             default arrow over text widgets.",
    );

    // cursor = None (xid 0) clears the per-window cursor.
    let mut body_none = Vec::with_capacity(12);
    body_none.extend_from_slice(&WINDOW_XID.to_le_bytes());
    body_none.extend_from_slice(&0u32.to_le_bytes());
    body_none.extend_from_slice(&0u16.to_le_bytes());
    body_none.extend_from_slice(&[0u8; 2]);

    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(CLIENT_ID),
        SequenceNumber(2),
        header,
        &body_none,
    )
    .expect("handle_xi2_request none");

    let define_cursor_calls: Vec<_> = backend
        .calls()
        .into_iter()
        .filter(|c| matches!(c, RecordedCall::DefineCursor { .. }))
        .collect();
    assert_eq!(
        define_cursor_calls,
        vec![
            RecordedCall::DefineCursor {
                host_window_xid: HOST_XID,
                cursor_host_xid: CURSOR_HOST_XID,
            },
            RecordedCall::DefineCursor {
                host_window_xid: HOST_XID,
                cursor_host_xid: 0,
            },
        ],
        "XIChangeCursor with cursor=None (xid 0) must invoke \
             `backend.define_cursor(window, 0)` — same X11 None \
             semantics as the CWA cursor path.",
    );
}
