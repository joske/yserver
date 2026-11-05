use super::*;
#[cfg(test)]
use crate::core_loop::process_request::vidmode::current_vidmode_mode_line;
use crate::core_loop::process_request::vidmode::{
    current_vidmode_output, vidmode_monitor_identity,
};

#[test]
fn xf86vidmode_v2_mode_line_round_trips_current_randr_timing() {
    use crate::randr::{ModeTiming, RandrOutput, RandrState};
    use yserver_protocol::x11::xf86vidmode as x11vm;

    let mut state = ServerState::new();
    state.randr = RandrState::from_outputs(
        1,
        vec![RandrOutput {
            name: "DP-1".to_string(),
            output_id: 1,
            crtc_id: 2,
            mode_id: 3,
            connected: true,
            x: 0,
            y: 0,
            width: 2560,
            height: 1440,
            vrefresh: 60,
            timing: Some(ModeTiming {
                clock_khz: 241_500,
                hsync_start: 2608,
                hsync_end: 2640,
                htotal: 2720,
                vsync_start: 1443,
                vsync_end: 1448,
                vtotal: 1481,
                mode_flags: 5,
            }),
            mm_width: 600,
            mm_height: 340,
            mode_ids: vec![3],
            num_preferred: 1,
            pending_transform: Default::default(),
            current_transform: Default::default(),
            rotation: crate::randr::RR_ROTATE_0,
        }],
    );
    let expected = current_vidmode_mode_line(&state).expect("active RandR mode");
    assert_eq!(
        expected,
        x11vm::ModeLine {
            dot_clock: 241_500,
            hdisplay: 2560,
            hsync_start: 2608,
            hsync_end: 2640,
            htotal: 2720,
            hskew: 0,
            vdisplay: 1440,
            vsync_start: 1443,
            vsync_end: 1448,
            vtotal: 1481,
            flags: 5,
        }
    );
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();

    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode: crate::nested::XF86VIDMODE_MAJOR_OPCODE,
            data: x11vm::QUERY_VERSION,
            length_units: 1,
        },
        &[],
        None,
    )
    .expect("QueryVersion");
    let version = read_all_or_buffered(&mut state, 1, &mut peer);
    assert_eq!(version.len(), 32);
    assert_eq!(&version[8..12], &[2, 0, 2, 0]);

    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(2),
        RequestHeader {
            opcode: crate::nested::XF86VIDMODE_MAJOR_OPCODE,
            data: x11vm::SET_CLIENT_VERSION,
            length_units: 2,
        },
        &[2, 0, 2, 0],
        None,
    )
    .expect("SetClientVersion");
    assert!(read_all_or_buffered(&mut state, 1, &mut peer).is_empty());
    assert_eq!(
        state.vidmode_client_versions.get(&ClientId(1)),
        Some(&(2, 2))
    );

    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(3),
        RequestHeader {
            opcode: crate::nested::XF86VIDMODE_MAJOR_OPCODE,
            data: x11vm::GET_MODE_LINE,
            length_units: 2,
        },
        &[0; 4],
        None,
    )
    .expect("GetModeLine");
    let mode = read_all_or_buffered(&mut state, 1, &mut peer);
    assert_eq!(mode.len(), 52);
    assert_eq!(u32::from_le_bytes(mode[4..8].try_into().unwrap()), 5);
    assert_eq!(
        u32::from_le_bytes(mode[8..12].try_into().unwrap()),
        expected.dot_clock
    );
    assert_eq!(
        u16::from_le_bytes(mode[12..14].try_into().unwrap()),
        expected.hdisplay
    );
    assert_eq!(
        u16::from_le_bytes(mode[18..20].try_into().unwrap()),
        expected.htotal
    );
    assert_eq!(
        u16::from_le_bytes(mode[22..24].try_into().unwrap()),
        expected.vdisplay
    );
    assert_eq!(
        u16::from_le_bytes(mode[28..30].try_into().unwrap()),
        expected.vtotal
    );
    assert_eq!(
        u32::from_le_bytes(mode[32..36].try_into().unwrap()),
        expected.flags
    );
    assert_eq!(&mode[36..52], &[0; 16]);
}

#[test]
fn xf86vidmode_defaults_to_legacy_reply_and_rejects_unknown_screen() {
    use yserver_protocol::x11::xf86vidmode as x11vm;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();

    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode: crate::nested::XF86VIDMODE_MAJOR_OPCODE,
            data: x11vm::GET_MODE_LINE,
            length_units: 2,
        },
        &[0; 4],
        None,
    )
    .expect("legacy GetModeLine");
    let legacy = read_all_or_buffered(&mut state, 1, &mut peer);
    assert_eq!(legacy.len(), 36);
    assert_eq!(u32::from_le_bytes(legacy[4..8].try_into().unwrap()), 1);

    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(2),
        RequestHeader {
            opcode: crate::nested::XF86VIDMODE_MAJOR_OPCODE,
            data: x11vm::GET_MODE_LINE,
            length_units: 2,
        },
        &[1, 0, 0, 0],
        None,
    )
    .expect("bad-screen GetModeLine");
    let error = read_all_or_buffered(&mut state, 1, &mut peer);
    assert_eq!(error.len(), 32);
    assert_eq!(error[0], 0);
    assert_eq!(error[1], x11::error::BAD_VALUE);
    assert_eq!(u32::from_le_bytes(error[4..8].try_into().unwrap()), 1);
    assert_eq!(
        u16::from_le_bytes(error[8..10].try_into().unwrap()),
        u16::from(x11vm::GET_MODE_LINE)
    );
    assert_eq!(error[10], crate::nested::XF86VIDMODE_MAJOR_OPCODE);
}

