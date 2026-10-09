use super::*;

#[test]
fn active_monitors_matches_outputs_and_order() {
    let mut state = ServerState::new();
    state.randr.outputs = vec![
        crate::randr::RandrOutput {
            name: "DP-1".into(),
            output_id: 1,
            crtc_id: 1,
            mode_id: 1,
            connected: true,
            x: 0,
            y: 0,
            width: 2560,
            height: 1440,
            vrefresh: 60,
            timing: None,
            mm_width: 0,
            mm_height: 0,
            mode_ids: vec![1],
            num_preferred: 1,
            pending_transform: Default::default(),
            current_transform: Default::default(),
            rotation: crate::randr::RR_ROTATE_0,
        },
        crate::randr::RandrOutput {
            name: "HDMI-A-1".into(),
            output_id: 2,
            crtc_id: 2,
            mode_id: 1,
            connected: false,
            x: 2560,
            y: 0,
            width: 2560,
            height: 1440,
            vrefresh: 60,
            timing: None,
            mm_width: 0,
            mm_height: 0,
            mode_ids: vec![1],
            num_preferred: 1,
            pending_transform: Default::default(),
            current_transform: Default::default(),
            rotation: crate::randr::RR_ROTATE_0,
        },
    ];

    let monitors = active_monitors(&state, true);
    assert_eq!(monitors.len(), state.randr.outputs.len());
    assert_eq!(monitors.len(), 2);
    assert!(monitors[0].primary);
    assert!(!monitors[1].primary);
    assert_eq!(monitors[1].x, 2560);
    assert_eq!(
        (monitors[1].width_mm, monitors[1].height_mm),
        (677, 381),
        "automatic monitor geometry still derives physical size from its retained CRTC",
    );
}

/// The two-output guest of `tools/vng-scenarios/xrandr-monitors.sh
/// --outputs 2` after `--right-of`: Virtual-1 1920x1440+0+0 (primary),
/// Virtual-2 1360x768+1920+0, both 325x203 mm.
fn monitor_fixture(byte_order: ClientByteOrder) -> (ServerState, UnixStream) {
    let output = |name: &str, id: u32, x: i16, width: u16, height: u16| crate::randr::RandrOutput {
        name: name.into(),
        output_id: id,
        crtc_id: id + 2,
        mode_id: id + 4,
        connected: true,
        x,
        y: 0,
        width,
        height,
        vrefresh: 60,
        timing: None,
        mm_width: 325,
        mm_height: 203,
        mode_ids: vec![id + 4],
        num_preferred: 1,
        pending_transform: Default::default(),
        current_transform: Default::default(),
        rotation: crate::randr::RR_ROTATE_0,
    };
    let mut state = ServerState::new();
    state.randr = crate::randr::RandrState::from_outputs(
        7,
        vec![
            output("Virtual-1", 1, 0, 1920, 1440),
            output("Virtual-2", 2, 1920, 1360, 768),
        ],
    );
    let peer = install_client(&mut state, 1);
    state.clients.get_mut(&1).unwrap().byte_order = byte_order;
    (state, peer)
}

fn wire_u32(bo: ClientByteOrder, b: &[u8]) -> u32 {
    let b: [u8; 4] = b[..4].try_into().unwrap();
    match bo {
        ClientByteOrder::LittleEndian => u32::from_le_bytes(b),
        ClientByteOrder::BigEndian => u32::from_be_bytes(b),
    }
}

fn wire_u16(bo: ClientByteOrder, b: &[u8]) -> u16 {
    let b: [u8; 2] = b[..2].try_into().unwrap();
    match bo {
        ClientByteOrder::LittleEndian => u16::from_le_bytes(b),
        ClientByteOrder::BigEndian => u16::from_be_bytes(b),
    }
}

/// Swap `body` as the reader would, then dispatch it as RANDR `minor`.
fn randr_wire_request(
    state: &mut ServerState,
    peer: &mut UnixStream,
    minor: u8,
    body: WireBody,
) -> Vec<u8> {
    let WireBody(byte_order, mut body) = body;
    yserver_protocol::x11::request_swap::swap_request_body(128, minor, byte_order, &mut body);
    handle_randr_request(
        state,
        &mut RecordingBackend::new(),
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode: 128,
            data: minor,
            length_units: u32::try_from(1 + body.len() / 4).unwrap(),
        },
        &body,
    )
    .expect("RANDR request");
    read_all_available(peer)
}

/// `SetMonitor` body: window, then name primary automatic noutput x y
/// width height mm-width mm-height outputs.
#[allow(clippy::too_many_arguments)]
fn set_monitor_body(
    bo: ClientByteOrder,
    window: u32,
    name: u32,
    primary: bool,
    noutput: u16,
    geometry: (i16, i16, u16, u16),
    mm: (u32, u32),
    outputs: &[u32],
) -> WireBody {
    #[allow(clippy::cast_sign_loss)]
    let mut body = WireBody(bo, Vec::new())
        .u32(window)
        .u32(name)
        .bytes(&[u8::from(primary), 0])
        .u16(noutput)
        .u16(geometry.0 as u16)
        .u16(geometry.1 as u16)
        .u16(geometry.2)
        .u16(geometry.3)
        .u32(mm.0)
        .u32(mm.1);
    for &output in outputs {
        body = body.u32(output);
    }
    body
}

fn set_monitor(
    state: &mut ServerState,
    peer: &mut UnixStream,
    name: &str,
    primary: bool,
    geometry: (i16, i16, u16, u16),
    mm: (u32, u32),
    outputs: &[u32],
) -> Vec<u8> {
    let bo = state.clients[&1].byte_order;
    let atom = state.atoms.intern(name, false).0;
    #[allow(clippy::cast_possible_truncation)]
    let body = set_monitor_body(
        bo,
        ROOT_WINDOW.0,
        atom,
        primary,
        outputs.len() as u16,
        geometry,
        mm,
        outputs,
    );
    randr_wire_request(state, peer, x11randr_minor::SET_MONITOR, body)
}

fn delete_monitor(state: &mut ServerState, peer: &mut UnixStream, name: u32) -> Vec<u8> {
    let bo = state.clients[&1].byte_order;
    let body = WireBody(bo, Vec::new()).u32(ROOT_WINDOW.0).u32(name);
    randr_wire_request(state, peer, x11randr_minor::DELETE_MONITOR, body)
}

/// One decoded `GetMonitors` entry: name, primary, automatic,
/// "WxH+X+Y", "mmWxmmH", outputs.
type WireMonitor = (String, bool, bool, String, (u32, u32), Vec<u32>);

fn get_monitors(
    state: &mut ServerState,
    peer: &mut UnixStream,
    get_active: bool,
) -> Vec<WireMonitor> {
    let bo = state.clients[&1].byte_order;
    let body = WireBody(bo, Vec::new())
        .u32(ROOT_WINDOW.0)
        .bytes(&[u8::from(get_active), 0, 0, 0]);
    let r = randr_wire_request(
        state,
        peer,
        yserver_protocol::x11::randr::RR_GET_MONITORS,
        body,
    );
    assert_eq!(r[0], 1, "GetMonitors reply: {r:02x?}");
    assert_eq!(
        wire_u32(bo, &r[4..]) as usize * 4 + 32,
        r.len(),
        "reply length"
    );
    let count = wire_u32(bo, &r[12..]);
    let mut total_outputs = 0;
    let mut offset = 32;
    let mut monitors = Vec::new();
    for _ in 0..count {
        let m = &r[offset..];
        let n_out = usize::from(wire_u16(bo, &m[6..]));
        #[allow(clippy::cast_possible_wrap)]
        let geometry = format!(
            "{}x{}+{}+{}",
            wire_u16(bo, &m[12..]),
            wire_u16(bo, &m[14..]),
            wire_u16(bo, &m[8..]) as i16,
            wire_u16(bo, &m[10..]) as i16,
        );
        let name = state
            .atoms
            .name(AtomId(wire_u32(bo, m)))
            .unwrap_or("?")
            .to_string();
        let outputs = (0..n_out).map(|i| wire_u32(bo, &m[24 + i * 4..])).collect();
        monitors.push((
            name,
            m[4] != 0,
            m[5] != 0,
            geometry,
            (wire_u32(bo, &m[16..]), wire_u32(bo, &m[20..])),
            outputs,
        ));
        total_outputs += n_out;
        offset += 24 + n_out * 4;
    }
    assert_eq!(offset, r.len());
    assert_eq!(wire_u32(bo, &r[16..]) as usize, total_outputs, "noutputs");
    monitors
}

fn wm(
    name: &str,
    primary: bool,
    automatic: bool,
    geometry: &str,
    mm: (u32, u32),
    outputs: &[u32],
) -> WireMonitor {
    (
        name.into(),
        primary,
        automatic,
        geometry.into(),
        mm,
        outputs.to_vec(),
    )
}

/// XINERAMA `(GetScreenCount, QueryScreens)` in the client's order.
fn xinerama_heads(state: &mut ServerState, peer: &mut UnixStream) -> (u8, Vec<String>) {
    use yserver_protocol::x11::xinerama as xin;
    let bo = state.clients[&1].byte_order;
    let mut send = |state: &mut ServerState, minor: u8, body: &[u8]| {
        handle_xinerama_request(
            state,
            ClientId(1),
            SequenceNumber(1),
            RequestHeader {
                opcode: 151,
                data: minor,
                length_units: u32::try_from(1 + body.len() / 4).unwrap(),
            },
            body,
        )
        .expect("XINERAMA request");
        read_all_available(peer)
    };
    let window = WireBody(bo, Vec::new()).u32(ROOT_WINDOW.0).1;
    let count = send(state, xin::GET_SCREEN_COUNT, &window);
    let screens = send(state, xin::QUERY_SCREENS, &[]);
    let n = wire_u32(bo, &screens[8..]) as usize;
    #[allow(clippy::cast_possible_wrap)]
    let heads = (0..n)
        .map(|i| {
            let s = &screens[32 + i * 8..];
            format!(
                "{}x{}+{}+{}",
                wire_u16(bo, &s[4..]),
                wire_u16(bo, &s[6..]),
                wire_u16(bo, s) as i16,
                wire_u16(bo, &s[2..]) as i16,
            )
        })
        .collect();
    (count[1], heads)
}

const BOTH_ORDERS: [ClientByteOrder; 2] =
    [ClientByteOrder::LittleEndian, ClientByteOrder::BigEndian];

/// Measured split: `--setmonitor left 960/170x1440/211+0+0 Virtual-1`
/// and `right ...+960+0 none` hide Virtual-1's automatic monitor, keep
/// Virtual-2's, and nobody is primary (the primary output is covered).
#[test]
fn set_monitor_split_replaces_the_covered_automatic_monitor() {
    for bo in BOTH_ORDERS {
        let (mut state, mut peer) = monitor_fixture(bo);
        let r = set_monitor(
            &mut state,
            &mut peer,
            "left",
            false,
            (0, 0, 960, 1440),
            (170, 211),
            &[1],
        );
        assert!(r.is_empty(), "{r:02x?}");
        let r = set_monitor(
            &mut state,
            &mut peer,
            "right",
            false,
            (960, 0, 960, 1440),
            (170, 211),
            &[],
        );
        assert!(r.is_empty(), "{r:02x?}");
        let want = vec![
            wm("left", false, false, "960x1440+0+0", (170, 211), &[1]),
            wm("right", false, false, "960x1440+960+0", (170, 211), &[]),
            wm(
                "Virtual-2",
                false,
                true,
                "1360x768+1920+0",
                (325, 203),
                &[2],
            ),
        ];
        assert_eq!(get_monitors(&mut state, &mut peer, false), want, "{bo:?}");
        assert_eq!(get_monitors(&mut state, &mut peer, true), want, "{bo:?}");
        assert_eq!(
            xinerama_heads(&mut state, &mut peer),
            (
                3,
                vec![
                    "960x1440+0+0".into(),
                    "960x1440+960+0".into(),
                    "1360x768+1920+0".into()
                ]
            ),
        );
    }
}

