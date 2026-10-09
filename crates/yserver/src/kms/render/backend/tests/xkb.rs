use super::*;

/// A big-endian client's XkbUseExtension through the real KMS backend
/// gets Xvfb's big-endian "unsupported" answer (supported=False,
/// sequence and server version 1.0 in big-endian:
/// `01000002 00000000 00010000 00…`) — yserver's XKB is
/// little-endian only, and Xorg's way to refuse a client is
/// supported=False. The backend's own reply is little-endian.
#[test]
fn xkb_use_extension_from_a_big_endian_client_is_refused_in_its_byte_order() {
    use std::{
        collections::{HashMap, HashSet, VecDeque},
        io::Read,
        os::unix::net::UnixStream,
        sync::{Arc, Mutex, atomic::AtomicU16},
    };
    use yserver_core::{core_loop::process_request, server::ClientState};
    use yserver_protocol::x11::{ClientByteOrder, ClientId, RequestHeader, SequenceNumber};

    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    let (mut peer, writer) = UnixStream::pair().unwrap();
    writer.set_nonblocking(true).unwrap();
    state.clients.insert(
        7,
        ClientState {
            writer: Arc::new(Mutex::new(yserver_core::transport::Transport::Unix(writer))),
            byte_order: ClientByteOrder::BigEndian,
            last_sequence: Arc::new(AtomicU16::new(0)),
            resource_id_base: 0,
            resource_id_mask: 0,
            event_masks: HashMap::new(),
            save_set: HashSet::new(),
            big_requests_enabled: false,
            xi2_masks: HashMap::new(),
            xi1_event_classes: HashSet::new(),
            xi1_window_event_classes: HashMap::new(),
            outbound: VecDeque::new(),
            watching_writable: false,
            write_failed: false,
            focused_window: yserver_core::resources::ROOT_WINDOW,
            reader_control: None,
            is_local: true,
            fd_passing: true,
        },
    );
    let xkb_major = backend.xkb_opcode().expect("KMS advertises XKB");
    process_request::process_request(
        &mut state,
        &mut backend as &mut dyn Backend,
        ClientId(7),
        SequenceNumber(2),
        RequestHeader {
            opcode: xkb_major,
            data: 0,
            length_units: 2,
        },
        // wantedMajor=1, wantedMinor=0 as a big-endian client sends it.
        &[0, 1, 0, 0],
        None,
    )
    .expect("XkbUseExtension");
    let mut reply = [0u8; 32];
    peer.read_exact(&mut reply).unwrap();
    let mut expected = [0u8; 32];
    expected[..10].copy_from_slice(&[1, 0, 0, 2, 0, 0, 0, 0, 0, 1]);
    assert_eq!(reply, expected);
}

/// Routing regression guard: minor 13 is GetIndicatorMap (clients send
/// it 8×), minor 22 is ListComponents. FU4 wired the 416-byte
/// IndicatorMap reply to 22 by mistake and stubbed the real opcode 13.
/// xkb_proxy(13) must return the 416-byte IndicatorMap; xkb_proxy(22)
/// must NOT (an IndicatorMap-shaped reply at 22 is wrong).
#[test]
fn xkb_proxy_routes_indicator_map_to_minor_13() {
    let mut backend = KmsBackend::for_tests();
    let mut intern = |_name: &str| 1u32;

    let r13 = backend
        .xkb_proxy(None, 13, &[], &mut intern)
        .expect("xkb_proxy ok")
        .expect("minor 13 returns a reply");
    assert_eq!(
        r13.len(),
        416,
        "minor 13 (GetIndicatorMap) must be the 32 + 32*12 IndicatorMap reply"
    );

    let r22 = backend
        .xkb_proxy(None, 22, &[], &mut intern)
        .expect("xkb_proxy ok");
    // minor 22 (ListComponents) must not emit an IndicatorMap-shaped reply.
    assert_ne!(
        r22.as_ref().map(Vec::len),
        Some(416),
        "minor 22 (ListComponents) must not be the IndicatorMap reply"
    );
}

#[test]
fn xkb_proxy_get_map_respects_client_requested_parts() {
    let mut backend = KmsBackend::for_tests();
    let mut intern = |_name: &str| 1u32;
    let mut body = [0u8; 20];
    body[0..2].copy_from_slice(&0x0100_u16.to_le_bytes()); // UseCoreKbd
    body[2..4].copy_from_slice(&0x0003_u16.to_le_bytes()); // KeyTypes|KeySyms

    let reply = backend
        .xkb_proxy(None, 8, &body, &mut intern)
        .expect("xkb_proxy ok")
        .expect("minor 8 returns a GetMap reply");

    assert_eq!(
        u16::from_le_bytes([reply[12], reply[13]]),
        0x0003,
        "GetMap present must be limited to the requested KeyTypes|KeySyms"
    );
    assert_eq!(u16::from_le_bytes([reply[22], reply[23]]), 0);
    assert_eq!(reply[24], 0, "unrequested KeyActions must be absent");
    assert_eq!(reply[33], 0, "unrequested ModifierMap must be absent");
    assert_eq!(
        u16::from_le_bytes([reply[38], reply[39]]),
        0,
        "unrequested VirtualMods must be absent"
    );
}

#[test]
fn backend_set_keymap_rmlvo_reports_range() {
    let mut backend = KmsBackend::for_tests();
    let range = backend.set_keymap_rmlvo("evdev", "pc105", "de", "", None);
    // Assert the robust half concretely and the xkb-data-derived half
    // structurally: max is a hard clamp to 255; min is the evdev floor
    // (8 offset) and is 9 on current xkb-data, but assert a plausible
    // range so a future xkb-data shift doesn't read as a logic break.
    let (min, max) = range.expect("de map compiles");
    assert_eq!(max, 255, "evdev keycode ceiling is clamped to 255");
    assert!(
        (8..=9).contains(&min),
        "evdev min keycode (8 offset), got {min}"
    );
    // Re-applying the same RMLVO is a no-op (None = no change).
    let again = backend.set_keymap_rmlvo("evdev", "pc105", "de", "", None);
    assert_eq!(again, None);
}

#[test]
fn load_keymap_by_components_multigroup() {
    use yserver_core::backend::{Backend, KeymapLoad};

    let mut backend = KmsBackend::for_tests();
    // The German switch string from the capture.
    let r = backend.load_keymap_by_components("pc+us+de:2+us:3+inet(evdev)");
    match r {
        KeymapLoad::Loaded { changed, .. } => assert!(changed, "first load is a change"),
        KeymapLoad::Failed => panic!("should load"),
    }
    // keycode 29 group 1 is now German `z`
    assert_eq!(
        backend
            .core
            .xkb_keymap
            .0
            .key_get_syms_by_level(xkbcommon::xkb::Keycode::new(29), 1, 0)
            .first()
            .map(|s| s.raw()),
        Some(0x7a)
    );
    // A second identical load is still Loaded, but changed=false.
    let r2 = backend.load_keymap_by_components("pc+us+de:2+us:3+inet(evdev)");
    assert!(matches!(r2, KeymapLoad::Loaded { changed: false, .. }));
    // An unparseable symbols string fails closed -> keymap unchanged.
    let r3 = backend.load_keymap_by_components("pc+wat_xyz_unknown:2+inet(evdev)");
    assert_eq!(r3, KeymapLoad::Failed);
}

#[test]
fn load_keymap_by_components_preserves_level3_chooser_option() {
    use xkbcommon::xkb::{KeyDirection, Keycode};
    use yserver_core::backend::{Backend, KeymapLoad};

    let mut backend = KmsBackend::for_tests();
    let r = backend.load_keymap_by_components(
        "pc+us+be:2+us:3+inet(evdev)+capslock(none)+level3(ralt_switch)",
    );
    assert!(matches!(r, KeymapLoad::Loaded { .. }), "loads, got {r:?}");

    // LOAD-BEARING, machine-independent guard: the `level3(ralt_switch)`
    // chooser partial from the symbols string must survive into the
    // recompiled keymap's RMLVO options as `lv3:ralt_switch` (was dropped
    // before the fix → `options: None`). This is what makes AltGr bind to
    // Mod5 on systems whose xkb-data does NOT default the chooser (the
    // real-HW failure on `silence`). NB: the downstream €-resolution itself
    // is xkb-DATA-dependent — on a machine whose `pc105` default already
    // binds RAlt→Mod5 it resolves with or without the option, so asserting
    // € here would be vacuous; we assert the option survived instead.
    let opts = backend.core.xkb_rmlvo.options.as_deref().unwrap_or("");
    assert!(
        opts.split(',').any(|o| o == "lv3:ralt_switch"),
        "lv3:ralt_switch must be preserved into rmlvo.options, got {opts:?}"
    );
    assert!(
        opts.split(',').any(|o| o == "caps:none"),
        "caps:none must be preserved too, got {opts:?}"
    );

    // Best-effort downstream check (informational; may pass regardless of
    // the option on xkb-data that defaults the chooser): on the `be` group,
    // RAlt+e should reach EuroSign.
    let st = &mut backend.core.xkb_state.0;
    st.update_mask(0, 0, 0, 0, 0, 1); // lock layout 1 (be)
    st.update_key(Keycode::new(108), KeyDirection::Down); // RAlt
    assert_eq!(
        st.key_get_one_sym(Keycode::new(26)).raw(),
        0x20ac,
        "AltGr+e on the be group resolves to EuroSign"
    );
}

#[test]
fn xkb_get_kbd_by_name_parses_capture_loads_and_notifies() {
    use yserver_core::backend::Backend;

    // Reconstruct the EXACT GetKbdByName request body from
    // cinnamon-xorg.xtrace:6201 (after the 4-byte XKB request header the
    // core loop strips): deviceSpec(2) need(2) want(2) load(1) pad(1),
    // then CARD8-length-prefixed component strings in the order
    // keymap, keycodes, types, compat, symbols, geometry.
    let mut body: Vec<u8> = Vec::new();
    body.extend_from_slice(&0x0100u16.to_le_bytes()); // deviceSpec
    body.extend_from_slice(&0x00bfu16.to_le_bytes()); // need
    body.extend_from_slice(&0x00ffu16.to_le_bytes()); // want
    body.push(1); // load = TRUE
    body.push(0); // pad
    for s in [
        "",                            // keymap (empty)
        "evdev+aliases(qwerty)",       // keycodes
        "complete",                    // types
        "complete",                    // compat
        "pc+us+de:2+us:3+inet(evdev)", // symbols
        "pc(pc105)",                   // geometry
    ] {
        body.push(u8::try_from(s.len()).unwrap());
        body.extend_from_slice(s.as_bytes());
    }

    let mut backend = KmsBackend::for_tests();
    let mut next_atom = 1u32;
    let (reply, notify) = backend
        .xkb_get_kbd_by_name(&body, &mut |_name| {
            let a = next_atom;
            next_atom += 1;
            a
        })
        .expect("real backend builds a GetKbdByName reply");

    // Header: loaded BOOL, found/reported MASKS (captured 0x7f / 0xff).
    assert_eq!(reply[10], 1, "loaded = TRUE");
    assert_eq!(
        u16::from_le_bytes([reply[12], reply[13]]),
        0x007f,
        "found mask matches capture"
    );
    assert_eq!(
        u16::from_le_bytes([reply[14], reply[15]]),
        0x00ff,
        "reported mask matches capture"
    );
    // The symbols component actually loaded the German group: keycode 29
    // group 1 level 0 is `z` (0x7a) under us+de.
    assert_eq!(
        backend
            .core
            .xkb_keymap
            .0
            .key_get_syms_by_level(xkbcommon::xkb::Keycode::new(29), 1, 0)
            .first()
            .map(|s| s.raw()),
        Some(0x7a),
        "symbols component loaded the de group"
    );
    // First load is a change -> NewKeyboardNotify with changed=0x0003.
    let info = notify.expect("a changing load broadcasts NewKeyboardNotify");
    assert_eq!(info.changed, 0x0003, "changed = Keycodes|Geometry");
    assert!(
        info.min_keycode <= info.max_keycode,
        "new keycode range is sane"
    );

    // A second identical request still succeeds (loaded=1) but does NOT
    // broadcast (changed=false -> no NKN churn).
    let (reply2, notify2) = backend
        .xkb_get_kbd_by_name(&body, &mut |_| 1u32)
        .expect("reply on reload");
    assert_eq!(reply2[10], 1, "reload still loaded = TRUE");
    assert!(
        notify2.is_none(),
        "unchanged reload must not broadcast NewKeyboardNotify"
    );
}