/// Drive one VidMode request and return whatever went back to the client.
fn dispatch_vidmode(
    state: &mut ServerState,
    peer: &mut UnixStream,
    sequence: u16,
    minor: u8,
    body: &[u8],
) -> Vec<u8> {
    let mut backend = RecordingBackend::new();
    dispatch_vidmode_with_backend(state, &mut backend, peer, sequence, minor, body)
}

fn dispatch_vidmode_with_backend(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    peer: &mut UnixStream,
    sequence: u16,
    minor: u8,
    body: &[u8],
) -> Vec<u8> {
    let length_units = u32::try_from(4 + body.len()).expect("test body") / 4;
    process_request(
        state,
        backend,
        ClientId(1),
        SequenceNumber(sequence),
        RequestHeader {
            opcode: crate::nested::XF86VIDMODE_MAJOR_OPCODE,
            data: minor,
            length_units,
        },
        body,
        None,
    )
    .expect("vidmode dispatch");
    read_all_or_buffered(state, 1, peer)
}

fn assert_vidmode_error(reply: &[u8], code: u8, minor: u8) {
    assert_eq!(reply.len(), 32, "X errors are always 32 bytes");
    assert_eq!(reply[0], 0, "error, not a reply");
    assert_eq!(reply[1], code);
    assert_eq!(
        u16::from_le_bytes(reply[8..10].try_into().unwrap()),
        u16::from(minor)
    );
    assert_eq!(reply[10], crate::nested::XF86VIDMODE_MAJOR_OPCODE);
}

fn validate_mode_line_body(
    mode: yserver_protocol::x11::xf86vidmode::ModeLine,
    version_2: bool,
    byte_order: ClientByteOrder,
) -> Vec<u8> {
    fn put_u16(body: &mut [u8], offset: usize, value: u16, order: ClientByteOrder) {
        let bytes = match order {
            ClientByteOrder::LittleEndian => value.to_le_bytes(),
            ClientByteOrder::BigEndian => value.to_be_bytes(),
        };
        body[offset..offset + 2].copy_from_slice(&bytes);
    }

    fn put_u32(body: &mut [u8], offset: usize, value: u32, order: ClientByteOrder) {
        let bytes = match order {
            ClientByteOrder::LittleEndian => value.to_le_bytes(),
            ClientByteOrder::BigEndian => value.to_be_bytes(),
        };
        body[offset..offset + 4].copy_from_slice(&bytes);
    }

    let mut body = vec![0; if version_2 { 48 } else { 32 }];
    put_u32(&mut body, 4, mode.dot_clock, byte_order);
    put_u16(&mut body, 8, mode.hdisplay, byte_order);
    put_u16(&mut body, 10, mode.hsync_start, byte_order);
    put_u16(&mut body, 12, mode.hsync_end, byte_order);
    put_u16(&mut body, 14, mode.htotal, byte_order);
    if version_2 {
        put_u16(&mut body, 16, mode.hskew, byte_order);
        put_u16(&mut body, 18, mode.vdisplay, byte_order);
        put_u16(&mut body, 20, mode.vsync_start, byte_order);
        put_u16(&mut body, 22, mode.vsync_end, byte_order);
        put_u16(&mut body, 24, mode.vtotal, byte_order);
        put_u32(&mut body, 28, mode.flags, byte_order);
    } else {
        put_u16(&mut body, 16, mode.vdisplay, byte_order);
        put_u16(&mut body, 18, mode.vsync_start, byte_order);
        put_u16(&mut body, 20, mode.vsync_end, byte_order);
        put_u16(&mut body, 22, mode.vtotal, byte_order);
        put_u32(&mut body, 24, mode.flags, byte_order);
    }
    body
}

/// Known writes must use the same Xorg non-local branch advertised by
/// GetPermissions; truly unknown minors remain BadRequest.
#[test]
fn xf86vidmode_rejects_writes_with_client_not_local() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    state.clients.get_mut(&1).expect("test client").is_local = false;

    for (seq, minor) in [2u8, 3, 5, 7, 8, 10, 12, 15, 18].into_iter().enumerate() {
        #[allow(clippy::cast_possible_truncation)]
        let seq = seq as u16 + 1;
        // The locality gate runs before per-request length parsing in Xorg.
        let reply = dispatch_vidmode(&mut state, &mut peer, seq, minor, &[]);
        assert_vidmode_error(
            &reply,
            crate::nested::XF86VIDMODE_FIRST_ERROR
                + yserver_protocol::x11::xf86vidmode::CLIENT_NOT_LOCAL,
            minor,
        );
    }

    for (seq, minor) in [21u8, 255].into_iter().enumerate() {
        #[allow(clippy::cast_possible_truncation)]
        let seq = seq as u16 + 20;
        let reply = dispatch_vidmode(&mut state, &mut peer, seq, minor, &[]);
        assert_vidmode_error(&reply, x11::error::BAD_REQUEST, minor);
    }
}