/// Measured: a 0x0+0+0 monitor over both outputs is their union, and
/// its physical size is Xorg 21.1's integer `last_w / first_w *
/// first_mm` — 1360/1920 = 0 — so 0x0 mm. With Virtual-2 off it is
/// Virtual-1's geometry alone.
#[test]
fn automatic_geometry_client_monitor_follows_the_crtc_layout() {
    for bo in BOTH_ORDERS {
        let (mut state, mut peer) = monitor_fixture(bo);
        set_monitor(
            &mut state,
            &mut peer,
            "both",
            false,
            (0, 0, 0, 0),
            (0, 0),
            &[1, 2],
        );
        assert_eq!(
            get_monitors(&mut state, &mut peer, false),
            vec![wm("both", false, false, "3280x1440+0+0", (0, 0), &[1, 2])],
        );
        state.randr.outputs[1].mode_id = 0;
        assert_eq!(
            get_monitors(&mut state, &mut peer, false),
            vec![wm(
                "both",
                false,
                false,
                "1920x1440+0+0",
                (325, 203),
                &[1, 2]
            )],
        );
    }
}

/// Measured `cprim`: a primary client monitor leads the list, and the
/// uncovered CRTC of the primary output is ALSO reported primary —
/// Xorg's leading entry does not count towards `has_primary`.
#[test]
fn client_primary_and_uncovered_primary_output_are_both_primary() {
    for bo in BOTH_ORDERS {
        let (mut state, mut peer) = monitor_fixture(bo);
        state.randr.primary_output = 2;
        set_monitor(
            &mut state,
            &mut peer,
            "autogeo",
            false,
            (0, 0, 0, 0),
            (0, 0),
            &[1],
        );
        set_monitor(
            &mut state,
            &mut peer,
            "cprim",
            true,
            (0, 0, 0, 0),
            (0, 0),
            &[1],
        );
        assert_eq!(
            get_monitors(&mut state, &mut peer, true),
            vec![
                wm("cprim", true, false, "1920x1440+0+0", (325, 203), &[1]),
                wm("autogeo", false, false, "1920x1440+0+0", (325, 203), &[1]),
                wm("Virtual-2", true, true, "1360x768+1920+0", (325, 203), &[2]),
            ],
        );
    }
}

/// Measured `extra-primary`: a new primary monitor clears the old one's
/// flag; deleting it leaves no primary client monitor behind.
#[test]
fn a_new_primary_client_monitor_clears_the_previous_one() {
    for bo in BOTH_ORDERS {
        let (mut state, mut peer) = monitor_fixture(bo);
        set_monitor(
            &mut state,
            &mut peer,
            "left",
            false,
            (0, 0, 960, 1440),
            (170, 211),
            &[1],
        );
        set_monitor(
            &mut state,
            &mut peer,
            "right",
            true,
            (960, 0, 960, 1440),
            (100, 50),
            &[],
        );
        set_monitor(
            &mut state,
            &mut peer,
            "extra",
            true,
            (0, 0, 10, 10),
            (100, 50),
            &[],
        );
        let names = |m: Vec<WireMonitor>| m.into_iter().map(|m| (m.0, m.1)).collect::<Vec<_>>();
        assert_eq!(
            names(get_monitors(&mut state, &mut peer, false)),
            vec![
                ("extra".into(), true),
                ("left".into(), false),
                ("right".into(), false),
                ("Virtual-2".into(), false),
            ],
        );
        let extra = state.atoms.intern("extra", true).0;
        assert!(delete_monitor(&mut state, &mut peer, extra).is_empty());
        assert_eq!(
            names(get_monitors(&mut state, &mut peer, false)),
            vec![
                ("left".into(), false),
                ("right".into(), false),
                ("Virtual-2".into(), false),
            ],
        );
    }
}

/// Measured `empty`: a 0x0 monitor with no outputs is listed by
/// `GetMonitors(get_active=0)` and counted by XINERAMA GetScreenCount,
/// but neither `get_active=1` nor QueryScreens reports it.
#[test]
fn an_empty_monitor_is_counted_but_not_active() {
    for bo in BOTH_ORDERS {
        let (mut state, mut peer) = monitor_fixture(bo);
        set_monitor(
            &mut state,
            &mut peer,
            "empty",
            false,
            (0, 0, 0, 0),
            (100, 50),
            &[],
        );
        let all = get_monitors(&mut state, &mut peer, false);
        assert_eq!(all.len(), 3);
        assert_eq!(all[1], wm("empty", false, false, "0x0+0+0", (100, 50), &[]));
        assert_eq!(get_monitors(&mut state, &mut peer, true).len(), 2);
        let (count, heads) = xinerama_heads(&mut state, &mut peer);
        assert_eq!((count, heads.len()), (3, 2));
    }
}

/// The error table measured on Xorg 21.1 (`mon.py errors`): codes and
/// wire values, no state change on failure, and no validation of the
/// output ids.
#[test]
fn set_and_delete_monitor_errors_match_xorg() {
    use yserver_protocol::x11::error;
    for bo in BOTH_ORDERS {
        let (mut state, mut peer) = monitor_fixture(bo);
        let err = |r: &[u8]| {
            assert_eq!(r.len(), 32, "one error: {r:02x?}");
            assert_eq!(r[0], 0);
            (r[1], wire_u32(bo, &r[4..]), wire_u16(bo, &r[8..]), r[10])
        };
        let output_atom = state.atoms.intern("Virtual-1", false).0;
        let name = state.atoms.intern("errmon", false).0;
        let root = ROOT_WINDOW.0;
        let set = |state: &mut ServerState,
                   peer: &mut UnixStream,
                   window,
                   name,
                   noutput,
                   outputs: &[u32]| {
            let body = set_monitor_body(
                bo,
                window,
                name,
                false,
                noutput,
                (0, 0, 10, 10),
                (100, 50),
                outputs,
            );
            randr_wire_request(state, peer, x11randr_minor::SET_MONITOR, body)
        };
        let cases: Vec<(&str, Vec<u8>, (u8, u32))> = vec![
            (
                "name=output",
                set(&mut state, &mut peer, root, output_atom, 0, &[]),
                (error::BAD_VALUE, output_atom),
            ),
            (
                "name=None",
                set(&mut state, &mut peer, root, 0, 0, &[]),
                (error::BAD_ATOM, root),
            ),
            (
                "name=0x7fffff",
                set(&mut state, &mut peer, root, 0x7f_ffff, 0, &[]),
                (error::BAD_ATOM, root),
            ),
            (
                "bad window",
                set(&mut state, &mut peer, 0x7ff_fffe, name, 0, &[]),
                (error::BAD_WINDOW, 0x7ff_fffe),
            ),
            (
                "noutput=1, none sent",
                set(&mut state, &mut peer, root, name, 1, &[]),
                (error::BAD_LENGTH, 0),
            ),
            (
                "noutput=0, one sent",
                set(&mut state, &mut peer, root, name, 0, &[1]),
                (error::BAD_LENGTH, 0),
            ),
        ];
        for (label, r, (code, value)) in cases {
            assert_eq!(err(&r), (code, value, 43, 128), "SetMonitor {label} {bo:?}");
        }
        let short = randr_wire_request(
            &mut state,
            &mut peer,
            x11randr_minor::SET_MONITOR,
            WireBody(bo, Vec::new()).u32(root).u32(name),
        );
        assert_eq!(err(&short).0, error::BAD_LENGTH);
        assert!(state.randr_client_monitors.is_empty());

        // Output ids are not validated (measured: `bogusout` succeeded).
        let bogus = state.atoms.intern("bogusout", false).0;
        assert!(set(&mut state, &mut peer, root, bogus, 1, &[0x7777]).is_empty());
        assert_eq!(
            err(&set(&mut state, &mut peer, root, bogus, 0, &[])),
            (error::BAD_VALUE, bogus, 43, 128),
            "21.1 refuses a name already in use",
        );

        let never = state.atoms.intern("nosuchmon", false).0;
        let del = |state: &mut ServerState, peer: &mut UnixStream, window: u32, name: u32| {
            let body = WireBody(bo, Vec::new()).u32(window).u32(name);
            randr_wire_request(state, peer, x11randr_minor::DELETE_MONITOR, body)
        };
        let cases: Vec<(&str, Vec<u8>, (u8, u32))> = vec![
            (
                "never set",
                del(&mut state, &mut peer, root, never),
                (error::BAD_VALUE, never),
            ),
            (
                "None",
                del(&mut state, &mut peer, root, 0),
                (error::BAD_ATOM, 0),
            ),
            (
                "0x7fffff",
                del(&mut state, &mut peer, root, 0x7f_ffff),
                (error::BAD_ATOM, 0x7f_ffff),
            ),
            (
                "bad window",
                del(&mut state, &mut peer, 0x7ff_fffe, bogus),
                (error::BAD_WINDOW, 0x7ff_fffe),
            ),
            (
                "output name",
                del(&mut state, &mut peer, root, output_atom),
                (error::BAD_VALUE, output_atom),
            ),
        ];
        for (label, r, (code, value)) in cases {
            assert_eq!(
                err(&r),
                (code, value, 44, 128),
                "DeleteMonitor {label} {bo:?}"
            );
        }
        let long = randr_wire_request(
            &mut state,
            &mut peer,
            x11randr_minor::DELETE_MONITOR,
            WireBody(bo, Vec::new()).u32(root).u32(bogus).u32(0),
        );
        assert_eq!(err(&long).0, error::BAD_LENGTH);
        assert_eq!(
            state.randr_client_monitors.len(),
            1,
            "bogusout survived every failure"
        );
        assert!(del(&mut state, &mut peer, root, bogus).is_empty());
        assert!(state.randr_client_monitors.is_empty());
    }
}

/// Measured `mon.py events`: each successful Set/DeleteMonitor sends one
/// core ConfigureNotify on the root (`RRSendConfigNotify`) and no RANDR
/// event; a failed one sends nothing.
#[test]
fn set_and_delete_monitor_notify_with_a_root_configure_notify() {
    for bo in BOTH_ORDERS {
        let (mut state, mut peer) = monitor_fixture(bo);
        state
            .clients
            .get_mut(&1)
            .unwrap()
            .event_masks
            .insert(ROOT_WINDOW, 0x0002_0000);
        state.randr_select_masks.insert((1, ROOT_WINDOW), 0x1f);
        let root = state
            .resources
            .window(ROOT_WINDOW)
            .map(|w| (w.width, w.height))
            .unwrap();
        let configure_notify = |r: &[u8]| {
            assert_eq!(r.len(), 32, "exactly one event: {r:02x?}");
            assert_eq!(r[0], 22, "ConfigureNotify");
            assert_eq!(wire_u32(bo, &r[4..]), ROOT_WINDOW.0, "event window");
            assert_eq!(wire_u32(bo, &r[8..]), ROOT_WINDOW.0, "window");
            assert_eq!(wire_u32(bo, &r[12..]), 0, "above-sibling None");
            assert_eq!((wire_u16(bo, &r[20..]), wire_u16(bo, &r[22..])), root);
        };
        configure_notify(&set_monitor(
            &mut state,
            &mut peer,
            "evmon",
            false,
            (0, 0, 100, 100),
            (100, 50),
            &[],
        ));
        let again = set_monitor(
            &mut state,
            &mut peer,
            "evmon",
            false,
            (0, 0, 200, 100),
            (100, 50),
            &[],
        );
        assert_eq!(
            (again.len(), again[0]),
            (32, 0),
            "reused name: an error, no event"
        );
        let evmon = state.atoms.intern("evmon", true).0;
        configure_notify(&delete_monitor(&mut state, &mut peer, evmon));
        let again = delete_monitor(&mut state, &mut peer, evmon);
        assert_eq!(
            (again.len(), again[0]),
            (32, 0),
            "unknown name: an error, no event"
        );
    }
}

#[test]
fn vidmode_retains_a_light_disconnected_assigned_primary() {
    let mut state = ServerState::new();
    let primary = state.randr.primary_output;
    let expected = state
        .randr
        .outputs
        .iter_mut()
        .find(|output| output.output_id == primary)
        .map(|output| {
            output.connected = false;
            (output.output_id, output.crtc_id)
        })
        .unwrap();

    let current = current_vidmode_output(&state).expect("the CRTC remains assigned");
    assert_eq!((current.output_id, current.crtc_id), expected);
}

fn randr_unimplemented_reply_bearing(minor: u8) -> Vec<u8> {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let header = RequestHeader {
        opcode: 128,
        data: minor,
        length_units: 1,
    };
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        header,
        &[],
    )
    .expect("process");
    read_all_available(&mut peer)
}