/// One case of `testdata/xorg-change-keyboard-mapping.txt`.
struct KbdMapCase {
    name: String,
    layout: String,
    options: Option<String>,
    first: u8,
    kpk: u8,
    count: u8,
    syms: Vec<u32>,
    /// Further `(first, kpk, count, syms)` requests sent before the read-back.
    more: Vec<(u8, u8, u8, Vec<u32>)>,
    width: u8,
    notify: Option<(u8, u8)>,
    rows: std::collections::BTreeMap<u8, Vec<u32>>,
}

/// Parse the Xvfb capture into round-trip cases and `(first, kpk, count, nsyms, outcome)` edge lines.
/// `(first, kpk, count, nsyms, outcome)` of one invalid/edge request line.
type KbdMapEdge = (u8, u8, u8, usize, String);

fn parse_kbd_map_fixture(text: &str) -> (Vec<KbdMapCase>, Vec<KbdMapEdge>) {
    let field = |line: &str, key: &str| -> String {
        line.split(' ')
            .find_map(|t| t.strip_prefix(&format!("{key}=")).map(str::to_owned))
            .unwrap_or_default()
    };
    let hex_list = |v: &str| -> Vec<u32> {
        if v == "-" {
            Vec::new()
        } else {
            v.split(',')
                .map(|h| u32::from_str_radix(h, 16).unwrap())
                .collect()
        }
    };
    let (mut cases, mut edges): (Vec<KbdMapCase>, Vec<_>) = (Vec::new(), Vec::new());
    for line in text.lines() {
        if line.starts_with("## + ") {
            cases.last_mut().unwrap().more.push((
                field(line, "first").parse().unwrap(),
                field(line, "kpk").parse().unwrap(),
                field(line, "count").parse().unwrap(),
                hex_list(&field(line, "syms")),
            ));
        } else if let Some(rest) = line.strip_prefix("## ") {
            let opts = field(line, "options");
            cases.push(KbdMapCase {
                name: rest.split(' ').next().unwrap().to_owned(),
                layout: field(line, "layout"),
                options: (opts != "-").then_some(opts),
                first: field(line, "first").parse().unwrap(),
                kpk: field(line, "kpk").parse().unwrap(),
                count: field(line, "count").parse().unwrap(),
                syms: hex_list(&field(line, "syms")),
                more: Vec::new(),
                width: 0,
                notify: None,
                rows: std::collections::BTreeMap::new(),
            });
        } else if let Some(rest) = line.strip_prefix("! ") {
            let (req, outcome) = rest.split_once(" -> ").unwrap();
            edges.push((
                field(req, "first").parse().unwrap(),
                field(req, "kpk").parse().unwrap(),
                field(req, "count").parse().unwrap(),
                field(req, "nsyms").parse().unwrap(),
                outcome.to_owned(),
            ));
        } else if let Some(w) = line.strip_prefix("# keysyms_per_keycode=") {
            cases.last_mut().unwrap().width = w.parse().unwrap();
        } else if line.starts_with("# notify") {
            cases.last_mut().unwrap().notify = Some((
                field(line, "first").parse().unwrap(),
                field(line, "count").parse().unwrap(),
            ));
        } else if !line.starts_with('#') && !line.is_empty() {
            let mut it = line.split(' ');
            let kc: u8 = it.next().unwrap().parse().unwrap();
            let rest: Vec<&str> = it.collect();
            let row = if rest == ["cleared"] {
                Vec::new()
            } else {
                rest.iter()
                    .map(|h| u32::from_str_radix(h, 16).unwrap())
                    .collect()
            };
            cases.last_mut().unwrap().rows.insert(kc, row);
        }
    }
    (cases, edges)
}

/// GetKeyboardMapping(8, 248) as `(keysyms_per_keycode, keysyms)`.
fn kbd_map_get(
    state: &mut yserver_core::server::ServerState,
    backend: &mut KmsBackend,
    peer: &mut std::os::unix::net::UnixStream,
) -> (u8, Vec<u32>) {
    kbd_map_request(state, backend, 101, 0, &[8, 248, 0, 0]);
    let r = kbd_map_drain(peer);
    assert_eq!(r[0], 1, "GetKeyboardMapping reply");
    let syms = r[32..]
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    (r[1], syms)
}