#[test]
fn xf86vidmode_permissions_follow_client_locality() {
    use yserver_protocol::x11::xf86vidmode as x11vm;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);

    let local = dispatch_vidmode(&mut state, &mut peer, 1, x11vm::GET_PERMISSIONS, &[0; 4]);
    assert_eq!(local[0], 1);
    assert_eq!(
        u32::from_le_bytes(local[8..12].try_into().unwrap()),
        x11vm::PERMISSION_READ | 2,
        "local clients receive XF86VM_WRITE_PERMISSION"
    );

    state.clients.get_mut(&1).expect("test client").is_local = false;
    let remote = dispatch_vidmode(&mut state, &mut peer, 2, x11vm::GET_PERMISSIONS, &[0; 4]);
    assert_eq!(remote[0], 1);
    assert_eq!(
        u32::from_le_bytes(remote[8..12].try_into().unwrap()),
        x11vm::PERMISSION_READ,
        "remote clients remain read-only"
    );
}

#[test]
fn xf86vidmode_rejects_malformed_bodies_with_bad_length() {
    use yserver_protocol::x11::xf86vidmode as x11vm;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    state.clients.get_mut(&1).expect("test client").is_local = false;

    // QueryVersion takes no body at all.
    let reply = dispatch_vidmode(&mut state, &mut peer, 1, x11vm::QUERY_VERSION, &[0; 4]);
    assert_vidmode_error(&reply, x11::error::BAD_LENGTH, x11vm::QUERY_VERSION);

    // The screen-scoped requests need exactly `screen + pad`.
    for (seq, minor) in [
        x11vm::GET_MODE_LINE,
        x11vm::GET_MONITOR,
        x11vm::GET_ALL_MODE_LINES,
        x11vm::GET_VIEW_PORT,
        x11vm::GET_DOT_CLOCKS,
        x11vm::GET_GAMMA_RAMP_SIZE,
        x11vm::GET_PERMISSIONS,
    ]
    .into_iter()
    .enumerate()
    {
        #[allow(clippy::cast_possible_truncation)]
        let seq = seq as u16 + 2;
        let reply = dispatch_vidmode(&mut state, &mut peer, seq, minor, &[0; 8]);
        assert_vidmode_error(&reply, x11::error::BAD_LENGTH, minor);
    }

    // The legacy gamma scalar request is padded to 32 bytes total.
    let reply = dispatch_vidmode(&mut state, &mut peer, 12, x11vm::GET_GAMMA, &[0; 4]);
    assert_vidmode_error(&reply, x11::error::BAD_LENGTH, x11vm::GET_GAMMA);

    // GetGammaRamp carries exactly screen + requested size.
    let reply = dispatch_vidmode(&mut state, &mut peer, 13, x11vm::GET_GAMMA_RAMP, &[0; 8]);
    assert_vidmode_error(&reply, x11::error::BAD_LENGTH, x11vm::GET_GAMMA_RAMP);

    // A legacy ValidateModeLine body is 32 bytes after the X header.
    let reply = dispatch_vidmode(
        &mut state,
        &mut peer,
        14,
        x11vm::VALIDATE_MODE_LINE,
        &[0; 4],
    );
    assert_vidmode_error(&reply, x11::error::BAD_LENGTH, x11vm::VALIDATE_MODE_LINE);

    // SetClientVersion needs exactly `major + minor`.
    let reply = dispatch_vidmode(
        &mut state,
        &mut peer,
        15,
        x11vm::SET_CLIENT_VERSION,
        &[2, 0],
    );
    assert_vidmode_error(&reply, x11::error::BAD_LENGTH, x11vm::SET_CLIENT_VERSION);
    assert!(
        !state.vidmode_client_versions.contains_key(&ClientId(1)),
        "a rejected SetClientVersion must not record a version"
    );
}