fn randr_transform_body(crtc: u32, matrix: [i32; 9], filter_name: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(44 + filter_name.len().next_multiple_of(4));
    body.extend_from_slice(&crtc.to_le_bytes());
    for cell in matrix {
        body.extend_from_slice(&cell.to_le_bytes());
    }
    body.extend_from_slice(&(filter_name.len() as u16).to_le_bytes());
    body.extend_from_slice(&[0u8; 2]);
    body.extend_from_slice(filter_name);
    body.resize(body.len().next_multiple_of(4), 0);
    body
}

#[test]
fn randr_create_mode_unimplemented_returns_error_not_hang() {
    // RRCreateMode (minor 16): reply-bearing, unimplemented → error.
    let bytes = randr_unimplemented_reply_bearing(16);
    assert!(
        bytes.len() >= 32,
        "expected error, not a hang: {bytes:02x?}"
    );
    assert_eq!(bytes[1], x11::error::BAD_IMPLEMENTATION, "code");
    assert_eq!(&bytes[8..10], &16u16.to_le_bytes(), "minor");
    assert_eq!(bytes[10], 128, "major = RANDR");
}

#[test]
fn randr_crtc_setters_reject_short_bodies_with_bad_length() {
    for minor in [
        yserver_protocol::x11::randr::RR_SET_CRTC_TRANSFORM,
        yserver_protocol::x11::randr::RR_SET_PANNING,
    ] {
        let bytes = randr_unimplemented_reply_bearing(minor);
        assert_eq!(bytes.len(), 32, "minor {minor} must return an error");
        assert_eq!(bytes[1], x11::error::BAD_LENGTH, "minor {minor} code");
        assert_eq!(&bytes[8..10], &u16::from(minor).to_le_bytes());
        assert_eq!(bytes[10], 128, "major = RANDR");
    }
}

#[test]
fn randr_create_lease_unimplemented_returns_error_not_hang() {
    // RRCreateLease (minor 45): reply-bearing, unimplemented → error.
    let bytes = randr_unimplemented_reply_bearing(45);
    assert!(
        bytes.len() >= 32,
        "expected error, not a hang: {bytes:02x?}"
    );
    assert_eq!(bytes[1], x11::error::BAD_IMPLEMENTATION, "code");
    assert_eq!(&bytes[8..10], &45u16.to_le_bytes(), "minor");
    assert_eq!(bytes[10], 128, "major = RANDR");
}

#[test]
fn randr_unsupported_warning_latch_is_per_minor() {
    let mut state = ServerState::new();
    assert!(mark_randr_unsupported_warned(&mut state, 18));
    assert!(!mark_randr_unsupported_warned(&mut state, 18));
    assert!(mark_randr_unsupported_warned(&mut state, 19));
}

const RR_IDENTITY: [i32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x0001_0000];

fn randr_transform_body_with_params(
    crtc: u32,
    matrix: [i32; 9],
    filter_name: &[u8],
    params: &[i32],
) -> Vec<u8> {
    let mut body = randr_transform_body(crtc, matrix, filter_name);
    for param in params {
        body.extend_from_slice(&param.to_le_bytes());
    }
    body
}

/// Send one SetCrtcTransform; the error code it produced, if any.
fn randr_set_crtc_transform(
    state: &mut ServerState,
    peer: &mut UnixStream,
    body: &[u8],
) -> Option<u8> {
    let mut backend = RecordingBackend::new();
    let header = RequestHeader {
        opcode: 128,
        data: yserver_protocol::x11::randr::RR_SET_CRTC_TRANSFORM,
        length_units: u32::try_from(1 + body.len() / 4).unwrap(),
    };
    handle_randr_request(
        state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        header,
        body,
    )
    .expect("SetCrtcTransform");
    let out = read_all_available(peer);
    if out.is_empty() {
        return None;
    }
    assert_eq!(out.len(), 32, "one error");
    assert_eq!(out[0], 0, "error packet");
    assert_eq!(out[10], 128, "major = RANDR");
    Some(out[1])
}

#[test]
fn randr_set_crtc_transform_validates_in_xorg_order() {
    // rrcrtc.c:1755-1785 and RRCrtcTransformSet, then D2. Each case
    // also carries the fault of every later step, so it shows the
    // earlier check wins. BadAccess (leased CRTC) is not reachable:
    // yserver has no RANDR leases.
    let mut state = ServerState::new();
    let crtc = state.randr.outputs[0].crtc_id;
    let mut peer = install_client(&mut state, 1);
    let bad_crtc = RANDR_BAD_CRTC;
    let singular = [0i32; 9];
    let rotate = [0, -0x0001_0000, 0, 0x0001_0000, 0, 0, 0, 0, 0x0001_0000];
    let mut overrun = randr_transform_body(crtc, singular, b"");
    overrun[40..42].copy_from_slice(&8u16.to_le_bytes());
    let mut overrun_bad_crtc = overrun.clone();
    overrun_bad_crtc[0..4].copy_from_slice(&0xdeadu32.to_le_bytes());
    let mut overrun_invertible = randr_transform_body(crtc, RR_IDENTITY, b"");
    overrun_invertible[40..42].copy_from_slice(&8u16.to_le_bytes());
    let one = 0x0001_0000;
    let cases: Vec<(&str, Vec<u8>, Option<u8>)> = vec![
        ("BadCrtc", overrun_bad_crtc, Some(bad_crtc)),
        ("non-invertible", overrun, Some(x11::error::BAD_MATCH)),
        (
            "negative nparams",
            overrun_invertible,
            Some(x11::error::BAD_LENGTH),
        ),
        (
            "unknown filter",
            randr_transform_body(crtc, rotate, b"box"),
            Some(x11::error::BAD_NAME),
        ),
        (
            "convolution parameter check",
            randr_transform_body_with_params(crtc, RR_IDENTITY, b"convolution", &[one]),
            Some(x11::error::BAD_MATCH),
        ),
        (
            "params without a filter",
            randr_transform_body_with_params(crtc, rotate, b"", &[one]),
            Some(x11::error::BAD_MATCH),
        ),
        (
            "D2: valid convolution",
            randr_transform_body_with_params(crtc, RR_IDENTITY, b"convolution", &[one, one, one]),
            Some(x11::error::BAD_MATCH),
        ),
        (
            "D2: rotation",
            randr_transform_body(crtc, rotate, b"good"),
            Some(x11::error::BAD_MATCH),
        ),
        (
            "D2: translation",
            randr_transform_body(crtc, [one, 0, 5 * one, 0, one, 0, 0, 0, one], b""),
            Some(x11::error::BAD_MATCH),
        ),
        (
            "pure scale",
            randr_transform_body(crtc, rr_scale(104_857), b"good"),
            None,
        ),
        (
            "bilinear keeps its parameters",
            randr_transform_body_with_params(crtc, rr_scale(131_072), b"bilinear", &[one]),
            None,
        ),
        (
            "identity with a filter",
            randr_transform_body(crtc, RR_IDENTITY, b"FAST"),
            None,
        ),
    ];
    for (name, body, expected) in cases {
        assert_eq!(
            randr_set_crtc_transform(&mut state, &mut peer, &body),
            expected,
            "{name}"
        );
    }
}

fn randr_get_crtc_transform(state: &mut ServerState, peer: &mut UnixStream, crtc: u32) -> Vec<u8> {
    randr_get_crtc_transform_body(state, peer, &crtc.to_le_bytes())
}

fn randr_get_crtc_transform_body(
    state: &mut ServerState,
    peer: &mut UnixStream,
    body: &[u8],
) -> Vec<u8> {
    let mut backend = RecordingBackend::new();
    handle_randr_request(
        state,
        &mut backend,
        ClientId(1),
        SequenceNumber(2),
        RequestHeader {
            opcode: 128,
            data: yserver_protocol::x11::randr::RR_GET_CRTC_TRANSFORM,
            length_units: u32::try_from(1 + body.len() / 4).unwrap(),
        },
        body,
    )
    .expect("GetCrtcTransform");
    read_all_available(peer)
}

#[test]
fn get_crtc_transform_of_the_wrong_length_is_bad_length_before_bad_crtc() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let crtc = state.randr.outputs[0].crtc_id;
    // Short (header only) and oversized, for a real and a bogus CRTC.
    for body in [
        Vec::new(),
        [crtc.to_le_bytes(), 0u32.to_le_bytes()].concat(),
        [0xdead_u32.to_le_bytes(), 0u32.to_le_bytes()].concat(),
    ] {
        let reply = randr_get_crtc_transform_body(&mut state, &mut peer, &body);
        assert_eq!(reply.len(), 32, "{body:?}");
        assert_eq!(
            (reply[0], reply[1]),
            (0, x11::error::BAD_LENGTH),
            "{body:?}"
        );
        assert_eq!(
            (reply[8], reply[10]),
            (yserver_protocol::x11::randr::RR_GET_CRTC_TRANSFORM, 128)
        );
    }
    let reply = randr_get_crtc_transform(&mut state, &mut peer, 0xdead);
    assert_eq!(
        (reply[0], reply[1]),
        (0, RANDR_BAD_CRTC),
        "the right size looks up"
    );
}

#[test]
fn randr_set_crtc_transform_stores_pending_for_get_crtc_transform() {
    let mut state = ServerState::new();
    let crtc = state.randr.outputs[0].crtc_id;
    let mut peer = install_client(&mut state, 1);

    let reply = randr_get_crtc_transform(&mut state, &mut peer, crtc);
    assert_eq!(reply.len(), 96, "default: no filter bytes");
    assert_eq!(reply[44], 1, "hasTransforms");
    assert_eq!(&reply[88..96], &[0u8; 8]);

    // muffin's scale-down 125% CRTC 6 request (spec table): `good`.
    let body = randr_transform_body_with_params(crtc, rr_scale(104_857), b"good", &[0x8000]);
    assert_eq!(randr_set_crtc_transform(&mut state, &mut peer, &body), None);
    let output = &state.randr.outputs[0];
    assert_eq!(output.pending_transform.matrix, rr_scale(104_857));
    assert_eq!(
        output.pending_transform.filter,
        Some(crate::randr::Filter::Bilinear)
    );
    assert_eq!(
        output.current_transform,
        crate::randr::CrtcTransform::identity()
    );

    let reply = randr_get_crtc_transform(&mut state, &mut peer, crtc);
    assert_eq!(reply.len(), 108);
    assert_eq!(
        u32::from_le_bytes(reply[8..12].try_into().unwrap()),
        104_857
    );
    assert_eq!(
        u32::from_le_bytes(reply[48..52].try_into().unwrap()),
        0x0001_0000
    );
    assert_eq!(&reply[88..96], &[8, 0, 1, 0, 0, 0, 0, 0]);
    assert_eq!(&reply[96..104], b"bilinear", "canonical name, not `good`");
    assert_eq!(&reply[104..108], &0x8000i32.to_le_bytes());

    // A rejected request leaves the pending transform alone.
    let rotate = [0, -0x0001_0000, 0, 0x0001_0000, 0, 0, 0, 0, 0x0001_0000];
    let body = randr_transform_body(crtc, rotate, b"");
    assert_eq!(
        randr_set_crtc_transform(&mut state, &mut peer, &body),
        Some(x11::error::BAD_MATCH)
    );
    assert_eq!(
        state.randr.outputs[0].pending_transform.matrix,
        rr_scale(104_857)
    );
}