/// Golden (Xvfb 21.1.24): core and XI key maps after ChangeKeyboardMapping, plus MappingNotify.
#[test]
fn change_keyboard_mapping_round_trips_as_xorg() {
    let (cases, _) = parse_kbd_map_fixture(include_str!(
        "../../../testdata/xorg-change-keyboard-mapping.txt"
    ));
    assert_eq!(cases.len(), 26, "fixture parsed");
    let mut failures = Vec::new();
    for case in &cases {
        let mut backend = kbd_map_backend(&case.layout, case.options.as_deref());
        let mut state = yserver_core::server::ServerState::new();
        let mut peer = kbd_map_client(&mut state);
        let (w0, before) = kbd_map_get(&mut state, &mut backend, &mut peer);
        let body = change_kbd_map_body(case.first, case.kpk, &case.syms);
        kbd_map_request(&mut state, &mut backend, 100, case.count, &body);
        let mut ev = kbd_map_drain(&mut peer);
        for (first, kpk, count, syms) in &case.more {
            kbd_map_request(
                &mut state,
                &mut backend,
                100,
                *count,
                &change_kbd_map_body(*first, *kpk, syms),
            );
            ev = kbd_map_drain(&mut peer);
        }
        let notify = (ev.len() >= 32 && ev[0] & 0x7f == 34 && ev[4] == 1).then(|| (ev[5], ev[6]));
        if notify != case.notify {
            failures.push(format!(
                "{}: MappingNotify ours {notify:?} xorg {:?}",
                case.name, case.notify
            ));
        }
        let (w, after) = kbd_map_get(&mut state, &mut backend, &mut peer);
        if w != case.width {
            failures.push(format!(
                "{}: keysyms_per_keycode ours {w} xorg {}",
                case.name, case.width
            ));
            continue;
        }
        for (i, row) in after.chunks(usize::from(w)).enumerate() {
            let kc = u8::try_from(8 + i).unwrap();
            let want = match case.rows.get(&kc) {
                Some(r) if r.is_empty() => vec![0; usize::from(w)],
                Some(r) => r.clone(),
                // Unlisted rows are unchanged; across a width change only all-NoSymbol rows are unlisted.
                None => {
                    let old = &before[i * usize::from(w0)..(i + 1) * usize::from(w0)];
                    let mut r = old.to_vec();
                    r.resize(usize::from(w), 0);
                    r
                }
            };
            if row != want.as_slice() {
                failures.push(format!(
                    "{}: keycode {kc}: ours {row:x?} xorg {want:x?}",
                    case.name
                ));
            }
        }
        // XI GetDeviceKeyMapping on the master keyboard reads the same map.
        kbd_map_request(&mut state, &mut backend, 137, 24, &[3, 8, 248, 0]);
        let xi = kbd_map_drain(&mut peer);
        let xi_syms: Vec<u32> = xi[32..]
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        if (xi[8], xi_syms) != (w, after) {
            failures.push(format!(
                "{}: XI GetDeviceKeyMapping differs from core",
                case.name
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// XI ChangeDeviceKeyMapping goes through the same conversion (Xorg: both reach XkbApplyMappingChange).
#[test]
fn xi_change_device_key_mapping_converts_as_core() {
    let (cases, _) = parse_kbd_map_fixture(include_str!(
        "../../../testdata/xorg-change-keyboard-mapping.txt"
    ));
    let case = cases.iter().find(|c| c.name == "two-sterling").unwrap();
    let mut backend = kbd_map_backend(&case.layout, None);
    let mut state = yserver_core::server::ServerState::new();
    let mut peer = kbd_map_client(&mut state);
    let mut body = vec![3, case.first, case.kpk, case.count];
    for s in &case.syms {
        body.extend_from_slice(&s.to_le_bytes());
    }
    kbd_map_request(&mut state, &mut backend, 137, 25, &body);
    let _ = kbd_map_drain(&mut peer);
    let (w, after) = kbd_map_get(&mut state, &mut backend, &mut peer);
    let i = usize::from(case.first - 8) * usize::from(w);
    assert_eq!(w, case.width);
    assert_eq!(
        &after[i..i + usize::from(w)],
        case.rows[&case.first].as_slice()
    );
}

/// Golden (Xvfb): ProcChangeKeyboardMapping's BadLength/BadValue rules and error values.
#[test]
fn change_keyboard_mapping_errors_as_xorg() {
    let (_, edges) = parse_kbd_map_fixture(include_str!(
        "../../../testdata/xorg-change-keyboard-mapping.txt"
    ));
    assert_eq!(edges.len(), 9, "fixture parsed");
    let mut failures = Vec::new();
    for (first, kpk, count, nsyms, want) in edges {
        let mut backend = kbd_map_backend("gb", None);
        let mut state = yserver_core::server::ServerState::new();
        let mut peer = kbd_map_client(&mut state);
        let body = change_kbd_map_body(first, kpk, &vec![0x61; nsyms]);
        kbd_map_request(&mut state, &mut backend, 100, count, &body);
        let r = kbd_map_drain(&mut peer);
        let got = if r.first() == Some(&0) {
            let value = u32::from_le_bytes([r[4], r[5], r[6], r[7]]);
            format!("error={} value={value}", r[1])
        } else if r.len() >= 32 && r[0] & 0x7f == 34 {
            format!("ok notify={},{}", r[5], r[6])
        } else {
            "ok notify=none".to_owned()
        };
        if got != want {
            failures.push(format!(
                "first={first} kpk={kpk} count={count} nsyms={nsyms}: ours {got} xorg {want}"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// One ChangeKeyboardMapping of a golden case: the request, Xorg's
/// result, the events and the GetMap/GetControls delta.
#[derive(Default)]
struct XkbCkmStep {
    first: u8,
    kpk: u8,
    count: u8,
    syms: Vec<u32>,
    ok: bool,
    /// Raw events on the XKB listener: the core keyboard's (dev 3) XKB
    /// events and its core events, in arrival order.
    listener: Vec<Vec<u8>>,
    /// Raw events on the plain (non-XKB) connection.
    plain: Vec<Vec<u8>>,
    /// `(keycode, before, after)` per-key repeat changes (GetControls).
    repeats: Vec<(u8, u8, u8)>,
    before: std::collections::BTreeMap<u8, XkbKeyRow>,
    after: std::collections::BTreeMap<u8, XkbKeyRow>,
}

struct XkbCkmCase {
    name: String,
    layout: String,
    options: Option<String>,
    steps: Vec<XkbCkmStep>,
}

fn parse_xkb_ckm_golden(text: &str) -> Vec<XkbCkmCase> {
    let field = |line: &str, key: &str| -> Option<String> {
        line.split(' ')
            .find_map(|t| t.strip_prefix(&format!("{key}=")).map(str::to_owned))
    };
    let raw = |line: &str| -> Vec<u8> {
        let h = field(line, "raw").unwrap();
        (0..h.len() / 2)
            .map(|i| u8::from_str_radix(&h[2 * i..2 * i + 2], 16).unwrap())
            .collect()
    };
    let mut cases: Vec<XkbCkmCase> = Vec::new();
    let mut in_total = false;
    for line in text.lines() {
        if line.starts_with("## + ") || (line.starts_with('#') && !line.starts_with("## ")) {
            continue;
        }
        if let Some(rest) = line.strip_prefix("## ") {
            let opts = field(line, "options").unwrap();
            cases.push(XkbCkmCase {
                name: rest.split(' ').next().unwrap().to_owned(),
                layout: field(line, "layout").unwrap(),
                options: (opts != "-").then_some(opts),
                steps: Vec::new(),
            });
            in_total = false;
        } else if let Some(rest) = line.strip_prefix("! ") {
            cases.push(XkbCkmCase {
                name: format!("edge {rest}"),
                layout: "gb".into(),
                options: None,
                steps: Vec::new(),
            });
            in_total = false;
        } else if line == "> total" {
            in_total = true;
        } else if let Some(req) = line.strip_prefix("> ckm:") {
            let mut it = req.splitn(4, ':');
            let first = it.next().unwrap().parse().unwrap();
            let kpk = it.next().unwrap().parse().unwrap();
            let count = it.next().unwrap().parse().unwrap();
            let syms = it
                .next()
                .unwrap()
                .split(',')
                .filter(|s| !s.is_empty())
                .map(|h| u32::from_str_radix(h, 16).unwrap())
                .collect();
            cases.last_mut().unwrap().steps.push(XkbCkmStep {
                first,
                kpk,
                count,
                syms,
                ..XkbCkmStep::default()
            });
        } else if in_total {
            continue;
        } else {
            let step = cases.last_mut().unwrap().steps.last_mut().unwrap();
            if line == "= ok" {
                step.ok = true;
            } else if line.starts_with("= error") {
                step.ok = false;
            } else if line.starts_with("e xkb ") {
                if field(line, "dev").as_deref() == Some("3") {
                    step.listener.push(raw(line));
                }
            } else if line.starts_with("e xkbl ") {
                step.listener.push(raw(line));
            } else if line.starts_with("e core ") {
                step.plain.push(raw(line));
            } else if let Some(r) = line.strip_prefix("repeat ") {
                let (kc, change) = r.split_once(' ').unwrap();
                let (a, b) = change.split_once("->").unwrap();
                step.repeats
                    .push((kc.parse().unwrap(), a.parse().unwrap(), b.parse().unwrap()));
            } else if line.starts_with("- ") {
                let (kc, row) = parse_xkb_key_row(line);
                step.before.insert(kc, row);
            } else if line.starts_with("+ ") {
                let (kc, row) = parse_xkb_key_row(line);
                step.after.insert(kc, row);
            } else if line.starts_with("coremodmap") {
            } else {
                panic!("unparsed golden line: {line}");
            }
        }
    }
    cases
}

/// Our XKB GetMap of the whole description, in the goldens' grammar
/// (decoded by the probe's printer port, which checks the wire layout).
struct XkbMapView {
    /// `type N` lines by Xorg index.
    types: Vec<String>,
    /// The VirtualMods section: real mapping per vmod index.
    vmods: [u8; 16],
    keys: std::collections::BTreeMap<u8, XkbKeyRow>,
}

fn xkb_map_view(backend: &KmsBackend) -> XkbMapView {
    use crate::kms::xkb_desc::{probe, reply};
    let desc = &backend.core.xkb_desc;
    let map = reply::encode_map(desc, reply::MapRequest::full(desc));
    let (types, vmods, keys) = probe::map_lines(&map);
    XkbMapView {
        types,
        vmods,
        keys: keys
            .into_iter()
            .map(|(kc, l)| parse_xkb_key_row(&format!("+ {kc} {l}")))
            .collect(),
    }
}

/// The cooking gate after a mutation: the installed cooking keymap
/// cooks as the description says.
fn cooking_gate(backend: &KmsBackend, what: &str, failures: &mut Vec<String>) {
    let (bad, _) =
        crate::kms::xkb_desc::gate::check(&backend.core.xkb_desc, &backend.core.xkb_keymap.0);
    for b in bad.into_iter().take(5) {
        failures.push(format!("{what}: cooking: {b}"));
    }
}

/// XKB GetControls through the core loop (which owns the per-key repeat).
fn xkb_get_controls(
    state: &mut yserver_core::server::ServerState,
    backend: &mut KmsBackend,
    peer: &mut std::os::unix::net::UnixStream,
) -> Vec<u8> {
    kbd_map_request(state, backend, 136, 6, &[0x00, 0x01, 0, 0]);
    let r = kbd_map_drain(peer);
    assert_eq!((r[0], r.len()), (1, 92), "GetControls reply");
    r
}

fn per_key_repeat(controls: &[u8], kc: u8) -> u8 {
    (controls[60 + usize::from(kc >> 3)] >> (kc & 7)) & 1
}

/// Golden (Xvfb 21.1.24): what an XKB client sees after ChangeKeyboardMapping,
/// the events in arrival order and XKB GetMap/GetControls of the changed keys.
///
/// Compared exactly, by Xorg index: every key Xorg lists before and
/// after (type indices, group info, width, keysyms, all actions,
/// behaviors, explicit, modmap, vmodmap), every other key unchanged,
/// the type table unchanged. Only where yserver has one XKB keyboard
/// the comparison is by meaning: yserver's is device 1 in every XKB
/// reply and event (Xorg: master 3, plus one MapNotify per slave 5/7,
/// which the listener sees as extra events of other devices), so the
/// listener's core-keyboard (dev 3) events are what we match; and
/// ControlsNotify.enabledControls is our GetControls' value. After
/// every request the cooking gate checks the installed keymap.
#[test]
fn xkb_view_of_change_keyboard_mapping_matches_xorg() {
    let cases = parse_xkb_ckm_golden(include_str!(
        "../../../testdata/xorg-xkb-change-keyboard-mapping.txt"
    ));
    assert_eq!(
        cases.iter().filter(|c| !c.name.starts_with("edge")).count(),
        26,
        "golden parsed"
    );
    let mut failures: Vec<String> = Vec::new();
    for case in &cases {
        let mut backend = kbd_map_backend(&case.layout, case.options.as_deref());
        let mut state = yserver_core::server::ServerState::new();
        let mut listener = kbd_map_client_id(&mut state, 5);
        let mut plain = kbd_map_client_id(&mut state, 6);
        // XkbSelectEvents(all) on the core keyboard, as the probe does.
        yserver_core::core_loop::xkb_select::xkb_select_events(&mut state, 5, 0x0100, 0x0fff);
        yserver_core::core_loop::xkb_layout::seed_keyboard_auto_repeats(&mut state, &backend);
        for step in &case.steps {
            let what = format!(
                "{} ckm:{}:{}:{}",
                case.name, step.first, step.kpk, step.count
            );
            let before = xkb_map_view(&backend);
            let ctl_before = xkb_get_controls(&mut state, &mut backend, &mut listener);
            let _ = kbd_map_drain(&mut plain);
            kbd_map_request(
                &mut state,
                &mut backend,
                100,
                step.count,
                &change_kbd_map_body(step.first, step.kpk, &step.syms),
            );
            let got = kbd_map_drain(&mut listener);
            let got_plain = kbd_map_drain(&mut plain);
            let after = xkb_map_view(&backend);
            let ctl_after = xkb_get_controls(&mut state, &mut backend, &mut listener);
            if !step.ok {
                if got.first() != Some(&0) || got.len() != 32 || !got_plain.is_empty() {
                    failures.push(format!("{what}: expected one error, got {got:x?}"));
                }
                if before.keys != after.keys || before.types != after.types {
                    failures.push(format!("{what}: refused request changed the map"));
                }
                continue;
            }
            cooking_gate(&backend, &what, &mut failures);

            // Events, in arrival order.
            let ours: Vec<&[u8]> = got.chunks(32).collect();
            if ours.len() != step.listener.len() {
                failures.push(format!(
                    "{what}: listener got {} events, xorg {}: {ours:x?}",
                    ours.len(),
                    step.listener.len()
                ));
            }
            for (o, x) in ours.iter().zip(&step.listener) {
                let same = match (x[0], x[1]) {
                    // XKB MapNotify: all but seq/time; device 3 -> our 1.
                    (0x55, 1) => o[..2] == x[..2] && o[8] == 1 && o[9..] == x[9..],
                    // XKB ControlsNotify: enabledControls is ours (GetControls).
                    (0x55, 3) => {
                        o[..2] == x[..2]
                            && o[8] == 1
                            && o[9..16] == x[9..16]
                            && o[16..20] == ctl_after[56..60]
                            && o[20..] == x[20..]
                    }
                    // Core MappingNotify.
                    (t, _) if t & 0x7f == 34 => {
                        o[0] & 0x7f == 34 && o[1] == x[1] && o[4..] == x[4..]
                    }
                    other => panic!("unexpected golden event {other:?}"),
                };
                if !same {
                    failures.push(format!("{what}: event ours {o:02x?} xorg {x:02x?}"));
                }
            }
            let ours_plain: Vec<&[u8]> = got_plain.chunks(32).collect();
            if ours_plain.len() != step.plain.len()
                || ours_plain
                    .iter()
                    .zip(&step.plain)
                    .any(|(o, x)| o[0] & 0x7f != x[0] & 0x7f || o[4..] != x[4..])
            {
                failures.push(format!(
                    "{what}: plain client got {ours_plain:02x?}, xorg {:02x?}",
                    step.plain
                ));
            }

            // GetMap of every key and the type table, by Xorg index.
            for (kc, want) in &step.before {
                if before.keys[kc] != *want {
                    failures.push(format!(
                        "{what}: keycode {kc} before: ours {:x?}, xorg {want:x?}",
                        before.keys[kc]
                    ));
                }
            }
            for kc in 8..=255u8 {
                let want = step.after.get(&kc).unwrap_or(&before.keys[&kc]);
                if after.keys[&kc] != *want {
                    failures.push(format!(
                        "{what}: keycode {kc} after: ours {:x?}, xorg {want:x?}",
                        after.keys[&kc]
                    ));
                }
            }
            if after.types != before.types {
                failures.push(format!("{what}: the key types changed, xorg left them"));
            }

            // Per-key repeat (GetControls).
            for kc in 8..=255u8 {
                let (b, a) = (
                    per_key_repeat(&ctl_before, kc),
                    per_key_repeat(&ctl_after, kc),
                );
                let want = step
                    .repeats
                    .iter()
                    .find(|r| r.0 == kc)
                    .map_or((b, b), |r| (r.1, r.2));
                if (b, a) != want {
                    failures.push(format!(
                        "{what}: per-key repeat of {kc}: ours {b}->{a}, xorg {}->{}",
                        want.0, want.1
                    ));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// A recorded request file (header included), sent raw by `client`.
fn xkb_send_recorded(
    state: &mut yserver_core::server::ServerState,
    backend: &mut KmsBackend,
    client: u32,
    req: &[u8],
) {
    assert_eq!(
        usize::from(u16::from_le_bytes([req[2], req[3]])) * 4,
        req.len()
    );
    xkb_client_request(state, backend, client, 136, req[1], &req[4..]);
}

fn xkb_testdata(path: &str) -> Vec<u8> {
    let path = format!("{}/src/kms/testdata/{path}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// Intern a recording's atoms (`0xATOM NAME` lines, in order) at their
/// recorded values, as the probe's `atoms:` step checks Xorg gives them.
fn intern_recorded_atoms(state: &mut yserver_core::server::ServerState, atoms: &str, what: &str) {
    for atom in atoms.lines() {
        let (v, n) = atom.split_once(' ').unwrap();
        let v = u32::from_str_radix(v.trim_start_matches("0x"), 16).unwrap();
        let id = yserver_protocol::x11::AtomId(v);
        assert!(
            state.atoms.intern_at(id, n) || state.atoms.id_for(n) == Some(id),
            "{what}: atom {n} at {v:#x}"
        );
    }
}

/// Golden (`xorg-xkb-setmap-errors.txt`, Xvfb 21.1.24): every malformed
/// SetMap, SetCompatMap, SetIndicatorMap, SetNames and SetGeometry draws
/// Xorg's error — code, errorValue, minor, major = our XKB opcode — and
/// changes nothing; an XKB request without XkbUseExtension draws
/// BadAccess; the few odd requests Xorg's checks accept are accepted.
/// Each request comes from a fresh client (UseExtension first for
/// `xreq`, none for `xreq0`) on a fresh server, the identity upload's
/// atoms interned first where the golden's probe run interned them.
#[test]
fn xkb_set_request_errors_match_xorg() {
    struct Case {
        atoms: bool,
        kind: String,
        file: String,
        /// `(code, errorValue, minor)`, `None` = accepted.
        error: Option<(u8, u32, u8)>,
    }
    let golden = include_str!("../../../testdata/xorg-xkb-setmap-errors.txt");
    let mut cases: Vec<Case> = Vec::new();
    let (mut atoms, mut step) = (false, None::<String>);
    for line in golden.lines() {
        if line.starts_with("## ") {
            atoms = false;
        } else if let Some(s) = line.strip_prefix("> ") {
            if s.starts_with("atoms:") {
                atoms = true;
            } else {
                step = Some(s.to_owned());
            }
        } else if line == "= ok" || line.starts_with("= error=") {
            let error = line.strip_prefix("= error=").map(|r| {
                let field = |k: &str| {
                    r.split(' ')
                        .find_map(|t| t.strip_prefix(k))
                        .unwrap()
                        .parse::<u32>()
                        .unwrap()
                };
                (
                    u8::try_from(r.split(' ').next().unwrap().parse::<u32>().unwrap()).unwrap(),
                    field("value="),
                    u8::try_from(field("minor=")).unwrap(),
                )
            });
            let (kind, file) = step
                .take()
                .unwrap()
                .split_once(':')
                .map(|(a, b)| (a.to_owned(), b.to_owned()))
                .unwrap();
            cases.push(Case {
                atoms,
                kind,
                file,
                error,
            });
        }
    }
    assert_eq!(cases.len(), 102, "golden parsed");
    assert_eq!(cases.iter().filter(|c| c.error.is_none()).count(), 3);
    let identity_atoms =
        String::from_utf8(xkb_testdata("xkbcomp-requests/identity/atoms.txt")).unwrap();
    let mut failures = Vec::new();
    for c in &cases {
        let file = &c.file;
        let mut backend = kbd_map_backend("gb", None);
        let mut state = yserver_core::server::ServerState::new();
        let mut actor = kbd_map_client_id(&mut state, 7);
        if c.atoms {
            intern_recorded_atoms(&mut state, &identity_atoms, file);
        }
        if c.kind == "xreq" {
            xkb_client_request(&mut state, &mut backend, 7, 136, 0, &[1, 0, 0, 0]);
            let _ = kbd_map_drain(&mut actor);
        }
        let before = backend.core.xkb_desc.clone();
        let req = xkb_testdata(file);
        xkb_send_recorded(&mut state, &mut backend, 7, &req);
        let got = kbd_map_drain(&mut actor);
        let Some((code, value, minor)) = c.error else {
            if !got.is_empty() {
                failures.push(format!("{file}: accepted by Xorg, ours {got:02x?}"));
            }
            continue;
        };
        assert_eq!(req[1], minor, "{file}");
        let want = (0u8, code, value, u16::from(minor), 136u8);
        let ours = (got.len() == 32).then(|| {
            (
                got[0],
                got[1],
                u32::from_le_bytes([got[4], got[5], got[6], got[7]]),
                u16::from_le_bytes([got[8], got[9]]),
                got[10],
            )
        });
        if ours != Some(want) {
            failures.push(format!(
                "{file}: ours {ours:?} ({} bytes), xorg (0, code, value, minor, major) {want:?} (value {value:#x})",
                got.len()
            ));
        }
        if backend.core.xkb_desc != before {
            failures.push(format!(
                "{file}: the refused request changed the description"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The events of one step of `xorg-xkbcomp-steps.txt`, the listener's
/// (core keyboard only: Xorg's copies for devices 5 and 7 dropped) and
/// the plain client's, raw; and the step's delta lines.
struct XkbcompStep {
    ok: bool,
    listener: Vec<Vec<u8>>,
    plain: Vec<Vec<u8>>,
    delta: Vec<String>,
    coremodmap: String,
}

/// A golden in `xorg-xkbcomp-steps.txt`'s grammar: case (its `## case`
/// name, up to a `:`) → its `xreq:` steps, by request file name, and its
/// `down:KC` / `up:KC` / `smmx:` steps, as they are.
fn parse_xkb_request_steps(golden: &str) -> Vec<(String, Vec<(String, XkbcompStep)>)> {
    let raw = |l: &str| -> Vec<u8> {
        let h = l.rsplit("raw=").next().unwrap();
        (0..32)
            .map(|i| u8::from_str_radix(&h[2 * i..2 * i + 2], 16).unwrap())
            .collect()
    };
    let mut cases: Vec<(String, Vec<(String, XkbcompStep)>)> = Vec::new();
    let mut current: Option<(String, XkbcompStep)> = None;
    for line in golden.lines() {
        if let Some(c) = line.strip_prefix("## case ") {
            if let Some(st) = current.take() {
                cases.last_mut().unwrap().1.push(st);
            }
            let name = c.split(':').next().unwrap();
            cases.push((name.to_owned(), Vec::new()));
        } else if let Some(s) = line.strip_prefix("> ") {
            if let Some(st) = current.take() {
                cases.last_mut().unwrap().1.push(st);
            }
            let name = if let Some(req) = s.strip_prefix("xreq:") {
                Some(req.rsplit('/').next().unwrap().trim_end_matches(".bin"))
            } else if s.starts_with("down:") || s.starts_with("up:") || s.starts_with("smmx:") {
                Some(s)
            } else {
                None
            };
            if let Some(name) = name {
                current = Some((
                    name.to_owned(),
                    XkbcompStep {
                        ok: false,
                        listener: Vec::new(),
                        plain: Vec::new(),
                        delta: Vec::new(),
                        coremodmap: String::new(),
                    },
                ));
            }
        } else if let Some((_, st)) = current.as_mut() {
            if line == "= ok" {
                st.ok = true;
            } else if line.starts_with("e xkb ") {
                let e = raw(line);
                // Xorg's device (NewKeyboardNotify, MapNotify: byte 8).
                if e[8] == 3 {
                    st.listener.push(e);
                }
            } else if line.starts_with("e xkbl ") {
                st.listener.push(raw(line));
            } else if line.starts_with("e core ") {
                st.plain.push(raw(line));
            } else if line.starts_with("coremodmap") {
                line.clone_into(&mut st.coremodmap);
            } else {
                st.delta.push(line.to_owned());
            }
        }
    }
    if let Some(st) = current.take() {
        cases.last_mut().unwrap().1.push(st);
    }
    cases
}

/// Our whole state, read back through the core loop by `client` (XKB
/// GetMap/GetControls/GetCompatMap/GetIndicatorMap/GetNames and core
/// GetModifierMapping), in the golden's grammar, keyed as the golden's
/// lines are.
fn xkb_state_through_core_loop(
    state: &mut yserver_core::server::ServerState,
    backend: &mut KmsBackend,
    client: u32,
    peer: &mut std::os::unix::net::UnixStream,
) -> std::collections::BTreeMap<String, String> {
    use crate::kms::xkb_desc::{probe, tests::CapturedState};
    let mut ask =
        |state: &mut yserver_core::server::ServerState, opcode: u8, minor: u8, body: &[u8]| {
            xkb_client_request(state, backend, client, opcode, minor, body);
            let r = kbd_map_drain(peer);
            assert_eq!(
                r[0],
                1,
                "reply to {opcode}/{minor}: {:02x?}",
                &r[..r.len().min(32)]
            );
            r
        };
    let mut get_map = vec![0u8; 24];
    get_map[0..2].copy_from_slice(&0x100u16.to_le_bytes());
    get_map[2] = 0xff;
    let map = ask(state, 136, 8, &get_map);
    let ctl = ask(state, 136, 6, &[0x00, 0x01, 0, 0]);
    let compat = ask(state, 136, 10, &[0x00, 0x01, 0x0f, 1, 0, 0, 0, 0]);
    let indicators = ask(state, 136, 13, &[0x00, 0x01, 0, 0, 0xff, 0xff, 0xff, 0xff]);
    let names = ask(state, 136, 17, &[0x00, 0x01, 0, 0, 0xff, 0x3f, 0, 0]);
    let modmap = ask(state, 119, 0, &[]);
    let atoms = &state.atoms;
    let name = |a: u32| probe::atom_display(a, atoms.name(yserver_protocol::x11::AtomId(a)));
    probe::state_lines(
        &map,
        &ctl,
        &compat,
        &indicators,
        &names,
        &name,
        modmap[1],
        &modmap[32..],
    )
    .into_iter()
    .map(|l| (CapturedState::key(&l), l))
    .collect()
}

/// A captured line against ours: equal, a level name Xorg left
/// uninitialised (`?`, any value), or — only for the parts the request
/// doesn't write (`written` says which type/key rows it does) — one of
/// the listed seed tolerances.
fn xkb_line_matches(xorg: &str, ours: &str, written: &dyn Fn(&str) -> bool) -> bool {
    if xorg == ours {
        return true;
    }
    if xorg.starts_with("levelnames ") {
        let split = |l: &str| -> (String, Vec<String>) {
            let (head, list) = l.split_once(" [").unwrap();
            let list = list.strip_suffix(']').unwrap();
            let mut items = Vec::new();
            let mut rest = list;
            while !rest.is_empty() {
                rest = rest.trim_start();
                let end = if let Some(quoted) = rest.strip_prefix('\'') {
                    quoted.find('\'').unwrap() + 2
                } else {
                    rest.find(' ').unwrap_or(rest.len())
                };
                items.push(rest[..end].to_owned());
                rest = &rest[end..];
            }
            (head.to_owned(), items)
        };
        let ((xh, xi), (oh, oi)) = (split(xorg), split(ours));
        return xh == oh
            && xi.len() == oi.len()
            && xi.iter().zip(&oi).all(|(x, o)| x == "?" || x == o);
    }
    let kind = xorg.split(' ').next().unwrap_or("");
    let seeded_row = match kind {
        "keys" | "geometry" | "alias" | "si" => true,
        "key" | "type" => !written(xorg),
        _ => false,
    };
    seeded_row && crate::kms::xkb_desc::tests::tolerated(xorg, ours)
}

/// A server on a frozen fixture as the probe sees it: an XKB listener
/// (UseExtension + SelectEvents all, all details), a plain core client
/// and an XKB-initialised actor, the recording's atoms interned at their
/// recorded values; and Xorg's state so far (`xorg-xkb-pristine.txt`'s
/// case plus the deltas of the steps replayed), with the type and key
/// rows the SetMaps so far wrote.
struct XkbReplay {
    backend: KmsBackend,
    state: yserver_core::server::ServerState,
    listener: std::os::unix::net::UnixStream,
    plain: std::os::unix::net::UnixStream,
    actor: std::os::unix::net::UnixStream,
    xorg: crate::kms::xkb_desc::tests::CapturedState,
    written_types: std::collections::BTreeSet<usize>,
    written_keys: std::collections::BTreeSet<usize>,
}

impl XkbReplay {
    /// A fresh `layout` server (Xorg's: `pristine_case`) after interning
    /// `atoms` (the recording's `0xATOM NAME` lines, in order, as the
    /// probe's `atoms:` step does).
    fn new(
        what: &str,
        (layout, options, pristine_case): (&str, Option<&str>, &str),
        atoms: &str,
    ) -> Self {
        use crate::kms::xkb_desc::tests::{CapturedState, pristine_lines};
        let mut backend = kbd_map_backend(layout, options);
        let mut state = yserver_core::server::ServerState::new();
        let mut listener = kbd_map_client_id(&mut state, 5);
        let mut plain = kbd_map_client_id(&mut state, 6);
        let mut actor = kbd_map_client_id(&mut state, 7);
        yserver_core::core_loop::xkb_layout::seed_keyboard_auto_repeats(&mut state, &backend);
        intern_recorded_atoms(&mut state, atoms, what);
        for client in [5, 7] {
            xkb_client_request(&mut state, &mut backend, client, 136, 0, &[1, 0, 0, 0]);
        }
        let mut select = Vec::new();
        for v in [0x100u16, 0x0fff, 0, 0x0fff, 0xff, 0xff] {
            select.extend_from_slice(&v.to_le_bytes());
        }
        xkb_client_request(&mut state, &mut backend, 5, 136, 1, &select);
        for peer in [&mut listener, &mut plain, &mut actor] {
            let _ = kbd_map_drain(peer);
        }
        Self {
            backend,
            state,
            listener,
            plain,
            actor,
            xorg: CapturedState::new(&pristine_lines(pristine_case)),
            written_types: std::collections::BTreeSet::new(),
            written_keys: std::collections::BTreeSet::new(),
        }
    }

    /// One golden step `name` on this server, compared with Xorg's `step`:
    /// a recorded XKB request `req` (header included) or an `smmx:`
    /// step's SetModifierMapping (its `sent` line) from the actor,
    /// through the core loop — its events by field on the listener and
    /// the plain client (Xorg's device-3 events, yserver's one device 1),
    /// the whole state afterwards (read back through the core loop)
    /// against Xorg's so far, line for line, and the cooking gate; or a
    /// `down:KC` / `up:KC` key (XTEST in the probe) cooked by the
    /// backend, whose events aren't compared.
    fn step(
        &mut self,
        what: &str,
        name: &str,
        req: Option<&[u8]>,
        step: &XkbcompStep,
        failures: &mut Vec<String>,
    ) {
        for l in &step.delta {
            self.xorg.apply(l);
        }
        if name.starts_with("smmx:") {
            let sent = step
                .delta
                .iter()
                .find_map(|l| l.strip_prefix("sent kpm="))
                .unwrap_or_else(|| panic!("{what}: sent line"));
            assert!(
                step.delta.iter().any(|l| l == "= status=0"),
                "{what}: Xorg applied it"
            );
            let (kpm, keys) = sent.split_once(" keys=").unwrap();
            let keys: Vec<u8> = keys.split(',').map(|k| k.parse().unwrap()).collect();
            xkb_client_request(
                &mut self.state,
                &mut self.backend,
                7,
                118,
                kpm.parse().unwrap(),
                &keys,
            );
            let reply = kbd_map_drain(&mut self.actor);
            if reply.len() != 32 || reply[..2] != [1, 0] {
                failures.push(format!("{what}: SetModifierMapping reply {reply:02x?}"));
            }
            self.compare(what, step, failures);
            return;
        }
        let Some(req) = req else {
            let (pressed, kc) = name
                .strip_prefix("down:")
                .map(|k| (true, k))
                .or_else(|| name.strip_prefix("up:").map(|k| (false, k)))
                .unwrap_or_else(|| panic!("{what}: step {name}"));
            let keycode = kc.parse().unwrap();
            let _ = self
                .backend
                .cook_host_key(yserver_core::host_x11::HostKeyEvent {
                    origin: yserver_core::core_loop::InputOrigin::NestedHost,
                    keycode,
                    pressed,
                    state: 0,
                    root_x: 0,
                    root_y: 0,
                    event_x: 0,
                    event_y: 0,
                    time: 0,
                });
            for peer in [&mut self.listener, &mut self.plain, &mut self.actor] {
                let _ = kbd_map_drain(peer);
            }
            return;
        };
        assert!(step.ok, "{what}: Xorg accepted it");
        xkb_send_recorded(&mut self.state, &mut self.backend, 7, req);
        // The type and key rows a SetMap writes (firstType/nTypes,
        // firstKeySym/nKeySyms, when present).
        if req[1] == 9 {
            let present = u16::from_le_bytes([req[6], req[7]]);
            let range =
                |first: u8, num: u8| usize::from(first)..usize::from(first) + usize::from(num);
            if present & 0x01 != 0 {
                self.written_types.extend(range(req[12], req[13]));
            }
            if present & 0x02 != 0 {
                self.written_keys.extend(range(req[14], req[15]));
            }
        }
        let got_actor = kbd_map_drain(&mut self.actor);
        if !got_actor.is_empty() {
            failures.push(format!("{what}: the actor got {got_actor:02x?}"));
        }
        self.compare(what, step, failures);
    }

    /// A step's events on the listener and the plain client, the whole
    /// state afterwards and the cooking gate, against Xorg's.
    fn compare(&mut self, what: &str, step: &XkbcompStep, failures: &mut Vec<String>) {
        let got = kbd_map_drain(&mut self.listener);
        let got_plain = kbd_map_drain(&mut self.plain);

        // Events.
        let ours: Vec<&[u8]> = got.chunks(32).collect();
        if ours.len() != step.listener.len() {
            failures.push(format!(
                "{what}: listener got {} events, xorg {}: {ours:02x?}",
                ours.len(),
                step.listener.len()
            ));
        }
        for (o, x) in ours.iter().zip(&step.listener) {
            let same = match (x[0] & 0x7f, x[1]) {
                // XkbMapNotify, XkbIndicatorStateNotify,
                // XkbIndicatorMapNotify, XkbNamesNotify,
                // XkbExtensionDeviceNotify: all but seq/time; device 3
                // -> our 1.
                (0x55, 1 | 4 | 5 | 6 | 11) => o[..2] == x[..2] && o[8] == 1 && o[9..] == x[9..],
                // XkbNewKeyboardNotify: devices, ranges, cause (our XKB
                // major), changed; bytes 18.. are Xorg stack.
                (0x55, 0) => {
                    o[..2] == x[..2]
                        && o[8..10] == [1, 1]
                        && o[10..14] == x[10..14]
                        && o[14] == 136
                        && o[15..18] == x[15..18]
                }
                // XkbControlsNotify: enabledControls is ours (GetControls
                // reports RepeatKeys only, the listed `keys` tolerance),
                // the cause our XKB major.
                (0x55, 3) => {
                    o[..2] == x[..2]
                        && o[8] == 1
                        && o[9..16] == x[9..16]
                        && o[16..20] == crate::kms::xkb::XKB_ENABLED_CONTROLS.to_le_bytes()
                        && o[20..26] == x[20..26]
                        && o[26] == 136
                        && o[27..] == x[27..]
                }
                // XkbCompatMapNotify: bytes 16.. are Xorg stack.
                (0x55, 7) => o[..2] == x[..2] && o[8] == 1 && o[9..16] == x[9..16],
                (34, _) => o[0] & 0x7f == 34 && o[1] == x[1] && o[4..] == x[4..],
                other => panic!("{what}: unexpected golden event {other:?}"),
            };
            if !same {
                failures.push(format!("{what}: event ours {o:02x?} xorg {x:02x?}"));
            }
        }
        let ours_plain: Vec<&[u8]> = got_plain.chunks(32).collect();
        if ours_plain.len() != step.plain.len()
            || ours_plain
                .iter()
                .zip(&step.plain)
                .any(|(o, x)| o[0] & 0x7f != x[0] & 0x7f || o[4..] != x[4..])
        {
            failures.push(format!(
                "{what}: plain client got {ours_plain:02x?}, xorg {:02x?}",
                step.plain
            ));
        }

        // The whole state.
        let mut xorg = self.xorg.lines.clone();
        xorg.insert("coremodmap".into(), step.coremodmap.clone());
        let mut ours =
            xkb_state_through_core_loop(&mut self.state, &mut self.backend, 5, &mut self.listener);
        let modmap_key = ours
            .keys()
            .find(|k| k.starts_with("coremodmap"))
            .cloned()
            .expect("coremodmap line");
        let modmap = ours.remove(&modmap_key).unwrap_or_default();
        ours.insert("coremodmap".into(), modmap);
        let written = |line: &str| {
            let mut w = line.split(' ');
            let (kind, n) = (w.next(), w.next().and_then(|n| n.parse::<usize>().ok()));
            match (kind, n) {
                (Some("type"), Some(n)) => self.written_types.contains(&n),
                (Some("key"), Some(n)) => self.written_keys.contains(&n),
                _ => true,
            }
        };
        let keys: std::collections::BTreeSet<&String> = xorg.keys().chain(ours.keys()).collect();
        let mut n = 0;
        for k in keys {
            let (x, o) = (xorg.get(k), ours.get(k));
            let ok = matches!((x, o), (Some(x), Some(o)) if xkb_line_matches(x, o, &written));
            if !ok && n < 20 {
                n += 1;
                failures.push(format!(
                    "{what}: {k}\n  xorg {}\n  ours {}",
                    x.map_or("-", String::as_str),
                    o.map_or("-", String::as_str)
                ));
            }
        }
        cooking_gate(&self.backend, what, failures);
    }
}

/// Replay one recorded XKB request (`req`, header included) on a fresh
/// server ([`XkbReplay`]) and compare with Xorg's `step`. Returns the
/// backend for further checks.
fn replay_xkb_request_step(
    what: &str,
    fixture: (&str, Option<&str>, &str),
    atoms: &str,
    req: &[u8],
    step: &XkbcompStep,
    failures: &mut Vec<String>,
) -> KmsBackend {
    let mut replay = XkbReplay::new(what, fixture, atoms);
    replay.step(what, "", Some(req), step, failures);
    replay.backend
}

/// The xkbcomp upload steps the backend implements: all five (SetMap,
/// SetIndicatorMap, SetCompatMap, SetNames, SetGeometry).
const XKBCOMP_STEPS_IMPLEMENTED: usize = 5;

/// Replay steps `1..=n` of a recorded xkbcomp upload `case` on one
/// server, comparing each with Xorg's (cumulative) state and events.
fn replay_xkbcomp_upload(
    case: &str,
    steps: &[(String, XkbcompStep)],
    n: usize,
    failures: &mut Vec<String>,
) -> XkbReplay {
    let atoms =
        String::from_utf8(xkb_testdata(&format!("xkbcomp-requests/{case}/atoms.txt"))).unwrap();
    let mut replay = XkbReplay::new(case, ("gb", None, "gb"), &atoms);
    for (i, (name, step)) in steps.iter().take(n).enumerate() {
        assert!(
            name.starts_with(&format!("{}-", i + 1)),
            "{case}: step {name}"
        );
        let req = xkb_testdata(&format!("xkbcomp-requests/{case}/{name}.bin"));
        replay.step(&format!("{case} {name}"), name, Some(&req), step, failures);
    }
    replay
}

/// Golden (`xorg-xkbcomp-steps.txt`, Xvfb 21.1.24): every recorded
/// xkbcomp upload replayed byte for byte through the core loop on one
/// server, request after request ([`XkbReplay::step`]): after each of
/// SetMap, SetIndicatorMap, SetCompatMap, SetNames and SetGeometry,
/// Xorg's events on an XKB listener and a plain client, Xorg's whole state so far (level names
/// Xorg left uninitialised skipped; the listed seed tolerances only for
/// rows no SetMap wrote), and the cooking gate.
#[test]
fn xkbcomp_uploads_match_xorg_request_by_request() {
    let cases = parse_xkb_request_steps(include_str!("../../../testdata/xorg-xkbcomp-steps.txt"));
    assert_eq!(cases.len(), 9, "golden parsed");
    let mut failures: Vec<String> = Vec::new();
    for (case, steps) in &cases {
        assert_eq!(steps.len(), 5, "{case}");
        let _ = replay_xkbcomp_upload(case, steps, XKBCOMP_STEPS_IMPLEMENTED, &mut failures);
    }
    assert!(
        failures.is_empty(),
        "{} failures:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// After the `compat` upload's SetIndicatorMap the Caps Lock LED follows
/// a locked Shift instead of the locked Lock (Xorg's map after step 2,
/// `xorg-xkbcomp-steps.txt`) — in the cooking keymap and on the
/// keyboard's LEDs — and after its SetCompatMap <CAPS> sets Control
/// (step 3: the recompute gave it the new Caps_Lock interpret's
/// `SetMods(Control)`).
#[test]
fn xkbcomp_compat_upload_reaches_leds_and_cooking() {
    use yserver_core::host_x11::HostKeyEvent;
    let key = |keycode: u8, pressed: bool| HostKeyEvent {
        origin: yserver_core::core_loop::InputOrigin::NestedHost,
        keycode,
        pressed,
        state: 0,
        root_x: 0,
        root_y: 0,
        event_x: 0,
        event_y: 0,
        time: 0,
    };
    let cases = parse_xkb_request_steps(include_str!("../../../testdata/xorg-xkbcomp-steps.txt"));
    let (_, steps) = cases.iter().find(|(c, _)| c == "compat").unwrap();
    // Caps Lock locked before the upload: its LED is on.
    let atoms = String::from_utf8(xkb_testdata("xkbcomp-requests/compat/atoms.txt")).unwrap();
    let mut replay = XkbReplay::new("compat", ("gb", None, "gb"), &atoms);
    let _ = replay.backend.cook_host_key(key(66, true));
    let _ = replay.backend.cook_host_key(key(66, false));
    let caps = input::Led::CAPSLOCK.bits();
    assert_eq!(
        replay.backend.leds_sent, caps,
        "precondition: Caps Lock LED on"
    );
    for (name, _) in steps.iter().take(2) {
        let req = xkb_testdata(&format!("xkbcomp-requests/compat/{name}.bin"));
        xkb_send_recorded(&mut replay.state, &mut replay.backend, 7, &req);
    }
    // Lock is still locked, but the LED now shows locked Shift.
    assert_eq!(
        replay.backend.leds_sent, 0,
        "Caps Lock LED off: it follows Shift now"
    );
    let st = &mut replay.backend.core.xkb_state.0;
    st.update_mask(0, 0, 0x01, 0, 0, 0);
    assert!(
        st.led_name_is_active(xkbcommon::xkb::LED_NAME_CAPS),
        "locked Shift lights it"
    );
    st.update_mask(0, 0, 0x02, 0, 0, 0);
    assert!(
        !st.led_name_is_active(xkbcommon::xkb::LED_NAME_CAPS),
        "locked Lock doesn't"
    );
    // Step 3 on a clean server: <CAPS> sets Control, locks nothing.
    let mut failures = Vec::new();
    let mut replay = replay_xkbcomp_upload("compat", steps, 3, &mut failures);
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    let _ = replay.backend.cook_host_key(key(66, true));
    let mods = replay
        .backend
        .core
        .xkb_state
        .0
        .serialize_mods(xkbcommon::xkb::STATE_MODS_EFFECTIVE);
    assert_eq!(mods & 0x06, 0x04, "compat: <CAPS> sets Control");
    let _ = replay.backend.cook_host_key(key(66, false));
    let locked = replay
        .backend
        .core
        .xkb_state
        .0
        .serialize_mods(xkbcommon::xkb::STATE_MODS_LOCKED);
    assert_eq!(locked, 0, "compat: no Caps Lock");
    assert_eq!(replay.backend.leds_sent, 0, "no LED");
}

/// Golden (`xorg-xkb-setcompat.txt`, Xvfb 21.1.24): SetCompatMap's
/// interpret replacement, truncation, growth and skipping of the broken
/// Any interpret, group compat maps with virtual modifiers (and the
/// CompatMapNotify a later SetModifierMapping's virtual modifier change
/// sends for them), and SetIndicatorMap's virtual modifiers, ignored
/// realMods byte, a map that lights its indicator, which=0, a map that
/// turns one off and maps none of which is in use (after XTEST Caps
/// Lock): events, whole state and cooking as [`XkbReplay::step`], one
/// fresh gb server per case.
#[test]
fn set_compat_and_indicator_maps_match_xorg() {
    let cases = parse_xkb_request_steps(include_str!("../../../testdata/xorg-xkb-setcompat.txt"));
    assert_eq!(cases.len(), 14, "golden parsed");
    let mut failures: Vec<String> = Vec::new();
    for (case, steps) in &cases {
        let mut replay = XkbReplay::new(case, ("gb", None, "gb"), "");
        for (name, step) in steps {
            let req =
                (!name.contains(':')).then(|| xkb_testdata(&format!("xkb-setcompat/{name}.bin")));
            replay.step(
                &format!("{case} {name}"),
                name,
                req.as_deref(),
                step,
                &mut failures,
            );
        }
    }
    assert!(
        failures.is_empty(),
        "{} failures:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Golden (`xorg-xkb-setnames.txt`, Xvfb 21.1.24): SetNames' parts
/// beyond xkbcomp's upload — virtual modifier and group names
/// (NamesNotify's changedVirtualMods is the group mask), level names
/// alone (nLevelNames = the request's unused nTypes), type names,
/// indicator names (ExtensionDeviceNotify IndicatorNames; the renamed
/// indicator still lights by its map, by index), clearing the key
/// aliases with radio group names, key names, the six component names —
/// and SetGeometry changing the geometry name (NamesNotify
/// GeometryName, then NewKeyboardNotify) or not (NewKeyboardNotify
/// only): events, whole state and cooking as [`XkbReplay::step`], one
/// fresh gb server per case with the identity upload's atoms. After the
/// indicator rename, XTEST Caps Lock lights indicator 0 as on Xorg
/// (its IndicatorStateNotify state); the rest of what Xvfb sends on
/// that key (a slave device switch) isn't compared.
#[test]
fn set_names_and_geometry_match_xorg() {
    let cases = parse_xkb_request_steps(include_str!("../../../testdata/xorg-xkb-setnames.txt"));
    assert_eq!(cases.len(), 11, "golden parsed");
    let atoms = String::from_utf8(xkb_testdata("xkbcomp-requests/identity/atoms.txt")).unwrap();
    let mut failures: Vec<String> = Vec::new();
    let mut lit_checked = 0;
    for (case, steps) in &cases {
        let mut replay = XkbReplay::new(case, ("gb", None, "gb"), &atoms);
        for (name, step) in steps {
            let req =
                (!name.contains(':')).then(|| xkb_testdata(&format!("xkb-setnames/{name}.bin")));
            let what = format!("{case} {name}");
            replay.step(&what, name, req.as_deref(), step, &mut failures);
            // Xorg's lit indicators after the step, where it says:
            // IndicatorStateNotify's state, else ExtensionDeviceNotify's
            // ledState.
            let xorg_lit = step
                .listener
                .iter()
                .find(|e| e[0] & 0x7f == 0x55 && e[1] == 4)
                .map(|e| u32::from_le_bytes([e[12], e[13], e[14], e[15]]))
                .or_else(|| {
                    step.listener
                        .iter()
                        .find(|e| e[0] & 0x7f == 0x55 && e[1] == 11)
                        .map(|e| u32::from_le_bytes([e[20], e[21], e[22], e[23]]))
                });
            if let Some(xorg) = xorg_lit {
                // The keyboard LEDs follow the indicators by index, as
                // Xorg's input drivers do: bit 0 lights Caps Lock.
                let caps = u32::from(xorg & 1 != 0) * input::Led::CAPSLOCK.bits();
                if replay.backend.leds_sent & input::Led::CAPSLOCK.bits() != caps {
                    failures.push(format!(
                        "{what}: keyboard LEDs {:#x}, Caps Lock LED expected {caps:#x}",
                        replay.backend.leds_sent
                    ));
                }
            }
            if name.starts_with("down:") {
                let xorg = xorg_lit.unwrap_or_else(|| panic!("{what}: IndicatorStateNotify"));
                let ours = replay
                    .backend
                    .core
                    .xkb_desc
                    .indicators_lit(&replay.backend.core.xkb_state.0);
                if ours != xorg {
                    failures.push(format!("{what}: lit {ours:#x}, xorg {xorg:#x}"));
                }
                lit_checked += 1;
            }
        }
    }
    assert_eq!(lit_checked, 1);
    assert!(
        failures.is_empty(),
        "{} failures:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Golden (`xorg-xkb-setmap-resize.txt`, Xvfb 21.1.24): a types-only
/// SetMap that changes the level count of a type keys use
/// (`XkbResizeKeyType`): growing FOUR_LEVEL moves the second group of
/// the two-group keys using it to the new stride while their width
/// stays (so they read back shifted); shrinking it clears their levels
/// past the new count. Events, whole state and cooking as
/// [`replay_xkb_request_step`].
#[test]
fn set_map_key_width_resizing_matches_xorg() {
    let cases =
        parse_xkb_request_steps(include_str!("../../../testdata/xorg-xkb-setmap-resize.txt"));
    assert_eq!(cases.len(), 2, "golden parsed");
    let mut failures: Vec<String> = Vec::new();
    for (case, steps) in &cases {
        let (name, step) = &steps[0];
        let req = xkb_testdata(&format!("xkb-setmap-resize/{name}.bin"));
        let _ = replay_xkb_request_step(
            case,
            ("us,ru", Some("grp:alt_shift_toggle"), "us,ru"),
            "",
            &req,
            step,
            &mut failures,
        );
    }
    assert!(
        failures.is_empty(),
        "{} failures:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// After the recorded SetMap of `swap` keycode 38 cooks `s`, and after
/// `capsctrl`'s keycode 66 cooks Control_L and sets Control (Xorg's
/// state after step 1, `xorg-xkbcomp-steps.txt`).
#[test]
fn xkbcomp_set_map_reaches_key_cooking() {
    use yserver_core::host_x11::HostKeyEvent;
    let key = |keycode: u8, pressed: bool| HostKeyEvent {
        origin: yserver_core::core_loop::InputOrigin::NestedHost,
        keycode,
        pressed,
        state: 0,
        root_x: 0,
        root_y: 0,
        event_x: 0,
        event_y: 0,
        time: 0,
    };
    let upload = |case: &str| {
        let mut backend = kbd_map_backend("gb", None);
        let mut state = yserver_core::server::ServerState::new();
        let mut actor = kbd_map_client_id(&mut state, 7);
        xkb_client_request(&mut state, &mut backend, 7, 136, 0, &[1, 0, 0, 0]);
        xkb_send_recorded(
            &mut state,
            &mut backend,
            7,
            &xkb_testdata(&format!("xkbcomp-requests/{case}/1-SetMap.bin")),
        );
        let _ = kbd_map_drain(&mut actor);
        backend
    };
    let sym = |b: &KmsBackend, kc: u32| {
        b.core
            .xkb_state
            .0
            .key_get_one_sym(xkbcommon::xkb::Keycode::new(kc))
            .raw()
    };
    let backend = upload("swap");
    assert_eq!(sym(&backend, 38), 0x73, "swap: <AC01> is s");
    assert_eq!(sym(&backend, 39), 0x61, "swap: <AC02> is a");
    let mut backend = upload("capsctrl");
    assert_eq!(sym(&backend, 66), 0xffe3, "capsctrl: <CAPS> is Control_L");
    let _ = backend.cook_host_key(key(66, true));
    let mods = backend
        .core
        .xkb_state
        .0
        .serialize_mods(xkbcommon::xkb::STATE_MODS_EFFECTIVE);
    assert_eq!(mods & 0x04, 0x04, "capsctrl: <CAPS> sets Control");
    let _ = backend.cook_host_key(key(66, false));
    let locked = backend
        .core
        .xkb_state
        .0
        .serialize_mods(xkbcommon::xkb::STATE_MODS_LOCKED);
    assert_eq!(locked, 0, "capsctrl: no Caps Lock");
}

/// After ChangeKeyboardMapping the key cooks to the new keysyms, since
/// cooking runs on the one edited keymap (golden `one-case-sym`: keycode
/// 10 becomes x/X on Xorg).
#[test]
fn change_keyboard_mapping_reaches_key_cooking() {
    use yserver_core::host_x11::HostKeyEvent;
    let mut backend = kbd_map_backend("gb", None);
    let mut state = yserver_core::server::ServerState::new();
    let mut peer = kbd_map_client(&mut state);
    kbd_map_request(
        &mut state,
        &mut backend,
        100,
        1,
        &change_kbd_map_body(10, 1, &[0x78]),
    );
    let _ = kbd_map_drain(&mut peer);
    let key = |keycode: u8, pressed: bool| HostKeyEvent {
        origin: yserver_core::core_loop::InputOrigin::NestedHost,
        keycode,
        pressed,
        state: 0,
        root_x: 0,
        root_y: 0,
        event_x: 0,
        event_y: 0,
        time: 0,
    };
    let sym = |b: &KmsBackend| {
        b.core
            .xkb_state
            .0
            .key_get_one_sym(xkbcommon::xkb::Keycode::new(10))
            .raw()
    };
    let _ = backend.cook_host_key(key(10, true));
    assert_eq!(sym(&backend), 0x78, "x");
    let _ = backend.cook_host_key(key(10, false));
    let _ = backend.cook_host_key(key(50, true)); // Shift_L
    let _ = backend.cook_host_key(key(10, true));
    assert_eq!(sym(&backend), 0x58, "Shift+x = X");
}

/// XI ChangeDeviceKeyMapping edits the same keymap, so XKB clients get
/// the same MapNotify (Xorg: both reach XkbApplyMappingChange).
#[test]
fn xi_change_device_key_mapping_notifies_xkb_clients() {
    let mut backend = kbd_map_backend("gb", None);
    let mut state = yserver_core::server::ServerState::new();
    let mut peer = kbd_map_client(&mut state);
    yserver_core::core_loop::xkb_select::xkb_select_events(&mut state, 5, 0x0100, 0x0002);
    let mut body = vec![3, 12, 2, 1];
    for s in [0x33u32, 0xa3] {
        body.extend_from_slice(&s.to_le_bytes());
    }
    kbd_map_request(&mut state, &mut backend, 137, 25, &body);
    let ev = kbd_map_drain(&mut peer);
    let map_notify = ev
        .chunks(32)
        .find(|e| e[0] == 85 && e[1] == 1)
        .expect("XkbMapNotify");
    // changed=KeySyms|KeyActions over keycode 12 only, range 8..255.
    assert_eq!(&map_notify[10..20], &[0x12, 0, 8, 255, 0, 0, 12, 1, 12, 1]);
    let view = xkb_map_view(&backend);
    assert_eq!(view.keys[&12].syms, vec![0x33, 0xa3, 0, 0]);
}

/// Golden (Xvfb 21.1.24, `xorg-xkb-set-modifier-mapping.txt`, every case
/// of both layouts): SetModifierMapping edits the real keymap, as Xorg's
/// `change_modmap` → `XkbApplyMappingChange`.
///
/// - reply status (Busy for a held old/new modifier), BadValue and its
///   value, nothing applied on a refusal;
/// - the events in arrival order, all bytes but seq/time: XkbMapNotify
///   with Xorg's action/vmodmap/type ranges, core MappingNotify,
///   ControlsNotify for per-key repeat changes (cause 118),
///   IndicatorMapNotify;
/// - XKB GetMap before and after, exactly and by Xorg index: every key
///   Xorg lists (types, keysyms, all actions, behaviors, explicit,
///   modmap, vmodmap), the others unchanged; the key types Xorg lists
///   and the virtual modifier table, the others unchanged;
/// - GetControls per-key repeat; GetModifierMapping readback;
/// - the cooking gate after every request.
///
/// Comparisons by meaning where yserver has one keyboard (as in the
/// ChangeKeyboardMapping golden): device 3 is our 1; ControlsNotify's
/// enabledControls is ours. Held keys come in through the host key path
/// (XTEST's), whose own NewKeyboardNotify/StateNotify events aren't part
/// of this; `xmodmap-direct` is the same requests as `xmodmap-replay`
/// sent by the real client; on a `setxkbmap` reload only what the case
/// is about is compared (per-key repeat kept, the edit gone).
#[test]
fn xkb_view_of_set_modifier_mapping_matches_xorg() {
    let cases = parse_xkb_smm_golden(include_str!(
        "../../../testdata/xorg-xkb-set-modifier-mapping.txt"
    ));
    assert_eq!(cases.len(), 40, "golden parsed");
    let mut failures: Vec<String> = Vec::new();
    for case in cases.iter().filter(|c| c.name != "xmodmap-direct") {
        let mut backend = kbd_map_backend(&case.layout, None);
        let mut state = yserver_core::server::ServerState::new();
        let mut listener = kbd_map_client_id(&mut state, 5);
        let mut plain = kbd_map_client_id(&mut state, 6);
        yserver_core::core_loop::xkb_select::xkb_select_events(&mut state, 5, 0x0100, 0x0fff);
        yserver_core::core_loop::xkb_layout::seed_keyboard_auto_repeats(&mut state, &backend);
        for (n, step) in case.steps.iter().enumerate() {
            let what = format!("{} {} step {n} {:?}", case.name, case.layout, step.request);
            let before = xkb_map_view(&backend);
            let ctl_before = xkb_get_controls(&mut state, &mut backend, &mut listener);
            let _ = kbd_map_drain(&mut plain);
            match &step.request {
                SmmRequest::Down(kc) | SmmRequest::Up(kc) => {
                    let pressed = matches!(step.request, SmmRequest::Down(_));
                    host_key(&mut backend, &mut state, *kc, pressed);
                    let _ = kbd_map_drain(&mut listener);
                    let _ = kbd_map_drain(&mut plain);
                    continue;
                }
                SmmRequest::Run(cmd) => {
                    let layout = cmd.rsplit(' ').next().unwrap();
                    assert!(cmd.contains("setxkbmap"), "{what}: unexpected run step");
                    yserver_core::core_loop::xkb_layout::apply_rules_names_change(
                        &mut state,
                        &mut backend,
                        format!("evdev\0pc105\0{layout}\0\0").as_bytes(),
                    );
                    let _ = kbd_map_drain(&mut listener);
                    let _ = kbd_map_drain(&mut plain);
                    let after = xkb_map_view(&backend);
                    let ctl_after = xkb_get_controls(&mut state, &mut backend, &mut listener);
                    assert!(step.repeats.is_empty(), "golden: repeat kept on reload");
                    for kc in 8..=255u8 {
                        if per_key_repeat(&ctl_before, kc) != per_key_repeat(&ctl_after, kc) {
                            failures
                                .push(format!("{what}: reload changed the per-key repeat of {kc}"));
                        }
                    }
                    for (kc, want) in &step.after {
                        if after.keys[kc].mm != want.mm {
                            failures.push(format!(
                                "{what}: keycode {kc} modmap ours {:#x} xorg {:#x}",
                                after.keys[kc].mm, want.mm
                            ));
                        }
                    }
                    continue;
                }
                SmmRequest::Ckm {
                    first,
                    kpk,
                    count,
                    syms,
                } => kbd_map_request(
                    &mut state,
                    &mut backend,
                    100,
                    *count,
                    &change_kbd_map_body(*first, *kpk, syms),
                ),
                SmmRequest::Smm { kpm, keys } => {
                    kbd_map_request(&mut state, &mut backend, 118, *kpm, keys);
                }
            }
            let got = kbd_map_drain(&mut listener);
            let got_plain = kbd_map_drain(&mut plain);
            let after = xkb_map_view(&backend);
            let ctl_after = xkb_get_controls(&mut state, &mut backend, &mut listener);

            // Reply (last on the listener, the requester) or error.
            let mut ours: Vec<&[u8]> = got.chunks(32).collect();
            let reply = match &step.result {
                SmmResult::Error(code, value) => {
                    let e = ours.pop().unwrap_or_default();
                    let got_err = (e.first(), e.get(1), e.get(4..8));
                    let want_err = (Some(&0), Some(code), Some(&value.to_le_bytes()[..]));
                    if got_err != want_err {
                        failures.push(format!("{what}: error ours {e:02x?}, xorg {code}/{value}"));
                    }
                    None
                }
                SmmResult::Status(st) => {
                    let r = ours.pop().unwrap_or_default();
                    if r.first() != Some(&1) || r.get(1) != Some(st) {
                        failures.push(format!("{what}: reply ours {r:02x?}, xorg status {st}"));
                    }
                    Some(*st)
                }
                _ => Some(0),
            };
            if reply != Some(0) {
                if !ours.is_empty() || !got_plain.is_empty() {
                    failures.push(format!("{what}: refused request sent events {ours:02x?}"));
                }
                if before.keys != after.keys || before.types != after.types {
                    failures.push(format!("{what}: refused request changed the map"));
                }
            }

            // Events, in arrival order.
            if ours.len() != step.listener.len() {
                failures.push(format!(
                    "{what}: listener got {} events, xorg {}: {ours:02x?}",
                    ours.len(),
                    step.listener.len()
                ));
            }
            for (o, x) in ours.iter().zip(&step.listener) {
                let same = match (x[0], x[1]) {
                    // XKB MapNotify / IndicatorMapNotify: all but
                    // seq/time; device 3 -> our 1.
                    (0x55, 1 | 5) => o[..2] == x[..2] && o[8] == 1 && o[9..] == x[9..],
                    (0x55, 3) => {
                        o[..2] == x[..2]
                            && o[8] == 1
                            && o[9..16] == x[9..16]
                            && o[16..20] == ctl_after[56..60]
                            && o[20..] == x[20..]
                    }
                    (t, _) if t & 0x7f == 34 => {
                        o[0] & 0x7f == 34 && o[1] == x[1] && o[4..] == x[4..]
                    }
                    other => panic!("unexpected golden event {other:?}"),
                };
                if !same {
                    failures.push(format!("{what}: event ours {o:02x?} xorg {x:02x?}"));
                }
            }
            let ours_plain: Vec<&[u8]> = got_plain.chunks(32).collect();
            if ours_plain.len() != step.plain.len()
                || ours_plain
                    .iter()
                    .zip(&step.plain)
                    .any(|(o, x)| o[0] & 0x7f != x[0] & 0x7f || o[4..] != x[4..])
            {
                failures.push(format!(
                    "{what}: plain client got {ours_plain:02x?}, xorg {:02x?}",
                    step.plain
                ));
            }

            if reply == Some(0) {
                cooking_gate(&backend, &what, &mut failures);
            }

            // GetMap: Xorg's rows before and after, the rest unchanged.
            for (kc, want) in &step.before {
                if before.keys[kc] != *want {
                    failures.push(format!(
                        "{what}: keycode {kc} before: ours {:x?}, xorg {want:x?}",
                        before.keys[kc]
                    ));
                }
            }
            for kc in 8..=255u8 {
                let want = step.after.get(&kc).unwrap_or(&before.keys[&kc]);
                if after.keys[&kc] != *want {
                    failures.push(format!(
                        "{what}: keycode {kc} after: ours {:x?}, xorg {want:x?}",
                        after.keys[&kc]
                    ));
                }
            }
            for (n, want) in &step.types_before {
                if before.types.get(*n) != Some(want) {
                    failures.push(format!(
                        "{what}: type {n} before: ours {:?}, xorg {want}",
                        before.types.get(*n)
                    ));
                }
            }
            for (n, t) in after.types.iter().enumerate() {
                let want = step
                    .types_after
                    .iter()
                    .find(|(i, _)| *i == n)
                    .map_or_else(|| before.types.get(n), |(_, t)| Some(t));
                if want != Some(t) {
                    failures.push(format!("{what}: type {n} after: ours {t}, xorg {want:?}"));
                }
            }
            if let Some(want) = &step.vmods_before
                && before.vmods.to_vec() != *want
            {
                failures.push(format!(
                    "{what}: vmods before ours {:02x?}, xorg {want:02x?}",
                    before.vmods
                ));
            }
            let want_vmods = step
                .vmods_after
                .as_ref()
                .map_or(before.vmods.to_vec(), Clone::clone);
            if after.vmods.to_vec() != want_vmods {
                failures.push(format!(
                    "{what}: vmods ours {:02x?}, xorg {want_vmods:02x?}",
                    after.vmods
                ));
            }

            // Per-key repeat (GetControls).
            for kc in 8..=255u8 {
                let (b, a) = (
                    per_key_repeat(&ctl_before, kc),
                    per_key_repeat(&ctl_after, kc),
                );
                let want = step
                    .repeats
                    .iter()
                    .find(|r| r.0 == kc)
                    .map_or((b, b), |r| (r.1, r.2));
                if (b, a) != want {
                    failures.push(format!(
                        "{what}: per-key repeat of {kc}: ours {b}->{a}, xorg {}->{}",
                        want.0, want.1
                    ));
                }
            }

            let modmap = get_modifier_mapping_reply(&mut state, &mut backend, &mut listener);
            if modmap != step.coremodmap {
                failures.push(format!(
                    "{what}: GetModifierMapping ours {modmap:?}, xorg {:?}",
                    step.coremodmap
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The xmodmap caps-as-control script
/// (`remove Lock = Caps_Lock`, `keycode 66 = Control_L`,
/// `add Control = Control_L`), replayed request by request as xmodmap
/// sends them (golden `xmodmap-replay`, gb and us), reaches key cooking:
/// keycode 66 cooks to Control_L, holding it sets Control for the next
/// key, and it no longer locks anything.
#[test]
fn xmodmap_caps_as_control_reaches_key_cooking() {
    use yserver_core::host_x11::HostKeyEvent;
    let cases = parse_xkb_smm_golden(include_str!(
        "../../../testdata/xorg-xkb-set-modifier-mapping.txt"
    ));
    for case in cases.iter().filter(|c| c.name == "xmodmap-replay") {
        let mut backend = kbd_map_backend(&case.layout, None);
        let mut state = yserver_core::server::ServerState::new();
        let mut peer = kbd_map_client(&mut state);
        for step in &case.steps {
            match &step.request {
                SmmRequest::Ckm {
                    first,
                    kpk,
                    count,
                    syms,
                } => kbd_map_request(
                    &mut state,
                    &mut backend,
                    100,
                    *count,
                    &change_kbd_map_body(*first, *kpk, syms),
                ),
                SmmRequest::Smm { kpm, keys } => {
                    kbd_map_request(&mut state, &mut backend, 118, *kpm, keys);
                }
                other => panic!("xmodmap-replay step {other:?}"),
            }
        }
        let _ = kbd_map_drain(&mut peer);
        let key = |keycode: u8, pressed: bool| HostKeyEvent {
            origin: yserver_core::core_loop::InputOrigin::NestedHost,
            keycode,
            pressed,
            state: 0,
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            time: 0,
        };
        let layout = &case.layout;
        let press = backend.cook_host_key(key(66, true));
        assert_eq!(press.state & 0x04, 0, "{layout}: pre-press state");
        assert_eq!(
            backend
                .core
                .xkb_state
                .0
                .key_get_one_sym(xkbcommon::xkb::Keycode::new(66))
                .raw(),
            xkbcommon::xkb::keysyms::KEY_Control_L,
            "{layout}: keycode 66 is Control_L"
        );
        assert!(
            backend
                .core
                .xkb_state
                .0
                .mod_name_is_active("Control", xkbcommon::xkb::STATE_MODS_EFFECTIVE),
            "{layout}: holding 66 sets Control"
        );
        let a = backend.cook_host_key(key(38, true));
        assert_eq!(
            a.state & 0x04,
            0x04,
            "{layout}: a key under 66 carries ControlMask"
        );
        let _ = backend.cook_host_key(key(38, false));
        let _ = backend.cook_host_key(key(66, false));
        let st = &backend.core.xkb_state.0;
        assert!(
            !st.mod_name_is_active("Control", xkbcommon::xkb::STATE_MODS_EFFECTIVE),
            "{layout}: released"
        );
        assert_eq!(
            st.serialize_mods(xkbcommon::xkb::STATE_MODS_LOCKED),
            0,
            "{layout}: nothing locked"
        );
    }
}