#[test]
fn xf86vidmode_gamma_ramp_is_the_vidmode_table_not_the_randr_crtc() {
    use yserver_protocol::x11::xf86vidmode as x11vm;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let crtc = current_vidmode_output(&state)
        .expect("active output")
        .crtc_id;
    let mut backend = RecordingBackend::new();

    let mut red = vec![0u16; 256];
    let mut green = vec![0u16; 256];
    let mut blue = vec![0u16; 256];
    red[0] = 0x1111;
    green[0] = 0x2222;
    blue[0] = 0x3333;
    red[255] = 0xaaaa;
    green[255] = 0xbbbb;
    blue[255] = 0xcccc;
    backend
        .set_crtc_gamma(crtc, &red, &green, &blue)
        .expect("seed selected CRTC gamma");

    let perms = dispatch_vidmode_with_backend(
        &mut state,
        &mut backend,
        &mut peer,
        1,
        x11vm::GET_PERMISSIONS,
        &[0; 4],
    );
    assert_eq!(perms.len(), 32);
    assert_eq!(perms[0], 1, "reply, not an error");
    assert_eq!(
        u32::from_le_bytes(perms[8..12].try_into().unwrap()),
        x11vm::PERMISSION_READ | x11vm::PERMISSION_WRITE,
        "local clients receive Xorg's WRITE permission"
    );

    let size = dispatch_vidmode_with_backend(
        &mut state,
        &mut backend,
        &mut peer,
        2,
        x11vm::GET_GAMMA_RAMP_SIZE,
        &[0; 4],
    );
    assert_eq!(size.len(), 32);
    assert_eq!(size[0], 1);
    assert_eq!(
        u16::from_le_bytes(size[8..10].try_into().unwrap()),
        1024,
        "the table's size, not the CRTC's 256"
    );

    let gamma = dispatch_vidmode_with_backend(
        &mut state,
        &mut backend,
        &mut peer,
        3,
        x11vm::GET_GAMMA,
        &[0; 28],
    );
    assert_eq!(gamma.len(), 32);
    for offset in [8usize, 12, 16] {
        assert_eq!(
            u32::from_le_bytes(gamma[offset..offset + 4].try_into().unwrap()),
            10_000,
            "GetGamma is the independent per-screen 1.0 scalar"
        );
    }

    let ramp = dispatch_vidmode_with_backend(
        &mut state,
        &mut backend,
        &mut peer,
        4,
        x11vm::GET_GAMMA_RAMP,
        &[0, 0, 0, 4], // screen=0, size=1024
    );
    assert_eq!(ramp.len(), 32 + 1024 * 3 * 2);
    assert_eq!(
        u32::from_le_bytes(ramp[4..8].try_into().unwrap()),
        1024 * 3 * 2 / 4
    );
    assert_eq!(u16::from_le_bytes(ramp[8..10].try_into().unwrap()), 1024);
    let entry = |at: usize| u16::from_le_bytes(ramp[at..at + 2].try_into().unwrap());
    // The identity table widened (xf86GetGammaRamp), not the seeded
    // 0x1111/0x2222/0x3333 of the CRTC's RANDR ramp.
    assert_eq!((entry(32), entry(34), entry(32 + 1023 * 2)), (0, 64, 65535));
    assert_eq!(entry(32 + 2048), 0, "green starts at 0");
    assert_eq!(entry(32 + 4096 + 1023 * 2), 65535, "blue ends at 65535");
    assert_eq!(
        backend.get_crtc_gamma(crtc).0[0],
        0x1111,
        "the RANDR ramp is untouched"
    );

    let wrong_size = dispatch_vidmode_with_backend(
        &mut state,
        &mut backend,
        &mut peer,
        5,
        x11vm::GET_GAMMA_RAMP,
        &[0, 0, 0, 1],
    );
    assert_vidmode_error(&wrong_size, x11::error::BAD_VALUE, x11vm::GET_GAMMA_RAMP);
}

#[test]
fn xf86vidmode_monitor_viewport_dotclocks_and_validation_are_readable() {
    use yserver_protocol::x11::xf86vidmode as x11vm;

    let mut state = ServerState::new();
    let current = current_vidmode_output(&state).expect("active output");
    let connector = current.connector.clone();
    let mut peer = install_client(&mut state, 1);

    let monitor = dispatch_vidmode(&mut state, &mut peer, 1, x11vm::GET_MONITOR, &[0; 4]);
    assert_eq!(monitor[0], 1);
    assert_eq!(monitor[8], 0, "no synthetic EDID vendor");
    assert_eq!(usize::from(monitor[9]), connector.len());
    assert_eq!(monitor[10], 1, "one active hsync range");
    assert_eq!(monitor[11], 1, "one active vsync range");
    assert_eq!(&monitor[40..40 + connector.len()], connector.as_bytes());

    let viewport = dispatch_vidmode(&mut state, &mut peer, 2, x11vm::GET_VIEW_PORT, &[0; 4]);
    assert_eq!(viewport.len(), 32);
    assert_eq!(&viewport[8..16], &[0; 8]);

    let clocks = dispatch_vidmode(&mut state, &mut peer, 3, x11vm::GET_DOT_CLOCKS, &[0; 4]);
    assert_eq!(clocks.len(), 32);
    assert_eq!(
        u32::from_le_bytes(clocks[8..12].try_into().unwrap()),
        x11vm::CLOCK_FLAG_PROGRAMMABLE
    );
    assert_eq!(u32::from_le_bytes(clocks[12..16].try_into().unwrap()), 0);
    assert_eq!(
        u32::from_le_bytes(clocks[16..20].try_into().unwrap()),
        x11vm::MAX_CLOCKS
    );

    let legacy_body = validate_mode_line_body(current.mode, false, ClientByteOrder::LittleEndian);
    let legacy_validate = dispatch_vidmode(
        &mut state,
        &mut peer,
        4,
        x11vm::VALIDATE_MODE_LINE,
        &legacy_body,
    );
    assert_eq!(legacy_validate.len(), 32);
    assert_eq!(
        u32::from_le_bytes(legacy_validate[8..12].try_into().unwrap()),
        x11vm::MODE_OK
    );

    let invalid_validate = dispatch_vidmode(
        &mut state,
        &mut peer,
        5,
        x11vm::VALIDATE_MODE_LINE,
        &[0; 32],
    );
    assert_eq!(
        u32::from_le_bytes(invalid_validate[8..12].try_into().unwrap()),
        x11vm::MODE_BAD
    );

    let mut inverted_mode = current.mode;
    inverted_mode.hsync_start = inverted_mode.hdisplay - 1;
    let inverted_body =
        validate_mode_line_body(inverted_mode, false, ClientByteOrder::LittleEndian);
    let inverted_validate = dispatch_vidmode(
        &mut state,
        &mut peer,
        6,
        x11vm::VALIDATE_MODE_LINE,
        &inverted_body,
    );
    assert_eq!(
        u32::from_le_bytes(inverted_validate[8..12].try_into().unwrap()),
        x11vm::MODE_BAD
    );

    dispatch_vidmode(
        &mut state,
        &mut peer,
        7,
        x11vm::SET_CLIENT_VERSION,
        &[2, 0, 2, 0],
    );
    let v2_body = validate_mode_line_body(current.mode, true, ClientByteOrder::LittleEndian);
    let v2_validate = dispatch_vidmode(
        &mut state,
        &mut peer,
        8,
        x11vm::VALIDATE_MODE_LINE,
        &v2_body,
    );
    assert_eq!(v2_validate.len(), 32);
    assert_eq!(
        u32::from_le_bytes(v2_validate[8..12].try_into().unwrap()),
        x11vm::MODE_OK
    );

    let mut other_mode = current.mode;
    other_mode.dot_clock += 1;
    let other_body = validate_mode_line_body(other_mode, true, ClientByteOrder::LittleEndian);
    let other_validate = dispatch_vidmode(
        &mut state,
        &mut peer,
        9,
        x11vm::VALIDATE_MODE_LINE,
        &other_body,
    );
    assert_eq!(
        u32::from_le_bytes(other_validate[8..12].try_into().unwrap()),
        x11vm::MODE_BAD
    );
}