fn randr_crtc_config_body(crtc: u32, x: i16, y: i16, mode: u32, outputs: &[u32]) -> Vec<u8> {
    let mut body = Vec::with_capacity(24 + outputs.len() * 4);
    body.extend_from_slice(&crtc.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // timestamp
    body.extend_from_slice(&0u32.to_le_bytes()); // config_timestamp
    body.extend_from_slice(&x.to_le_bytes());
    body.extend_from_slice(&y.to_le_bytes());
    body.extend_from_slice(&mode.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes()); // RR_Rotate_0
    body.extend_from_slice(&[0u8; 2]);
    for output in outputs {
        body.extend_from_slice(&output.to_le_bytes());
    }
    body
}

fn randr_set_crtc_config(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    handle_randr_request(
        state,
        backend,
        ClientId(1),
        SequenceNumber(3),
        RequestHeader {
            opcode: 128,
            data: yserver_protocol::x11::randr::RR_SET_CRTC_CONFIG,
            length_units: u32::try_from(1 + body.len() / 4).unwrap(),
        },
        body,
    )
}

/// `(width, height)` of every CrtcChangeNotify in `wire`, and whether a
/// SetCrtcConfig Success reply is present.
fn randr_crtc_notifies_and_success(wire: &[u8]) -> (Vec<(u16, u16)>, bool) {
    let u16_at = |c: &[u8], o: usize| u16::from_le_bytes(c[o..o + 2].try_into().unwrap());
    let notifies = wire
        .chunks_exact(32)
        .filter(|c| c[0] == 89 + 1 && c[1] == yserver_protocol::x11::randr::NOTIFY_CRTC_CHANGE)
        .map(|c| (u16_at(c, 28), u16_at(c, 30)))
        .collect();
    let success = wire.chunks_exact(32).any(|c| c[0] == 1 && c[1] == 0);
    (notifies, success)
}

#[test]
fn randr_set_crtc_config_applies_a_pending_transform_as_a_change() {
    let mut state = ServerState::new();
    let output = state.randr.outputs[0].clone();
    let (crtc, mode) = (output.crtc_id, output.mode_id);
    let mut peer = install_client(&mut state, 1);
    state.randr_select_masks.insert(
        (1, crate::resources::ROOT_WINDOW),
        yserver_protocol::x11::randr::NOTIFY_MASK_CRTC_CHANGE,
    );
    let mut backend = RecordingBackend::new();
    let same_config = randr_crtc_config_body(crtc, output.x, output.y, mode, &[output.output_id]);

    // Nothing pending: the backend's no-op stays a no-op.
    randr_set_crtc_config(&mut state, &mut backend, &same_config).unwrap();
    let (notifies, success) = randr_crtc_notifies_and_success(&read_all_available(&mut peer));
    assert!(success);
    assert!(notifies.is_empty());

    // xrandr --scale 2 (spec, "What Xorg does").
    let body = randr_transform_body(crtc, rr_scale(131_072), b"");
    assert_eq!(randr_set_crtc_transform(&mut state, &mut peer, &body), None);
    randr_set_crtc_config(&mut state, &mut backend, &same_config).unwrap();
    let (notifies, success) = randr_crtc_notifies_and_success(&read_all_available(&mut peer));
    assert!(success);
    assert_eq!(
        notifies,
        vec![(output.width, output.height)],
        "one CrtcChangeNotify carrying the mode size"
    );
    let applied = &state.randr.outputs[0];
    assert_eq!(applied.current_transform.matrix, rr_scale(131_072));
    assert_eq!(applied.footprint(), (output.width * 2, output.height * 2));

    // Applied: the same config is a no-op again.
    randr_set_crtc_config(&mut state, &mut backend, &same_config).unwrap();
    let (notifies, _) = randr_crtc_notifies_and_success(&read_all_available(&mut peer));
    assert!(notifies.is_empty());

    // A disable leaves the current transform in place.
    let body = randr_transform_body(crtc, rr_scale(32_768), b"");
    assert_eq!(randr_set_crtc_transform(&mut state, &mut peer, &body), None);
    randr_set_crtc_config(
        &mut state,
        &mut backend,
        &randr_crtc_config_body(crtc, 0, 0, 0, &[]),
    )
    .unwrap();
    let _ = read_all_available(&mut peer);
    assert_eq!(
        state.randr.outputs[0].current_transform.matrix,
        rr_scale(131_072)
    );
}

fn randr_get_crtc_info(state: &mut ServerState, peer: &mut UnixStream, crtc: u32) -> Vec<u8> {
    let mut backend = RecordingBackend::new();
    let body = [crtc.to_le_bytes(), 0u32.to_le_bytes()].concat();
    handle_randr_request(
        state,
        &mut backend,
        ClientId(1),
        SequenceNumber(4),
        RequestHeader {
            opcode: 128,
            data: yserver_protocol::x11::randr::RR_GET_CRTC_INFO,
            length_units: 3,
        },
        &body,
    )
    .expect("GetCrtcInfo");
    read_all_available(peer)
}

#[test]
fn randr_set_crtc_config_rotation_is_a_change_reported_as_xorg() {
    use crate::randr::{RR_REFLECT_X, RR_ROTATE_0, RR_ROTATE_90};
    let mut state = ServerState::new();
    let output = state.randr.outputs[0].clone();
    let (crtc, mode) = (output.crtc_id, output.mode_id);
    let mut peer = install_client(&mut state, 1);
    state.randr_select_masks.insert(
        (1, crate::resources::ROOT_WINDOW),
        yserver_protocol::x11::randr::NOTIFY_MASK_CRTC_CHANGE,
    );
    let mut backend = RecordingBackend::new();
    let rotated = |rotation: u16| {
        let mut body = randr_crtc_config_body(crtc, output.x, output.y, mode, &[output.output_id]);
        body[20..22].copy_from_slice(&rotation.to_le_bytes());
        body
    };
    let u16_at = |c: &[u8], o: usize| u16::from_le_bytes(c[o..o + 2].try_into().unwrap());

    // `xrandr --rotate left`: same mode and origin, a new rotation.
    randr_set_crtc_config(&mut state, &mut backend, &rotated(RR_ROTATE_90)).unwrap();
    let wire = read_all_available(&mut peer);
    let (notifies, success) = randr_crtc_notifies_and_success(&wire);
    assert!(success);
    assert_eq!(
        notifies,
        vec![(output.width, output.height)],
        "CrtcChangeNotify keeps the mode size (rrcrtc.c:249)"
    );
    let notify = wire.chunks_exact(32).find(|c| c[0] == 90).unwrap();
    assert_eq!(u16_at(notify, 20), RR_ROTATE_90, "and carries the rotation");
    assert_eq!(state.randr.outputs[0].rotation, RR_ROTATE_90);
    assert!(state.randr.outputs[0].current_transform.is_identity());

    // GetCrtcInfo: the rotated footprint, rotation and modesetting's
    // rotations 0x3f (measured, tools/vng-scenarios/xrandr-rotate.sh).
    let reply = randr_get_crtc_info(&mut state, &mut peer, crtc);
    assert_eq!(reply[0], 1);
    assert_eq!(
        (u16_at(&reply, 16), u16_at(&reply, 18)),
        (output.height, output.width)
    );
    assert_eq!(u16_at(&reply, 24), RR_ROTATE_90);
    assert_eq!(u16_at(&reply, 26), 0x3f);

    // The same rotation again is a no-op.
    randr_set_crtc_config(&mut state, &mut backend, &rotated(RR_ROTATE_90)).unwrap();
    let (notifies, _) = randr_crtc_notifies_and_success(&read_all_available(&mut peer));
    assert!(notifies.is_empty());

    // A reflection bit is a change too.
    randr_set_crtc_config(
        &mut state,
        &mut backend,
        &rotated(RR_ROTATE_90 | RR_REFLECT_X),
    )
    .unwrap();
    let (notifies, _) = randr_crtc_notifies_and_success(&read_all_available(&mut peer));
    assert_eq!(notifies.len(), 1);
    assert_eq!(state.randr.outputs[0].rotation, RR_ROTATE_90 | RR_REFLECT_X);

    // A disable keeps the rotation, as xf86RandR12CrtcSet does.
    randr_set_crtc_config(
        &mut state,
        &mut backend,
        &randr_crtc_config_body(crtc, 0, 0, 0, &[]),
    )
    .unwrap();
    let _ = read_all_available(&mut peer);
    assert_eq!(state.randr.outputs[0].rotation, RR_ROTATE_90 | RR_REFLECT_X);
    assert_ne!(state.randr.outputs[0].rotation, RR_ROTATE_0);
}

/// A SetScreenConfig from client 1: `(status, new_timestamp,
/// new_config_timestamp, root)` of the reply, or `Err(error code)`.
fn randr_set_screen_config(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    peer: &mut UnixStream,
    body: &[u8],
) -> Result<(u8, u32, u32, u32), u8> {
    handle_randr_request(
        state,
        backend,
        ClientId(1),
        SequenceNumber(5),
        RequestHeader {
            opcode: 128,
            data: yserver_protocol::x11::randr::RR_SET_SCREEN_CONFIG,
            length_units: u32::try_from(1 + body.len() / 4).unwrap(),
        },
        body,
    )
    .expect("SetScreenConfig");
    let wire = read_all_available(peer);
    let reply = wire
        .chunks_exact(32)
        .find(|c| c[0] <= 1)
        .expect("a reply or an error");
    let u32_at = |o: usize| u32::from_le_bytes(reply[o..o + 4].try_into().unwrap());
    if reply[0] == 0 {
        return Err(reply[1]);
    }
    Ok((reply[1], u32_at(8), u32_at(12), u32_at(16)))
}

/// drawable, timestamp, configTimestamp, sizeID, rotation[, rate, pad].
fn screen_config_body(
    config_timestamp: u32,
    timestamp: u32,
    size_id: u16,
    rotation: u16,
    rate: Option<u16>,
) -> Vec<u8> {
    let mut body = crate::resources::ROOT_WINDOW.0.to_le_bytes().to_vec();
    body.extend_from_slice(&timestamp.to_le_bytes());
    body.extend_from_slice(&config_timestamp.to_le_bytes());
    body.extend_from_slice(&size_id.to_le_bytes());
    body.extend_from_slice(&rotation.to_le_bytes());
    if let Some(rate) = rate {
        body.extend_from_slice(&rate.to_le_bytes());
        body.extend_from_slice(&[0; 2]);
    }
    body
}

#[test]
fn randr_set_screen_config_rotates_the_first_output_as_xorg() {
    use crate::randr::{RR_ROTATE_90, RR_ROTATE_180};
    let mut state = ServerState::new();
    let output = state.randr.outputs[0].clone();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    state.randr_client_versions.insert(ClientId(1), (1, 5));
    let cts = state.randr.config_timestamp;
    let (w, h) = (state.randr.screen_width, state.randr.screen_height);
    let mm = (state.randr.width_mm, state.randr.height_mm);

    // `xrandr -o left` (tools/vng-scenarios/xrandr-orientation.sh on
    // Xorg): screen and CRTC 800×1280 from 1280×800, mm unchanged,
    // CRTC at 0,0, rotation Rotate_90.
    let reply = randr_set_screen_config(
        &mut state,
        &mut backend,
        &mut peer,
        &screen_config_body(cts, 500, 0, RR_ROTATE_90, Some(0)),
    )
    .unwrap();
    assert_eq!(reply, (0, 500, cts, crate::resources::ROOT_WINDOW.0));
    assert_eq!(
        (state.randr.screen_width, state.randr.screen_height),
        (h, w)
    );
    assert_eq!((state.randr.width_mm, state.randr.height_mm), mm);
    let applied = &state.randr.outputs[0];
    assert_eq!(applied.rotation, RR_ROTATE_90);
    assert_eq!(applied.footprint(), (output.height, output.width));
    assert!(backend.calls().iter().any(|call| matches!(
        call,
        RecordedCall::ApplyCrtcConfig {
            x: 0,
            y: 0,
            mode: Some(_),
            ..
        }
    )));

    // Xorg's statuses, in its order (same probe).
    let body =
        |cts, ts, size, rotation, rate| screen_config_body(cts, ts, size, rotation, Some(rate));
    let mut call = |state: &mut ServerState, b: Vec<u8>| {
        randr_set_screen_config(state, &mut backend, &mut peer, &b)
    };
    assert_eq!(
        call(&mut state, body(cts + 1, 0, 0, 1, 0)).map(|r| r.0),
        Ok(1)
    );
    assert_eq!(call(&mut state, body(cts, 1, 0, 1, 0)).map(|r| r.0), Ok(2));
    assert_eq!(
        call(&mut state, body(cts, 0, 1, 1, 0)),
        Err(x11::error::BAD_VALUE),
        "one mode size here"
    );
    assert_eq!(
        call(&mut state, body(cts, 0, 0, 3, 0)),
        Err(x11::error::BAD_VALUE)
    );
    assert_eq!(
        call(&mut state, body(cts, 0, 0, 0x41, 0)),
        Err(x11::error::BAD_MATCH)
    );
    assert_eq!(
        call(&mut state, body(cts, 0, 0, 1, 1)),
        Err(x11::error::BAD_VALUE)
    );
    let rate = state.randr.rr10_data().unwrap().rate;
    assert_eq!(
        call(&mut state, body(cts, 600, 0, RR_ROTATE_90, rate)).map(|r| (r.0, r.1)),
        Ok((0, 600)),
        "the current rate"
    );
    // A no-op success still moves lastSetTime (rrscreen.c:1100).
    assert_eq!(
        call(&mut state, body(cts, 700, 0, RR_ROTATE_90, 0)).map(|r| (r.0, r.1)),
        Ok((0, 700))
    );
    assert_eq!(state.randr.timestamp, 700);
    // `-o inverted`: back to the mode's own size.
    assert_eq!(
        call(&mut state, body(cts, 800, 0, RR_ROTATE_180, 0)).map(|r| r.0),
        Ok(0)
    );
    assert_eq!(
        (state.randr.screen_width, state.randr.screen_height),
        (w, h)
    );
    assert_eq!(state.randr.outputs[0].rotation, RR_ROTATE_180);
}

#[test]
fn randr_set_screen_config_size_follows_the_clients_randr_version() {
    // REQUEST_SIZE_MATCH by RRClientKnowsRates (rrscreen.c:926-933).
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let cts = state.randr.config_timestamp;
    // Explicit times: CurrentTime is the test clock, behind lastSetTime.
    let v10 = screen_config_body(cts, 1000, 0, 1, None);
    let v11 = screen_config_body(cts, 1000, 0, 1, Some(0));
    assert_eq!(
        randr_set_screen_config(&mut state, &mut backend, &mut peer, &v11),
        Err(x11::error::BAD_LENGTH),
        "no QueryVersion: the 1.0 size"
    );
    assert_eq!(
        randr_set_screen_config(&mut state, &mut backend, &mut peer, &v10).map(|r| r.0),
        Ok(0)
    );
    state.randr_client_versions.insert(ClientId(1), (1, 1));
    assert_eq!(
        randr_set_screen_config(&mut state, &mut backend, &mut peer, &v10),
        Err(x11::error::BAD_LENGTH),
        "measured: a 1.0-sized request from a 1.5 client"
    );
    assert_eq!(
        randr_set_screen_config(&mut state, &mut backend, &mut peer, &v11).map(|r| r.0),
        Ok(0)
    );
}

#[test]
fn randr_get_screen_info_lists_mode_sizes_and_rates_by_client_version() {
    use crate::randr::RR_ROTATE_90;
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let output = state.randr.outputs[0].clone();
    state.randr.outputs[0].rotation = RR_ROTATE_90;
    let get = |state: &mut ServerState, backend: &mut RecordingBackend, peer: &mut UnixStream| {
        handle_randr_request(
            state,
            backend,
            ClientId(1),
            SequenceNumber(6),
            RequestHeader {
                opcode: 128,
                data: yserver_protocol::x11::randr::RR_GET_SCREEN_INFO,
                length_units: 2,
            },
            &crate::resources::ROOT_WINDOW.0.to_le_bytes(),
        )
        .expect("GetScreenInfo");
        read_all_available(peer)
    };
    let u16_at = |b: &[u8], o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
    let rate = state.randr.rr10_data().unwrap().rate;
    let v10 = get(&mut state, &mut backend, &mut peer);
    assert_eq!(v10[1], 0x3f, "setOfRotations");
    assert_eq!(u16_at(&v10, 20), 1, "nSizes");
    assert_eq!(u16_at(&v10, 24), RR_ROTATE_90);
    assert_eq!(u16_at(&v10, 26), rate);
    assert_eq!(u16_at(&v10, 28), 2, "nrateEnts = nsize + nrefresh");
    assert_eq!(v10.len(), 32 + 8, "no rate lists for a 1.0 client");
    // The mode size, unswapped while rotated (measured).
    assert_eq!(
        (u16_at(&v10, 32), u16_at(&v10, 34)),
        (output.width, output.height)
    );

    state.randr_client_versions.insert(ClientId(1), (1, 5));
    let v11 = get(&mut state, &mut backend, &mut peer);
    assert_eq!(v11.len(), 32 + 12);
    assert_eq!((u16_at(&v11, 40), u16_at(&v11, 42)), (1, rate));
}

#[test]
fn an_asynchronous_crtc_config_applies_the_transform_pending_at_request_time() {
    let mut state = ServerState::new();
    let output = state.randr.outputs[0].clone();
    let (crtc, mode) = (output.crtc_id, output.mode_id);
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let body = randr_transform_body(crtc, rr_scale(131_072), b"");
    assert_eq!(randr_set_crtc_transform(&mut state, &mut peer, &body), None);
    backend.pending_crtc_config = Some(CrtcConfigToken(7));
    let config = randr_crtc_config_body(crtc, output.x, output.y, mode, &[output.output_id]);
    let RequestOutcome::PendingCrtcConfig(pending) =
        randr_set_crtc_config(&mut state, &mut backend, &config).unwrap()
    else {
        panic!("the enable must park");
    };
    // A new SetCrtcTransform while the enable is in flight stays pending.
    let body = randr_transform_body(crtc, rr_scale(32_768), b"");
    assert_eq!(randr_set_crtc_transform(&mut state, &mut peer, &body), None);
    complete_crtc_config(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(3),
        pending.completion,
        Ok(true),
    )
    .unwrap();
    let output = &state.randr.outputs[0];
    assert_eq!(output.current_transform.matrix, rr_scale(131_072));
    assert_eq!(output.pending_transform.matrix, rr_scale(32_768));
}

#[test]
fn randr_set_crtc_config_does_not_bound_a_crtc_by_the_screen() {
    // rrcrtc.c:1436 skips the bounds check for transform-capable CRTCs.
    let mut state = ServerState::new();
    let output = state.randr.outputs[0].clone();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let x = i16::try_from(state.randr.screen_width).unwrap();
    let body = randr_crtc_config_body(output.crtc_id, x, 0, output.mode_id, &[output.output_id]);
    randr_set_crtc_config(&mut state, &mut backend, &body).unwrap();
    let (_, success) = randr_crtc_notifies_and_success(&read_all_available(&mut peer));
    assert!(success, "past the screen edge is not BadValue");
}

/// The two 2560×1440 outputs of the muffin capture (spec, "What muffin
/// sends"): CRTC 4 at 0,0 and CRTC 6 at 2560,0, both mode 0x13.
const MUFFIN_MODE: u32 = 0x13;
const MUFFIN_MM: (u32, u32) = (597, 336);

fn muffin_output(output_id: u32, crtc_id: u32, x: i16) -> crate::randr::RandrOutput {
    crate::randr::RandrOutput {
        name: format!("DP-{output_id}"),
        output_id,
        crtc_id,
        mode_id: MUFFIN_MODE,
        connected: true,
        x,
        y: 0,
        width: 2560,
        height: 1440,
        vrefresh: 60,
        timing: None,
        mm_width: MUFFIN_MM.0,
        mm_height: MUFFIN_MM.1,
        mode_ids: vec![MUFFIN_MODE],
        num_preferred: 1,
        pending_transform: Default::default(),
        current_transform: Default::default(),
        rotation: crate::randr::RR_ROTATE_0,
    }
}

struct MuffinReplay {
    state: ServerState,
    peer: UnixStream,
    backend: RecordingBackend,
}

type Rect = (i16, i16, u16, u16);

impl MuffinReplay {
    const OUTPUT_4: u32 = 3;
    const OUTPUT_6: u32 = 5;

    fn new() -> Self {
        let mut state = ServerState::new();
        state.randr = crate::randr::RandrState::from_outputs_with_modes(
            1,
            vec![
                muffin_output(Self::OUTPUT_4, 4, 0),
                muffin_output(Self::OUTPUT_6, 6, 2560),
            ],
            vec![crate::randr::RandrMode {
                mode_id: MUFFIN_MODE,
                width: 2560,
                height: 1440,
                vrefresh: 60,
                timing: None,
            }],
        );
        let root = state.resources.window_mut(ROOT_WINDOW).unwrap();
        (root.width, root.height) = (5120, 1440);
        let peer = install_client(&mut state, 1);
        let mut backend = RecordingBackend::new();
        backend.apply_crtc_configs = true;
        Self {
            state,
            peer,
            backend,
        }
    }

    fn send(&mut self, minor: u8, body: &[u8]) -> Vec<u8> {
        handle_randr_request(
            &mut self.state,
            &mut self.backend,
            ClientId(1),
            SequenceNumber(1),
            RequestHeader {
                opcode: 128,
                data: minor,
                length_units: u32::try_from(1 + body.len() / 4).unwrap(),
            },
            body,
        )
        .expect("RANDR request");
        read_all_available(&mut self.peer)
    }

    fn set_screen_size(&mut self, w: u16, h: u16, mm_w: u32, mm_h: u32) {
        let mut body = ROOT_WINDOW.0.to_le_bytes().to_vec();
        body.extend_from_slice(&w.to_le_bytes());
        body.extend_from_slice(&h.to_le_bytes());
        body.extend_from_slice(&mm_w.to_le_bytes());
        body.extend_from_slice(&mm_h.to_le_bytes());
        let out = self.send(yserver_protocol::x11::randr::RR_SET_SCREEN_SIZE, &body);
        assert!(
            out.chunks_exact(32).all(|c| c[0] != 0),
            "SetScreenSize {w}x{h} failed: {out:02x?}"
        );
    }

    /// SetCrtcTransform then SetCrtcConfig, as muffin orders them.
    fn configure(&mut self, crtc: u32, output: u32, x: i16, scale: i32, filter: &[u8]) {
        let out = self.send(
            yserver_protocol::x11::randr::RR_SET_CRTC_TRANSFORM,
            &randr_transform_body(crtc, rr_scale(scale), filter),
        );
        assert!(out.is_empty(), "SetCrtcTransform crtc {crtc}: {out:02x?}");
        self.set_crtc_config(crtc, x, MUFFIN_MODE, &[output]);
    }

    fn set_crtc_config(&mut self, crtc: u32, x: i16, mode: u32, outputs: &[u32]) {
        let out = self.send(
            yserver_protocol::x11::randr::RR_SET_CRTC_CONFIG,
            &randr_crtc_config_body(crtc, x, 0, mode, outputs),
        );
        let reply = out.chunks_exact(32).find(|c| c[0] != 0 && c[0] < 2);
        assert_eq!(
            reply.map(|c| (c[0], c[1])),
            Some((1, 0)),
            "SetCrtcConfig crtc {crtc} succeeds: {out:02x?}"
        );
    }

    fn screen(&self) -> (u16, u16) {
        let root = self.state.resources.window(ROOT_WINDOW).unwrap();
        assert_eq!(
            (root.width, root.height),
            (
                self.state.randr.screen_width,
                self.state.randr.screen_height
            ),
            "root follows the RANDR screen"
        );
        (root.width, root.height)
    }

    /// GetCrtcInfo's `(x, y, width, height)`.
    fn crtc_info(&mut self, crtc: u32) -> Rect {
        let mut body = crtc.to_le_bytes().to_vec();
        body.extend_from_slice(&0u32.to_le_bytes());
        let r = self.send(yserver_protocol::x11::randr::RR_GET_CRTC_INFO, &body);
        assert_eq!(r[0], 1, "GetCrtcInfo reply");
        let i16_at = |o: usize| i16::from_le_bytes(r[o..o + 2].try_into().unwrap());
        let u16_at = |o: usize| u16::from_le_bytes(r[o..o + 2].try_into().unwrap());
        (i16_at(12), i16_at(14), u16_at(16), u16_at(18))
    }

    /// GetMonitors' `(x, y, width, height)` per monitor; asserts the
    /// EDID mm are untouched by any transform.
    fn monitors(&mut self) -> Vec<Rect> {
        let mut body = ROOT_WINDOW.0.to_le_bytes().to_vec();
        body.extend_from_slice(&[1, 0, 0, 0]);
        let r = self.send(yserver_protocol::x11::randr::RR_GET_MONITORS, &body);
        assert_eq!(r[0], 1, "GetMonitors reply");
        let count = u32::from_le_bytes(r[12..16].try_into().unwrap());
        let mut offset = 32;
        let mut rects = Vec::new();
        for _ in 0..count {
            let m = &r[offset..];
            let n_out = usize::from(u16::from_le_bytes(m[6..8].try_into().unwrap()));
            let i16_at = |o: usize| i16::from_le_bytes(m[o..o + 2].try_into().unwrap());
            let u16_at = |o: usize| u16::from_le_bytes(m[o..o + 2].try_into().unwrap());
            let u32_at = |o: usize| u32::from_le_bytes(m[o..o + 4].try_into().unwrap());
            assert_eq!((u32_at(16), u32_at(20)), MUFFIN_MM);
            rects.push((i16_at(8), i16_at(10), u16_at(12), u16_at(14)));
            offset += 24 + n_out * 4;
        }
        rects
    }

    fn assert_layout(&mut self, screen: (u16, u16), crtc4: Rect, crtc6: Rect, monitors: &[Rect]) {
        assert_eq!(self.screen(), screen, "screen");
        assert_eq!(self.crtc_info(4), crtc4, "GetCrtcInfo 4");
        assert_eq!(self.crtc_info(6), crtc6, "GetCrtcInfo 6");
        assert_eq!(self.monitors(), monitors, "GetMonitors");
    }
}

const MUFFIN_IDENTITY: i32 = 0x0001_0000;
const MUFFIN_1_599991: i32 = 104_857;
const MUFFIN_1_337494: i32 = 87_654;
const MUFFIN_0_5: i32 = 32_768;
const MUFFIN_0_799988: i32 = 52_428;

#[test]
fn randr_replays_muffin_scale_down_100_125_150() {
    let mut r = MuffinReplay::new();
    let left = (0, 0, 2560, 1440);
    r.assert_layout(
        (5120, 1440),
        left,
        (2560, 0, 2560, 1440),
        &[left, (2560, 0, 2560, 1440)],
    );

    // Each row of the spec table: SetScreenSize, then CRTC 4, then CRTC 6.
    for (screen, mm, scale6, crtc6) in [
        ((7680, 2880), (1355, 508), MUFFIN_2_0, (2560, 0, 5120, 2880)),
        (
            (6656, 2304),
            (1084, 375),
            MUFFIN_1_599991,
            (2560, 0, 4096, 2304),
        ),
        (
            (5984, 1926),
            (906, 292),
            MUFFIN_1_337494,
            (2560, 0, 3424, 1926),
        ),
    ] {
        let before6 = r.crtc_info(6);
        r.set_screen_size(screen.0, screen.1, mm.0, mm.1);
        // A screen may crop the previous scaled footprint for a moment.
        r.assert_layout(screen, left, before6, &[left, before6]);
        r.configure(4, MuffinReplay::OUTPUT_4, 0, MUFFIN_IDENTITY, b"fast");
        r.assert_layout(screen, left, before6, &[left, before6]);
        r.configure(6, MuffinReplay::OUTPUT_6, 2560, scale6, b"good");
        r.assert_layout(screen, left, crtc6, &[left, crtc6]);
    }
}

#[test]
fn randr_replays_muffin_scale_up_125_through_both_crtcs_off() {
    let mut r = MuffinReplay::new();
    r.set_crtc_config(4, 0, 0, &[]);
    r.assert_layout(
        (5120, 1440),
        (0, 0, 0, 0),
        (2560, 0, 2560, 1440),
        &[(2560, 0, 2560, 1440)],
    );
    r.set_crtc_config(6, 0, 0, &[]);
    r.assert_layout((5120, 1440), (0, 0, 0, 0), (0, 0, 0, 0), &[]);
    r.set_screen_size(4608, 1152, 750, 188);
    r.assert_layout((4608, 1152), (0, 0, 0, 0), (0, 0, 0, 0), &[]);
    // The mode is taller than the screen: this is what went dark.
    r.configure(4, MuffinReplay::OUTPUT_4, 0, MUFFIN_0_5, b"nearest");
    let left = (0, 0, 1280, 720);
    r.assert_layout((4608, 1152), left, (0, 0, 0, 0), &[left]);
    // CRTC 6 stays at 2560 although CRTC 4 is only 1280 wide.
    r.configure(6, MuffinReplay::OUTPUT_6, 2560, MUFFIN_0_799988, b"good");
    let right = (2560, 0, 2048, 1152);
    r.assert_layout((4608, 1152), left, right, &[left, right]);
    assert_eq!(
        crate::core_loop::run::enabled_output_bbox(&r.state),
        Some((4608, 1152))
    );
}

#[test]
fn randr_client_screen_size_survives_a_larger_transformed_bbox() {
    let mut r = MuffinReplay::new();
    r.configure(6, MuffinReplay::OUTPUT_6, 2560, MUFFIN_2_0, b"good");
    assert_eq!(
        crate::core_loop::run::enabled_output_bbox(&r.state),
        Some((7680, 2880))
    );
    let right = (2560, 0, 5120, 2880);
    r.assert_layout(
        (5120, 1440),
        (0, 0, 2560, 1440),
        right,
        &[(0, 0, 2560, 1440), right],
    );
}

/// `RRSetCrtcConfig` real handler — validates mode/output/rotation and
/// calls `apply_crtc_config`. Replaces the old no-op-accept test.
///
/// Cases:
/// The fixture has two outputs with the same connector name. Requests
/// target the second by `(crtc=5, output=4)` so routing cannot fall back
/// to a globally-ambiguous name.
///
/// 1. valid enable (mode=3, crtc=5, outputs=[4], rotation=RR_Rotate_0)
///    → reply status=0 (Success).
/// 2. disable (mode=0, crtc=2, no outputs)
///    → reply status=0.
/// 3. bad mode (555 not in output's mode_ids)
///    → X11 error BadMatch (type byte = 0).
/// 4. bad rotation (0x41: a bit outside the CRTC's `rotations` 0x3f)
///    → X11 error BadMatch (type byte = 0).
#[test]
fn randr_set_crtc_config_validates_mode_id() {
    use crate::randr::{RandrMode, RandrOutput, RandrState};

    const CLIENT_ID: u32 = 1;
    let mode_table = vec![RandrMode {
        mode_id: 3,
        width: 1920,
        height: 1080,
        vrefresh: 60,
        timing: None,
    }];
    let outputs = vec![
        RandrOutput {
            name: "test-0".to_string(),
            output_id: 1,
            crtc_id: 2,
            mode_id: 3,
            connected: true,
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            vrefresh: 60,
            timing: None,
            mm_width: 0,
            mm_height: 0,
            mode_ids: vec![3],
            num_preferred: 1,
            pending_transform: Default::default(),
            current_transform: Default::default(),
            rotation: crate::randr::RR_ROTATE_0,
        },
        // Equal connector names are legal across different DRM devices.
        // Address this second row by CRTC/XID to prove the core never
        // re-resolves the request through the ambiguous display name.
        RandrOutput {
            name: "test-0".to_string(),
            output_id: 4,
            crtc_id: 5,
            mode_id: 3,
            connected: true,
            x: 1920,
            y: 0,
            width: 1920,
            height: 1080,
            vrefresh: 60,
            timing: None,
            mm_width: 0,
            mm_height: 0,
            mode_ids: vec![3],
            num_preferred: 1,
            pending_transform: Default::default(),
            current_transform: Default::default(),
            rotation: crate::randr::RR_ROTATE_0,
        },
    ];
    let mut state = ServerState::new();
    state.randr = RandrState::from_outputs_with_modes(1, outputs, mode_table);
    let mut backend = RecordingBackend::new();
    let mut peer = install_client(&mut state, CLIENT_ID);

    // Body layout: crtc(4) ts(4) cts(4) x(2) y(2) mode(4) rotation(2)
    // pad(2) outputs(4*N).
    fn build_body(mode: u32, rotation: u16, output_ids: &[u32]) -> Vec<u8> {
        let mut b = Vec::with_capacity(24 + output_ids.len() * 4);
        b.extend_from_slice(&5u32.to_le_bytes()); // target the second same-name CRTC
        b.extend_from_slice(&0u32.to_le_bytes()); // timestamp
        b.extend_from_slice(&0u32.to_le_bytes()); // config_timestamp
        b.extend_from_slice(&0i16.to_le_bytes()); // x
        b.extend_from_slice(&0i16.to_le_bytes()); // y
        b.extend_from_slice(&mode.to_le_bytes()); // mode
        b.extend_from_slice(&rotation.to_le_bytes()); // rotation
        b.extend_from_slice(&[0u8; 2]); // pad
        for &oid in output_ids {
            b.extend_from_slice(&oid.to_le_bytes());
        }
        b
    }
    let header = yserver_protocol::x11::RequestHeader {
        opcode: 140,
        data: 21, // RR_SET_CRTC_CONFIG
        length_units: 7,
    };

    // (1) Valid enable: mode=3, rotation=RR_Rotate_0(1), outputs=[4] → Success reply.
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(CLIENT_ID),
        SequenceNumber(1),
        header,
        &build_body(3, 1, &[4]),
    )
    .expect("valid enable");

    // (2) Disable: mode=0, no outputs → Success reply.
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(CLIENT_ID),
        SequenceNumber(2),
        header,
        &build_body(0, 1, &[]),
    )
    .expect("disable");

    // (3) Bad mode: mode=555 not in output's mode_ids → BadMatch error.
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(CLIENT_ID),
        SequenceNumber(3),
        header,
        &build_body(555, 1, &[4]),
    )
    .expect("bad mode → error");

    // (4) Bad rotation: RR_Rotate_0 plus bit 6, which no CRTC
    // advertises. validate_set_crtc_config succeeds (mode 3 known), then
    // `(~crtc->rotations) & rotation` fires BadMatch (rrcrtc.c:1403).
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(CLIENT_ID),
        SequenceNumber(4),
        header,
        &build_body(3, 0x41, &[4]),
    )
    .expect("bad rotation → error");

    use std::io::Read;
    peer.set_nonblocking(true).unwrap();
    let mut wire = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        match peer.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => wire.extend_from_slice(&tmp[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }
    // All 4 responses are 32 bytes. type byte at offset 0:
    // 1 = reply, 0 = error.
    assert_eq!(wire.len(), 128, "4 × 32-byte packets");
    let type_bytes: Vec<u8> = wire.chunks_exact(32).map(|c| c[0]).collect();
    assert_eq!(type_bytes[0], 1, "valid enable → reply");
    let status_enable = wire[1]; // data byte of first reply
    assert_eq!(status_enable, 0, "valid enable → status=0 (Success)");
    assert_eq!(type_bytes[1], 1, "disable → reply");
    let status_disable = wire[32 + 1];
    assert_eq!(status_disable, 0, "disable → status=0");
    assert_eq!(type_bytes[2], 0, "bad mode → X11 error (type=0)");
    assert_eq!(type_bytes[3], 0, "bad rotation → X11 error (type=0)");

    let crtc_calls: Vec<_> = backend
        .calls()
        .into_iter()
        .filter(|call| matches!(call, RecordedCall::ApplyCrtcConfig { .. }))
        .collect();
    assert_eq!(
        crtc_calls,
        vec![
            RecordedCall::ApplyCrtcConfig {
                output_id: 4,
                connector: "test-0".to_string(),
                mode: Some(ModeSpec {
                    width: 1920,
                    height: 1080,
                    vrefresh: 60,
                }),
                x: 0,
                y: 0,
            },
            RecordedCall::ApplyCrtcConfig {
                output_id: 4,
                connector: "test-0".to_string(),
                mode: None,
                x: 0,
                y: 0,
            },
        ],
        "the second same-name output XID must be carried into backend routing",
    );

    // An asynchronous backend returns a continuation token without
    // sending a premature reply. The core loop owns parking and later
    // invokes `complete_crtc_config` with the finished result.
    let token = CrtcConfigToken(0x1234);
    backend.pending_crtc_config = Some(token);
    let outcome = handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(CLIENT_ID),
        SequenceNumber(5),
        header,
        &build_body(3, 1, &[4]),
    )
    .expect("asynchronous enable begins");
    let RequestOutcome::PendingCrtcConfig(pending) = outcome else {
        panic!("asynchronous backend must return a pending continuation");
    };
    assert_eq!(pending.token, token);
    assert_eq!(pending.completion.output_id, 4);
    assert!(
        read_all_available(&mut peer).is_empty(),
        "pending request must not receive a reply before completion"
    );
}

#[test]
fn randr_get_crtc_gamma_size_uses_backend_and_invalid_crtc_is_bad_crtc() {
    use crate::randr::{RandrOutput, RandrState};
    use yserver_protocol::x11::randr as x11randr;

    const CLIENT_ID: u32 = 1;
    let outputs = vec![RandrOutput {
        name: "DP-1".to_string(),
        output_id: 1,
        crtc_id: 2,
        mode_id: 3,
        connected: true,
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
        vrefresh: 60,
        timing: None,
        mm_width: 0,
        mm_height: 0,
        mode_ids: vec![3],
        num_preferred: 1,
        pending_transform: Default::default(),
        current_transform: Default::default(),
        rotation: crate::randr::RR_ROTATE_0,
    }];
    let mut state = ServerState::new();
    state.randr = RandrState::from_outputs(1, outputs);
    let mut backend = RecordingBackend::new();
    let mut peer = install_client(&mut state, CLIENT_ID);

    let header = RequestHeader {
        opcode: 128,
        data: x11randr::RR_GET_CRTC_GAMMA_SIZE,
        length_units: 2,
    };
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(CLIENT_ID),
        SequenceNumber(1),
        header,
        &2u32.to_le_bytes(),
    )
    .expect("valid get gamma size");
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(CLIENT_ID),
        SequenceNumber(2),
        header,
        &999u32.to_le_bytes(),
    )
    .expect("invalid get gamma size");

    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes.len(), 64, "reply + error");
    assert_eq!(bytes[0], 1, "valid query replies");
    assert_eq!(&bytes[8..10], &256u16.to_le_bytes(), "gamma size");
    assert_eq!(bytes[32], 0, "invalid crtc emits error packet");
    assert_eq!(bytes[33], RANDR_BAD_CRTC);
}