#[test]
fn xf86vidmode_dotclocks_reports_programmable_clock_when_headless() {
    use yserver_protocol::x11::xf86vidmode as x11vm;

    let mut state = ServerState::new();
    state.randr.outputs.clear();
    let mut peer = install_client(&mut state, 1);

    // Xorg's modesetting driver advertises a programmable clock even
    // without an active output; the active dot clock belongs to
    // GetModeLine rather than the legacy fixed-clock table.
    let clocks = dispatch_vidmode(&mut state, &mut peer, 1, x11vm::GET_DOT_CLOCKS, &[0; 4]);
    assert_eq!(clocks.len(), 32);
    assert_eq!(
        u32::from_le_bytes(clocks[8..12].try_into().unwrap()),
        x11vm::CLOCK_FLAG_PROGRAMMABLE
    );
    assert_eq!(u32::from_le_bytes(clocks[12..16].try_into().unwrap()), 0);
    assert_eq!(
        u32::from_le_bytes(clocks[16..20].try_into().unwrap()),
        x11vm::MAX_CLOCKS
    );
}

#[test]
fn xf86vidmode_monitor_identity_decodes_edid_and_falls_back_to_connector() {
    let mut edid = vec![0u8; 128];
    // DEL manufacturer code: D=4, E=5, L=12.
    edid[8..10].copy_from_slice(&0x10ac_u16.to_be_bytes());
    edid[54..59].copy_from_slice(&[0, 0, 0, 0xfc, 0]);
    edid[59..72].copy_from_slice(b"U2723QE\n     ");
    assert_eq!(
        vidmode_monitor_identity(&edid, "DP-1"),
        (b"DEL".to_vec(), b"U2723QE".to_vec())
    );
    assert_eq!(
        vidmode_monitor_identity(&[], "Virtual-1"),
        (Vec::new(), b"Virtual-1".to_vec())
    );
}

#[test]
fn xf86vidmode_get_all_mode_lines_reports_only_the_active_mode() {
    use yserver_protocol::x11::xf86vidmode as x11vm;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);

    // Legacy client: 32-byte header + one 28-byte xXF86OldVidModeModeInfo.
    let legacy = dispatch_vidmode(&mut state, &mut peer, 1, x11vm::GET_ALL_MODE_LINES, &[0; 4]);
    assert_eq!(legacy.len(), 60);
    assert_eq!(u32::from_le_bytes(legacy[8..12].try_into().unwrap()), 1);

    dispatch_vidmode(
        &mut state,
        &mut peer,
        2,
        x11vm::SET_CLIENT_VERSION,
        &[2, 0, 2, 0],
    );

    // v2 client: 32-byte header + one 48-byte xXF86VidModeModeInfo.
    let v2 = dispatch_vidmode(&mut state, &mut peer, 3, x11vm::GET_ALL_MODE_LINES, &[0; 4]);
    assert_eq!(v2.len(), 80);
    assert_eq!(u32::from_le_bytes(v2[4..8].try_into().unwrap()), 12);
    assert_eq!(u32::from_le_bytes(v2[8..12].try_into().unwrap()), 1);
    // Same active mode GetModeLine reports.
    let expected = current_vidmode_mode_line(&state).expect("active mode");
    assert_eq!(
        u32::from_le_bytes(v2[32..36].try_into().unwrap()),
        expected.dot_clock
    );
    assert_eq!(
        u16::from_le_bytes(v2[42..44].try_into().unwrap()),
        expected.htotal
    );
}