#[test]
fn randr_get_crtc_gamma_reports_seeded_identity_ramp() {
    use crate::randr::{RandrOutput, RandrState};
    use yserver_protocol::x11::randr as x11randr;

    const CLIENT_ID: u32 = 1;
    let outputs = vec![RandrOutput {
        name: "DP-1".to_string(),
        output_id: 1,
        crtc_id: 2,
        mode_id: 3,
        connected: true,
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
        vrefresh: 60,
        timing: None,
        mm_width: 0,
        mm_height: 0,
        mode_ids: vec![3],
        num_preferred: 1,
        pending_transform: Default::default(),
        current_transform: Default::default(),
        rotation: crate::randr::RR_ROTATE_0,
    }];
    let mut state = ServerState::new();
    state.randr = RandrState::from_outputs(1, outputs);
    let mut backend = RecordingBackend::new();
    let mut peer = install_client(&mut state, CLIENT_ID);

    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(CLIENT_ID),
        SequenceNumber(1),
        RequestHeader {
            opcode: 128,
            data: x11randr::RR_GET_CRTC_GAMMA,
            length_units: 2,
        },
        &2u32.to_le_bytes(),
    )
    .expect("get gamma");

    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes.len(), 32 + 256 * 3 * 2);
    assert_eq!(bytes[0], 1);
    assert_eq!(&bytes[8..10], &256u16.to_le_bytes());
    assert_eq!(&bytes[32..34], &0u16.to_le_bytes(), "red[0]");
    assert_eq!(
        &bytes[32 + 510..32 + 512],
        &65535u16.to_le_bytes(),
        "red[last]"
    );
    assert_eq!(&bytes[32 + 512..32 + 514], &0u16.to_le_bytes(), "green[0]");
    assert_eq!(
        &bytes[32 + 1022..32 + 1024],
        &65535u16.to_le_bytes(),
        "green[last]"
    );
    assert_eq!(&bytes[32 + 1024..32 + 1026], &0u16.to_le_bytes(), "blue[0]");
    assert_eq!(
        &bytes[32 + 1534..32 + 1536],
        &65535u16.to_le_bytes(),
        "blue[last]"
    );
}