#[test]
fn xf86vidmode_v2_mode_list_keeps_the_core_stream_aligned() {
    use yserver_protocol::x11::xf86vidmode as x11vm;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();

    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode: crate::nested::XF86VIDMODE_MAJOR_OPCODE,
            data: x11vm::SET_CLIENT_VERSION,
            length_units: 2,
        },
        &[2, 0, 2, 0],
        None,
    )
    .expect("SetClientVersion");
    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(2),
        RequestHeader {
            opcode: crate::nested::XF86VIDMODE_MAJOR_OPCODE,
            data: x11vm::GET_ALL_MODE_LINES,
            length_units: 2,
        },
        &[0; 4],
        None,
    )
    .expect("GetAllModeLines");
    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(3),
        RequestHeader {
            opcode: 43, // GetInputFocus
            data: 0,
            length_units: 1,
        },
        &[],
        None,
    )
    .expect("GetInputFocus");

    let stream = read_all_or_buffered(&mut state, 1, &mut peer);
    assert_eq!(
        stream.len(),
        80 + 32,
        "complete VidMode reply followed by core reply"
    );

    assert_eq!(stream[0], 1);
    assert_eq!(u16::from_le_bytes(stream[2..4].try_into().unwrap()), 2);
    assert_eq!(u32::from_le_bytes(stream[4..8].try_into().unwrap()), 12);
    assert_eq!(u32::from_le_bytes(stream[8..12].try_into().unwrap()), 1);

    let focus = &stream[80..];
    assert_eq!(focus[0], 1);
    assert_eq!(u16::from_le_bytes(focus[2..4].try_into().unwrap()), 3);
    assert_eq!(u32::from_le_bytes(focus[4..8].try_into().unwrap()), 0);
}

/// `client_reader` swaps ordinary request bodies before dispatch.
/// ValidateModeLine remains in original client order for its
/// version-dependent parser; replies and errors always use client order.
#[test]
fn xf86vidmode_big_endian_client_gets_a_byte_swapped_reply() {
    use yserver_protocol::x11::xf86vidmode as x11vm;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    state.clients.get_mut(&1).expect("test client").byte_order = ClientByteOrder::BigEndian;

    // Bodies arrive already in host order — as `swap_request_body` left
    // them — so this is still a little-endian `2.2`.
    dispatch_vidmode(
        &mut state,
        &mut peer,
        1,
        x11vm::SET_CLIENT_VERSION,
        &[2, 0, 2, 0],
    );
    assert_eq!(
        state.vidmode_client_versions.get(&ClientId(1)),
        Some(&(2, 2)),
        "the swapped body must parse as 2.2, not 512.512"
    );

    let expected = current_vidmode_mode_line(&state).expect("active mode");
    let reply = dispatch_vidmode(&mut state, &mut peer, 0x1234, x11vm::GET_MODE_LINE, &[0; 4]);
    assert_eq!(reply.len(), 52);
    assert_eq!(&reply[2..4], &0x1234_u16.to_be_bytes());
    assert_eq!(&reply[4..8], &5_u32.to_be_bytes());
    assert_eq!(&reply[8..12], &expected.dot_clock.to_be_bytes());
    assert_eq!(&reply[18..20], &expected.htotal.to_be_bytes());
    assert_eq!(&reply[28..30], &expected.vtotal.to_be_bytes());

    // Errors follow the client's byte order too. `screen = 1` is still
    // spelled little-endian in the already-swapped body.
    let error = dispatch_vidmode(
        &mut state,
        &mut peer,
        5,
        x11vm::GET_MODE_LINE,
        &[1, 0, 0, 0],
    );
    assert_eq!(error[0], 0);
    assert_eq!(error[1], x11::error::BAD_VALUE);
    assert_eq!(&error[4..8], &1_u32.to_be_bytes());

    let validate = validate_mode_line_body(expected, true, ClientByteOrder::BigEndian);
    let validated = dispatch_vidmode(
        &mut state,
        &mut peer,
        6,
        x11vm::VALIDATE_MODE_LINE,
        &validate,
    );
    assert_eq!(validated.len(), 32);
    assert_eq!(&validated[2..4], &6u16.to_be_bytes());
    assert_eq!(&validated[8..12], &x11vm::MODE_OK.to_be_bytes());
}

/// The VidMode mode line and RANDR's `ModeInfo` describe the same
/// hardware mode; they must agree on blanking, and their pixel clocks
/// must agree once RANDR's Hz is reduced to VidMode's kHz.
#[test]
fn xf86vidmode_mode_line_agrees_with_randr_mode_info() {
    let state = ServerState::new();
    let mode = current_vidmode_mode_line(&state).expect("active mode");
    let resources = state.randr.screen_resources_current();
    let info = resources
        .modes
        .iter()
        .find(|m| m.width == mode.hdisplay && m.height == mode.vdisplay)
        .expect("matching RANDR mode");

    assert_eq!(mode.htotal, info.htotal);
    assert_eq!(mode.vtotal, info.vtotal);
    assert_eq!(mode.hsync_start, info.hsync_start);
    assert_eq!(mode.hsync_end, info.hsync_end);
    assert_eq!(mode.vsync_start, info.vsync_start);
    assert_eq!(mode.vsync_end, info.vsync_end);
    assert_eq!(mode.flags, info.mode_flags);
    assert_eq!(mode.dot_clock, (info.dot_clock + 500) / 1000);
}

/// A `SetGamma` body: screen 0 and the three values times 10000.
fn set_gamma_body(red: u32, green: u32, blue: u32) -> Vec<u8> {
    let mut body = vec![0u8; 28];
    for (at, value) in [(4, red), (8, green), (12, blue)] {
        body[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }
    body
}

/// GetGamma's three values.
fn get_gamma(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    peer: &mut UnixStream,
) -> [u32; 3] {
    let reply = dispatch_vidmode_with_backend(
        state,
        backend,
        peer,
        90,
        yserver_protocol::x11::xf86vidmode::GET_GAMMA,
        &[0; 28],
    );
    assert_eq!(reply[0], 1, "GetGamma replies");
    [8usize, 12, 16].map(|at| u32::from_le_bytes(reply[at..at + 4].try_into().unwrap()))
}

/// GetGammaRamp's three channels, for a 1024-entry ramp.
fn get_gamma_ramp(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    peer: &mut UnixStream,
) -> (Vec<u16>, Vec<u16>, Vec<u16>) {
    let reply = dispatch_vidmode_with_backend(
        state,
        backend,
        peer,
        91,
        yserver_protocol::x11::xf86vidmode::GET_GAMMA_RAMP,
        &[0, 0, 0, 4],
    );
    assert_eq!(reply[0], 1, "GetGammaRamp replies");
    let channel = |index: usize| -> Vec<u16> {
        let start = 32 + index * 2048;
        reply[start..start + 2048]
            .chunks_exact(2)
            .map(|w| u16::from_le_bytes([w[0], w[1]]))
            .collect()
    };
    (channel(0), channel(1), channel(2))
}

/// A local client's SetGamma stores the values GetGamma reads back and
/// builds the screen's table with Xorg's `ComputeGamma` (values printed by
/// Xorg's own function), which the backend gets to load under every
/// CRTC's RANDR ramp; the RANDR ramp itself is untouched.
#[test]
fn xf86vidmode_local_set_gamma_builds_the_vidmode_table() {
    use crate::backend::gamma::{VidModeGammaTable, identity_ramp};
    use yserver_protocol::x11::xf86vidmode as x11vm;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let crtc = current_vidmode_output(&state)
        .expect("active output")
        .crtc_id;
    let mut backend = RecordingBackend::new();

    let reply = dispatch_vidmode_with_backend(
        &mut state,
        &mut backend,
        &mut peer,
        1,
        x11vm::SET_GAMMA,
        &set_gamma_body(20_000, 10_000, 5_000),
    );
    assert!(reply.is_empty(), "SetGamma has no reply: {reply:?}");
    assert_eq!(
        get_gamma(&mut state, &mut backend, &mut peer),
        [20_000, 10_000, 5_000]
    );
    let table = backend
        .vidmode_gamma
        .clone()
        .expect("the backend got the table");
    assert_eq!(table, VidModeGammaTable::from_gamma([2.0, 1.0, 0.5]));
    assert_eq!((table.red[1], table.green[1], table.blue[1]), (32, 1, 0));
    assert_eq!(
        (table.red[512], table.green[512], table.blue[512]),
        (724, 512, 256)
    );
    let ramp = get_gamma_ramp(&mut state, &mut backend, &mut peer);
    assert_eq!(
        (ramp.0[1], ramp.1[1], ramp.2[1]),
        (32 << 6 | 32 >> 4, 64, 0),
        "GetGammaRamp reads the table back"
    );
    let identity = identity_ramp(256);
    assert_eq!(
        backend.get_crtc_gamma(crtc),
        (identity.clone(), identity.clone(), identity),
        "the CRTC's RANDR ramp is untouched"
    );
}

/// Xorg's CMapChangeGamma takes each value in [0.1, 10] and answers
/// anything else with BadValue, storing nothing.
#[test]
fn xf86vidmode_set_gamma_out_of_range_is_bad_value() {
    use yserver_protocol::x11::xf86vidmode as x11vm;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();

    for (seq, (red, green, blue)) in [
        (5_000, 0, 0),
        (999, 10_000, 10_000),
        (10_000, 100_001, 10_000),
    ]
    .into_iter()
    .enumerate()
    {
        #[allow(clippy::cast_possible_truncation)]
        let seq = seq as u16 + 1;
        let reply = dispatch_vidmode_with_backend(
            &mut state,
            &mut backend,
            &mut peer,
            seq,
            x11vm::SET_GAMMA,
            &set_gamma_body(red, green, blue),
        );
        assert_vidmode_error(&reply, x11::error::BAD_VALUE, x11vm::SET_GAMMA);
    }
    assert_eq!(
        get_gamma(&mut state, &mut backend, &mut peer),
        [10_000; 3],
        "a refused SetGamma stores nothing"
    );

    let edges = dispatch_vidmode_with_backend(
        &mut state,
        &mut backend,
        &mut peer,
        10,
        x11vm::SET_GAMMA,
        &set_gamma_body(1_000, 100_000, 10_000),
    );
    assert!(edges.is_empty(), "0.1 and 10 are in range: {edges:?}");
    assert_eq!(
        get_gamma(&mut state, &mut backend, &mut peer),
        [1_000, 100_000, 10_000]
    );

    let short = dispatch_vidmode_with_backend(
        &mut state,
        &mut backend,
        &mut peer,
        11,
        x11vm::SET_GAMMA,
        &[0; 4],
    );
    assert_vidmode_error(&short, x11::error::BAD_LENGTH, x11vm::SET_GAMMA);
}

/// SetGammaRamp takes exactly the table's size, GetGammaRampSize's 1024,
/// as a request of exactly that many entries, and makes the ramp the table
/// (`ComputeGammaRamp`, values printed by Xorg's own function) without
/// touching GetGamma's values or the CRTC's RANDR ramp.
#[test]
fn xf86vidmode_local_set_gamma_ramp_replaces_the_vidmode_table() {
    use crate::backend::gamma::identity_ramp;
    use yserver_protocol::x11::xf86vidmode as x11vm;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let crtc = current_vidmode_output(&state)
        .expect("active output")
        .crtc_id;
    let mut backend = RecordingBackend::new();

    let wrong_size = dispatch_vidmode_with_backend(
        &mut state,
        &mut backend,
        &mut peer,
        1,
        x11vm::SET_GAMMA_RAMP,
        &[0, 0, 0, 1], // the CRTC's 256, not the table's 1024
    );
    assert_vidmode_error(&wrong_size, x11::error::BAD_VALUE, x11vm::SET_GAMMA_RAMP);

    let mut body = vec![0, 0, 0, 4]; // screen 0, size 1024
    let truncated = dispatch_vidmode_with_backend(
        &mut state,
        &mut backend,
        &mut peer,
        2,
        x11vm::SET_GAMMA_RAMP,
        &body,
    );
    assert_vidmode_error(&truncated, x11::error::BAD_LENGTH, x11vm::SET_GAMMA_RAMP);

    let channels: [Vec<u16>; 3] = [
        (0..1024).map(|i| 65535 - i * 64).collect(),
        (0..1024).map(|i| i * 64 + 63).collect(),
        (0..1024u32).map(|i| (i * 37) as u16).collect(),
    ];
    for channel in &channels {
        for entry in channel {
            body.extend_from_slice(&entry.to_le_bytes());
        }
    }
    let reply = dispatch_vidmode_with_backend(
        &mut state,
        &mut backend,
        &mut peer,
        3,
        x11vm::SET_GAMMA_RAMP,
        &body,
    );
    assert!(reply.is_empty(), "SetGammaRamp has no reply: {reply:?}");
    let table = backend
        .vidmode_gamma
        .clone()
        .expect("the backend got the table");
    assert_eq!(
        (table.red[0], table.red[1], table.blue[1023]),
        (1023, 1022, 591)
    );
    let ramp = get_gamma_ramp(&mut state, &mut backend, &mut peer);
    assert_eq!((ramp.0[0], ramp.1[0], ramp.2[0]), (65535, 0, 0));
    assert_eq!((ramp.0[1], ramp.1[1], ramp.2[1]), (65471, 64, 0));
    assert_eq!(
        (ramp.0[1023], ramp.1[1023], ramp.2[1023]),
        (0, 65535, 37860)
    );
    assert_eq!(
        get_gamma(&mut state, &mut backend, &mut peer),
        [10_000; 3],
        "SetGammaRamp leaves GetGamma's values"
    );
    let identity = identity_ramp(256);
    assert_eq!(
        backend.get_crtc_gamma(crtc),
        (identity.clone(), identity.clone(), identity),
        "the CRTC's RANDR ramp is untouched"
    );

    let short = dispatch_vidmode_with_backend(
        &mut state,
        &mut backend,
        &mut peer,
        4,
        x11vm::SET_GAMMA_RAMP,
        &[0, 0],
    );
    assert_vidmode_error(&short, x11::error::BAD_LENGTH, x11vm::SET_GAMMA_RAMP);
}

/// LockModeSwitch succeeds for a local client; mode setting stays RANDR's
/// and keeps ClientNotLocal.
#[test]
fn xf86vidmode_local_lock_succeeds_and_mode_setting_stays_refused() {
    use yserver_protocol::x11::xf86vidmode as x11vm;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);

    let lock = dispatch_vidmode(
        &mut state,
        &mut peer,
        1,
        x11vm::LOCK_MODE_SWITCH,
        &[0, 0, 1, 0],
    );
    assert!(lock.is_empty(), "LockModeSwitch has no reply: {lock:?}");
    let other_screen = dispatch_vidmode(
        &mut state,
        &mut peer,
        2,
        x11vm::LOCK_MODE_SWITCH,
        &[1, 0, 1, 0],
    );
    assert_vidmode_error(
        &other_screen,
        x11::error::BAD_VALUE,
        x11vm::LOCK_MODE_SWITCH,
    );
    let short = dispatch_vidmode(&mut state, &mut peer, 3, x11vm::LOCK_MODE_SWITCH, &[0, 0]);
    assert_vidmode_error(&short, x11::error::BAD_LENGTH, x11vm::LOCK_MODE_SWITCH);

    for (seq, minor) in [2u8, 3, 7, 8, 10, 12].into_iter().enumerate() {
        #[allow(clippy::cast_possible_truncation)]
        let seq = seq as u16 + 10;
        let reply = dispatch_vidmode(&mut state, &mut peer, seq, minor, &[]);
        assert_vidmode_error(
            &reply,
            crate::nested::XF86VIDMODE_FIRST_ERROR + x11vm::CLIENT_NOT_LOCAL,
            minor,
        );
    }
}