#[test]
fn randr_set_crtc_gamma_validates_and_roundtrips() {
    use crate::randr::{RandrOutput, RandrState};
    use yserver_protocol::x11::randr as x11randr;

    fn set_gamma_body(crtc: u32, red: &[u16], green: &[u16], blue: &[u16]) -> Vec<u8> {
        let size = u16::try_from(red.len()).unwrap();
        let mut body = Vec::with_capacity(8 + red.len() * 6);
        body.extend_from_slice(&crtc.to_le_bytes());
        body.extend_from_slice(&size.to_le_bytes());
        body.extend_from_slice(&[0u8; 2]);
        for channel in [red, green, blue] {
            for &entry in channel {
                body.extend_from_slice(&entry.to_le_bytes());
            }
        }
        body
    }

    const CLIENT_ID: u32 = 1;
    let outputs = vec![RandrOutput {
        name: "DP-1".to_string(),
        output_id: 1,
        crtc_id: 2,
        mode_id: 3,
        connected: true,
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
        vrefresh: 60,
        timing: None,
        mm_width: 0,
        mm_height: 0,
        mode_ids: vec![3],
        num_preferred: 1,
        pending_transform: Default::default(),
        current_transform: Default::default(),
        rotation: crate::randr::RR_ROTATE_0,
    }];
    let mut state = ServerState::new();
    state.randr = RandrState::from_outputs(1, outputs);
    let mut backend = RecordingBackend::new();
    let mut peer = install_client(&mut state, CLIENT_ID);

    let red = vec![1u16; 256];
    let green = vec![2u16; 256];
    let blue = vec![3u16; 256];
    let body = set_gamma_body(2, &red, &green, &blue);
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(CLIENT_ID),
        SequenceNumber(1),
        RequestHeader {
            opcode: 128,
            data: x11randr::RR_SET_CRTC_GAMMA,
            length_units: u32::try_from(1 + body.len().div_ceil(4)).unwrap(),
        },
        &body,
    )
    .expect("set gamma");
    assert_eq!(backend.get_crtc_gamma(2), (red, green, blue));

    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(CLIENT_ID),
        SequenceNumber(2),
        RequestHeader {
            opcode: 128,
            data: x11randr::RR_GET_CRTC_GAMMA,
            length_units: 2,
        },
        &2u32.to_le_bytes(),
    )
    .expect("get gamma");

    let short_body = {
        let mut b = Vec::new();
        b.extend_from_slice(&2u32.to_le_bytes());
        b.extend_from_slice(&256u16.to_le_bytes());
        b.extend_from_slice(&[0u8; 2]);
        b
    };
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(CLIENT_ID),
        SequenceNumber(3),
        RequestHeader {
            opcode: 128,
            data: x11randr::RR_SET_CRTC_GAMMA,
            length_units: 3,
        },
        &short_body,
    )
    .expect("short set gamma");

    let red_small = vec![9u16; 128];
    let green_small = vec![8u16; 128];
    let blue_small = vec![7u16; 128];
    let mismatch_body = set_gamma_body(2, &red_small, &green_small, &blue_small);
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(CLIENT_ID),
        SequenceNumber(4),
        RequestHeader {
            opcode: 128,
            data: x11randr::RR_SET_CRTC_GAMMA,
            length_units: u32::try_from(1 + mismatch_body.len().div_ceil(4)).unwrap(),
        },
        &mismatch_body,
    )
    .expect("size mismatch set gamma");

    let invalid_body = set_gamma_body(999, &[0u16; 256], &[0u16; 256], &[0u16; 256]);
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(CLIENT_ID),
        SequenceNumber(5),
        RequestHeader {
            opcode: 128,
            data: x11randr::RR_SET_CRTC_GAMMA,
            length_units: u32::try_from(1 + invalid_body.len().div_ceil(4)).unwrap(),
        },
        &invalid_body,
    )
    .expect("invalid crtc set gamma");

    let bytes = read_all_available(&mut peer);
    assert_eq!(
        bytes.len(),
        (32 + 256 * 3 * 2) + (32 * 3),
        "one gamma reply + three errors"
    );
    let gamma_reply_len = 32 + 256 * 3 * 2;
    assert_eq!(bytes[0], 1, "get reply");
    assert_eq!(bytes[gamma_reply_len], 0, "short body => error");
    assert_eq!(bytes[gamma_reply_len + 1], x11::error::BAD_LENGTH);
    assert_eq!(bytes[gamma_reply_len + 32], 0, "size mismatch => error");
    assert_eq!(bytes[gamma_reply_len + 33], x11::error::BAD_MATCH);
    assert_eq!(bytes[gamma_reply_len + 64], 0, "invalid crtc => error");
    assert_eq!(bytes[gamma_reply_len + 65], RANDR_BAD_CRTC);
}

/// `RRSetScreenSize` with the cursor stranded off the shrunken screen
/// must trigger a `warp_pointer_root` call that clamps it inside.
/// Also verifies the basic success path and per-dimension errorValue.
#[test]
fn screen_shrink_warps_stranded_cursor_inside() {
    use crate::randr::{RandrOutput, RandrState};

    const CLIENT_ID: u32 = 1;
    // One enabled 1920×1080 output at (0,0).
    let outputs = vec![RandrOutput {
        name: "eDP-1".into(),
        output_id: 1,
        crtc_id: 2,
        mode_id: 3,
        connected: true,
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
        vrefresh: 60,
        timing: None,
        mm_width: 0,
        mm_height: 0,
        mode_ids: vec![3],
        num_preferred: 1,
        pending_transform: Default::default(),
        current_transform: Default::default(),
        rotation: crate::randr::RR_ROTATE_0,
    }];
    let mut state = ServerState::new();
    state.randr = RandrState::from_outputs(0, outputs);
    // Park the cursor at (2000, 1500) — valid under the old larger screen.
    state.pointer_root = (2000, 1500);
    let mut backend = RecordingBackend::new();
    let _peer = install_client(&mut state, CLIENT_ID);

    // Build a SetScreenSize body: window(4) width(2) height(2)
    // mm_width(4) mm_height(4).
    fn build_body(width: u16, height: u16, mm_w: u32, mm_h: u32) -> Vec<u8> {
        let mut b = Vec::with_capacity(16);
        b.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
        b.extend_from_slice(&width.to_le_bytes());
        b.extend_from_slice(&height.to_le_bytes());
        b.extend_from_slice(&mm_w.to_le_bytes());
        b.extend_from_slice(&mm_h.to_le_bytes());
        b
    }

    let header = yserver_protocol::x11::RequestHeader {
        opcode: 128, // RANDR major opcode
        data: yserver_protocol::x11::randr::RR_SET_SCREEN_SIZE,
        length_units: 5,
    };

    // Shrink to 1920×1080 (same as output — no crop) with real mm values.
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(CLIENT_ID),
        SequenceNumber(1),
        header,
        &build_body(1920, 1080, 527, 296),
    )
    .expect("valid shrink");

    // RRSetScreenSize is void — no reply on the wire.
    // The cursor at (2000, 1500) is outside 1920×1080, so a warp must fire.
    assert_eq!(
        backend.warped_to,
        Some((1919, 1079)),
        "cursor must be warped to (w-1, h-1) when stranded off-screen"
    );
    // RandrState must reflect the new logical size.
    assert_eq!(state.randr.screen_width, 1920);
    assert_eq!(state.randr.screen_height, 1080);
    assert_eq!(state.randr.width_mm, 527, "mm verbatim from client");
    assert_eq!(state.randr.height_mm, 296, "mm verbatim from client");
}

/// A compositor that renders through the Composite Overlay Window
/// typically backs it with DRI3/Present buffers. `RRSetScreenSize`
/// resizes the root and COW outside the normal `ConfigureWindow` path,
/// so it must still emit both core ConfigureNotify and
/// Present::ConfigureNotify for the materialized COW. Otherwise the
/// compositor can keep presenting its old smaller swap buffers after a
/// shrink-grow cycle, leaving the desktop in the upper-left corner while
/// input/RandR already use the full screen.
#[test]
fn screen_resize_notifies_materialized_cow_present_subscriber() {
    use crate::{
        backend::WindowHandle,
        randr::{RandrOutput, RandrState},
        resources::COMPOSITE_OVERLAY_WINDOW,
        server::PresentEventSelection,
    };
    use yserver_protocol::x11::present as x11present;

    const CLIENT_ID: u32 = 1;
    const PRESENT_EID: u32 = 0x0010_0042;

    let outputs = vec![RandrOutput {
        name: "DP-4".into(),
        output_id: 1,
        crtc_id: 2,
        mode_id: 3,
        connected: true,
        x: 0,
        y: 0,
        width: 1680,
        height: 1050,
        vrefresh: 100,
        timing: None,
        mm_width: 0,
        mm_height: 0,
        mode_ids: vec![3],
        num_preferred: 1,
        pending_transform: Default::default(),
        current_transform: Default::default(),
        rotation: crate::randr::RR_ROTATE_0,
    }];
    let mut state = ServerState::new();
    state.randr = RandrState::from_outputs(0, outputs);
    let mut peer = install_client(&mut state, CLIENT_ID);
    let mut backend = RecordingBackend::new();

    state
        .resources
        .materialize_cow_resource(WindowHandle::from_raw_for_test(COMPOSITE_OVERLAY_WINDOW.0));
    state
        .clients
        .get_mut(&CLIENT_ID)
        .unwrap()
        .event_masks
        .insert(COMPOSITE_OVERLAY_WINDOW, 0x0002_0000);
    state.present_event_selections.insert(
        PRESENT_EID,
        PresentEventSelection {
            owner: ClientId(CLIENT_ID),
            window: COMPOSITE_OVERLAY_WINDOW,
            event_mask: x11present::EVENT_MASK_CONFIGURE_NOTIFY,
        },
    );
    assert_eq!(
        crate::core_loop::fanout::subscribers_by_id(&state, COMPOSITE_OVERLAY_WINDOW, 0x0002_0000,),
        vec![ClientId(CLIENT_ID)],
        "COW StructureNotify subscription visible before resize",
    );
    assert_eq!(
        state
            .present_event_selections
            .get(&PRESENT_EID)
            .unwrap()
            .window,
        COMPOSITE_OVERLAY_WINDOW,
        "Present Configure subscription visible before resize",
    );

    let mut body = Vec::with_capacity(16);
    body.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    body.extend_from_slice(&3440u16.to_le_bytes());
    body.extend_from_slice(&1440u16.to_le_bytes());
    body.extend_from_slice(&910u32.to_le_bytes());
    body.extend_from_slice(&381u32.to_le_bytes());

    let header = yserver_protocol::x11::RequestHeader {
        opcode: 128,
        data: yserver_protocol::x11::randr::RR_SET_SCREEN_SIZE,
        length_units: 5,
    };
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(CLIENT_ID),
        SequenceNumber(1),
        header,
        &body,
    )
    .expect("RRSetScreenSize grow");

    assert_eq!(state.randr.screen_width, 3440, "RandR width updated");
    assert_eq!(state.randr.screen_height, 1440, "RandR height updated");
    let cow = state
        .resources
        .window(COMPOSITE_OVERLAY_WINDOW)
        .expect("COW still materialized");
    assert_eq!(cow.width, 3440, "COW width updated");
    assert_eq!(cow.height, 1440, "COW height updated");
    assert_eq!(
        crate::core_loop::fanout::subscribers_by_id(&state, COMPOSITE_OVERLAY_WINDOW, 0x0002_0000,),
        vec![ClientId(CLIENT_ID)],
        "COW StructureNotify subscription still visible after resize",
    );
    assert_eq!(
        state
            .present_event_selections
            .get(&PRESENT_EID)
            .unwrap()
            .window,
        COMPOSITE_OVERLAY_WINDOW,
        "Present Configure subscription still visible after resize",
    );
    assert_eq!(
        state.clients.get(&CLIENT_ID).unwrap().outbound.len(),
        0,
        "small COW resize notifications should not be stuck in outbound",
    );

    let bytes = read_all_available(&mut peer);
    assert_eq!(
        bytes.len(),
        32 + 40,
        "COW subscriber should receive core ConfigureNotify plus \
             Present::ConfigureNotify only; got {bytes:?}",
    );

    let core = &bytes[..32];
    assert_eq!(core[0] & 0x7f, 22, "first event must be ConfigureNotify");
    assert_eq!(
        u32::from_le_bytes(core[4..8].try_into().unwrap()),
        COMPOSITE_OVERLAY_WINDOW.0,
        "ConfigureNotify event window must be COW",
    );
    assert_eq!(
        u32::from_le_bytes(core[8..12].try_into().unwrap()),
        COMPOSITE_OVERLAY_WINDOW.0,
        "ConfigureNotify target window must be COW",
    );
    assert_eq!(u16::from_le_bytes(core[20..22].try_into().unwrap()), 3440);
    assert_eq!(u16::from_le_bytes(core[22..24].try_into().unwrap()), 1440);

    let present = &bytes[32..];
    assert_eq!(present[0], 35, "second event must be GenericEvent");
    assert_eq!(present[1], 145, "GenericEvent extension must be PRESENT");
    assert_eq!(
        u16::from_le_bytes(present[8..10].try_into().unwrap()),
        u16::from(x11present::EVENT_CONFIGURE_NOTIFY),
    );
    assert_eq!(
        u32::from_le_bytes(present[12..16].try_into().unwrap()),
        PRESENT_EID,
    );
    assert_eq!(
        u32::from_le_bytes(present[16..20].try_into().unwrap()),
        COMPOSITE_OVERLAY_WINDOW.0,
    );
    assert_eq!(
        u16::from_le_bytes(present[24..26].try_into().unwrap()),
        3440,
        "Present ConfigureNotify width must track the grown COW",
    );
    assert_eq!(
        u16::from_le_bytes(present[26..28].try_into().unwrap()),
        1440,
        "Present ConfigureNotify height must track the grown COW",
    );
    assert_eq!(
        u16::from_le_bytes(present[32..34].try_into().unwrap()),
        3440,
        "Present pixmap width must ask Mesa/KWin to reallocate full-size buffers",
    );
    assert_eq!(
        u16::from_le_bytes(present[34..36].try_into().unwrap()),
        1440,
        "Present pixmap height must ask Mesa/KWin to reallocate full-size buffers",
    );

    crate::core_loop::run::emit_screen_resize_window_notifications_if_outputs_caught_up(
        &mut state,
        Some((1680, 1050)),
    );
    assert!(
        read_all_available(&mut peer).is_empty(),
        "a stale output bbox must not re-emit full-size configure notifications",
    );

    state.randr.outputs[0].width = 3440;
    state.randr.outputs[0].height = 1440;
    crate::core_loop::run::emit_screen_resize_window_notifications_if_outputs_caught_up(
        &mut state,
        Some((1680, 1050)),
    );
    let caught_up = read_all_available(&mut peer);
    assert_eq!(
        caught_up.len(),
        32 + 40,
        "matching output bbox must re-emit core and Present COW configure notifications",
    );
    assert_eq!(caught_up[0] & 0x7f, 22);
    assert_eq!(caught_up[32], 35);
    assert_eq!(
        u16::from_le_bytes(caught_up[32 + 32..32 + 34].try_into().unwrap()),
        3440,
    );
    assert_eq!(
        u16::from_le_bytes(caught_up[32 + 34..32 + 36].try_into().unwrap()),
        1440,
    );

    crate::core_loop::run::emit_screen_resize_window_notifications_if_outputs_caught_up(
        &mut state,
        Some((3440, 1440)),
    );
    assert!(
        read_all_available(&mut peer).is_empty(),
        "an unchanged output bbox must not re-emit configure notifications",
    );
}

/// `RRSetScreenSize` validation: crop check returns BadMatch; zero-mm
/// returns BadValue; out-of-range width/height each report the offending
/// dimension as errorValue.
#[test]
fn screen_set_size_validation_errors() {
    use crate::randr::{RandrOutput, RandrState};
    use yserver_protocol::x11::randr as x11randr;

    const CLIENT_ID: u32 = 1;
    let outputs = vec![RandrOutput {
        name: "eDP-1".into(),
        output_id: 1,
        crtc_id: 2,
        mode_id: 3,
        connected: true,
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
        vrefresh: 60,
        timing: None,
        mm_width: 0,
        mm_height: 0,
        mode_ids: vec![3],
        num_preferred: 1,
        pending_transform: Default::default(),
        current_transform: Default::default(),
        rotation: crate::randr::RR_ROTATE_0,
    }];
    let mut state = ServerState::new();
    state.randr = RandrState::from_outputs(0, outputs);
    let mut backend = RecordingBackend::new();
    let mut peer = install_client(&mut state, CLIENT_ID);

    fn build_body(width: u16, height: u16, mm_w: u32, mm_h: u32) -> Vec<u8> {
        let mut b = Vec::with_capacity(16);
        b.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
        b.extend_from_slice(&width.to_le_bytes());
        b.extend_from_slice(&height.to_le_bytes());
        b.extend_from_slice(&mm_w.to_le_bytes());
        b.extend_from_slice(&mm_h.to_le_bytes());
        b
    }

    let header = yserver_protocol::x11::RequestHeader {
        opcode: 128,
        data: x11randr::RR_SET_SCREEN_SIZE,
        length_units: 5,
    };

    // width=0 → out-of-range (< min=1) → BadValue with errorValue=0.
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(CLIENT_ID),
        SequenceNumber(1),
        header,
        &build_body(0, 1080, 527, 296),
    )
    .unwrap();
    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes[1], x11::error::BAD_VALUE, "width=0 → BadValue");
    // errorValue for the offending width is 0 (the bad width).
    let ev = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    assert_eq!(ev, 0u32, "errorValue = offending width");

    // height=0 → out-of-range → BadValue with errorValue=height.
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(CLIENT_ID),
        SequenceNumber(2),
        header,
        &build_body(1920, 0, 527, 296),
    )
    .unwrap();
    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes[1], x11::error::BAD_VALUE, "height=0 → BadValue");
    let ev = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    assert_eq!(ev, 0u32, "errorValue = offending height");

    // Crop: 1280×720 crops the 1920×1080 output → BadMatch.
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(CLIENT_ID),
        SequenceNumber(3),
        header,
        &build_body(1280, 720, 527, 296),
    )
    .unwrap();
    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes[1], x11::error::BAD_MATCH, "crop → BadMatch");

    // mm_width=0 → BadValue.
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(CLIENT_ID),
        SequenceNumber(4),
        header,
        &build_body(1920, 1080, 0, 296),
    )
    .unwrap();
    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes[1], x11::error::BAD_VALUE, "mm_width=0 → BadValue");
}

/// `RRSetScreenSize` sends ScreenChangeNotify only when pixel size or mm
/// differ from the current ones (`RRScreenSizeNotify`, rrscreen.c); a
/// repeated size and a rejected crop send nothing, as measured on Xorg
/// 21.1.24 by the xrandr-rotate vng scenario.
#[test]
fn screen_set_size_notifies_only_on_change() {
    use crate::randr::{RandrOutput, RandrState};
    use yserver_protocol::x11::randr as x11randr;

    const CLIENT_ID: u32 = 1;
    let outputs = vec![RandrOutput {
        name: "Virtual-1".into(),
        output_id: 1,
        crtc_id: 2,
        mode_id: 3,
        connected: true,
        x: 0,
        y: 0,
        width: 1280,
        height: 800,
        vrefresh: 60,
        timing: None,
        mm_width: 0,
        mm_height: 0,
        mode_ids: vec![3],
        num_preferred: 1,
        pending_transform: Default::default(),
        current_transform: Default::default(),
        rotation: crate::randr::RR_ROTATE_0,
    }];
    let mut state = ServerState::new();
    state.randr = RandrState::from_outputs(0, outputs);
    let mut backend = RecordingBackend::new();
    let mut peer = install_client(&mut state, CLIENT_ID);
    state.randr_select_masks.insert(
        (CLIENT_ID, ROOT_WINDOW),
        x11randr::NOTIFY_MASK_SCREEN_CHANGE,
    );

    let header = yserver_protocol::x11::RequestHeader {
        opcode: 128,
        data: x11randr::RR_SET_SCREEN_SIZE,
        length_units: 5,
    };
    let mut set = |state: &mut ServerState, seq: u16, w: u16, h: u16, mm_w: u32, mm_h: u32| {
        let mut b = Vec::with_capacity(16);
        b.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
        b.extend_from_slice(&w.to_le_bytes());
        b.extend_from_slice(&h.to_le_bytes());
        b.extend_from_slice(&mm_w.to_le_bytes());
        b.extend_from_slice(&mm_h.to_le_bytes());
        handle_randr_request(
            state,
            &mut backend,
            ClientId(CLIENT_ID),
            SequenceNumber(seq),
            header,
            &b,
        )
        .unwrap();
    };
    // RRScreenChangeNotify is RANDR event base 89 + 0.
    let screen_changes =
        |bytes: &[u8]| bytes.chunks_exact(32).filter(|e| e[0] & 0x7f == 89).count();

    set(&mut state, 1, 1280, 800, 300, 200);
    assert_eq!(
        screen_changes(&read_all_available(&mut peer)),
        1,
        "mm changed"
    );
    set(&mut state, 2, 1280, 800, 300, 200);
    assert_eq!(
        screen_changes(&read_all_available(&mut peer)),
        0,
        "unchanged"
    );
    set(&mut state, 3, 1279, 800, 300, 200);
    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes[1], x11::error::BAD_MATCH, "crop → BadMatch");
    assert_eq!(screen_changes(&bytes), 0, "rejected crop");
    set(&mut state, 4, 4000, 4000, 300, 200);
    assert_eq!(screen_changes(&read_all_available(&mut peer)), 1, "grown");
    set(&mut state, 5, 1280, 800, 300, 200);
    assert_eq!(
        screen_changes(&read_all_available(&mut peer)),
        1,
        "restored"
    );
}
