use super::{xkb::xkb_get_controls, *};

/// `testdata/xorg-repeat-controls.txt` (Xvfb 21.1.24).
const GOLDEN: &str = include_str!("../../../testdata/xorg-repeat-controls.txt");

/// The value fields of the golden line `<kind>: <step> <fields…>`.
fn golden(kind: &str, step: &str, fields: usize) -> Vec<String> {
    GOLDEN
        .lines()
        .filter_map(|l| l.strip_prefix(kind)?.strip_prefix(": "))
        .find_map(|rest| {
            let mut parts: Vec<&str> = rest.rsplitn(fields + 1, ' ').collect();
            let name = parts.pop()?;
            parts.reverse();
            (name == step).then(|| parts.into_iter().map(str::to_owned).collect())
        })
        .unwrap_or_else(|| panic!("golden has no `{kind}: {step}`"))
}

fn hex(s: &str) -> u32 {
    u32::from_str_radix(s.trim_start_matches("0x"), 16).unwrap()
}

/// A KMS backend with a client 5 that selected every XkbControlsNotify
/// detail on the core keyboard, as the golden's probe did.
fn setup() -> (
    yserver_core::server::ServerState,
    KmsBackend,
    std::os::unix::net::UnixStream,
) {
    let backend = kbd_map_backend("us", None);
    let mut state = yserver_core::server::ServerState::new();
    let peer = kbd_map_client_id(&mut state, 5);
    yserver_core::core_loop::xkb_select::xkb_select_events(&mut state, 5, 0x0100, 0x0008);
    yserver_core::core_loop::xkb_layout::seed_keyboard_auto_repeats(&mut state, &backend);
    (state, backend, peer)
}

/// `XkbSetAutoRepeatRate(delay, interval)`: SetControls on the core
/// keyboard with changeCtrls = RepeatKeys only.
fn set_auto_repeat_rate(delay: u16, interval: u16) -> [u8; 96] {
    let mut body = [0u8; 96];
    body[0..2].copy_from_slice(&0x100u16.to_le_bytes());
    body[28..32].copy_from_slice(&1u32.to_le_bytes());
    body[32..34].copy_from_slice(&delay.to_le_bytes());
    body[34..36].copy_from_slice(&interval.to_le_bytes());
    body
}

/// (repeatDelay, repeatInterval, enabledCtrls) of our GetControls.
fn controls(
    state: &mut yserver_core::server::ServerState,
    backend: &mut KmsBackend,
    peer: &mut std::os::unix::net::UnixStream,
) -> (u16, u16, u32) {
    let r = xkb_get_controls(state, backend, peer);
    (
        u16::from_le_bytes([r[20], r[21]]),
        u16::from_le_bytes([r[22], r[23]]),
        u32::from_le_bytes([r[56], r[57], r[58], r[59]]),
    )
}

/// Golden: a fresh server's GetControls reports Xorg's RepeatKeys delay and
/// interval (`XkbDfltRepeatDelay`/`Interval`) with RepeatKeys enabled.
#[test]
fn get_controls_reports_xorg_repeat_defaults() {
    let (mut state, mut backend, mut peer) = setup();
    let want = golden("controls", "default", 3);
    let (delay, interval, enabled) = controls(&mut state, &mut backend, &mut peer);
    assert_eq!(delay.to_string(), want[0]);
    assert_eq!(interval.to_string(), want[1]);
    assert_eq!(enabled & 1, hex(&want[2]) & 1, "RepeatKeys enabled");
}

/// Golden: `xset r rate`, `xset r off|on` and `xset ±r 38` step by step —
/// each step's ControlsNotify (changedControls, enabledControlChanges,
/// cause; enabledControls as far as RepeatKeys, the rest of our
/// enabledControls is the backend's) or none, and GetControls afterwards.
/// SetControls with a zero delay or interval is Xorg's BadValue.
#[test]
fn repeat_controls_follow_set_controls_and_keyboard_control_as_xorg() {
    enum Req {
        Rate(u16, u16),
        AutoRepeat(Option<u8>, u8),
    }
    let (mut state, mut backend, mut peer) = setup();
    let steps = [
        ("xset r rate 200 20", Req::Rate(200, 50)),
        ("xset r rate 200 20 (again)", Req::Rate(200, 50)),
        ("xset r off", Req::AutoRepeat(None, 0)),
        ("xset r on", Req::AutoRepeat(None, 1)),
        ("xset -r 38", Req::AutoRepeat(Some(38), 0)),
        ("xset r 38", Req::AutoRepeat(Some(38), 1)),
        ("xset r rate", Req::Rate(660, 40)),
    ];
    for (step, req) in steps {
        match req {
            Req::Rate(delay, interval) => xkb_client_request(
                &mut state,
                &mut backend,
                5,
                136,
                7,
                &set_auto_repeat_rate(delay, interval),
            ),
            Req::AutoRepeat(key, mode) => {
                let mut body = Vec::new();
                let mask: u32 = if key.is_some() { 0xc0 } else { 0x80 };
                body.extend_from_slice(&mask.to_le_bytes());
                if let Some(k) = key {
                    body.extend_from_slice(&u32::from(k).to_le_bytes());
                }
                body.extend_from_slice(&u32::from(mode).to_le_bytes());
                xkb_client_request(&mut state, &mut backend, 5, 102, 0, &body);
            }
        }
        let got = kbd_map_drain(&mut peer);
        if GOLDEN.contains(&format!("no-notify: {step}\n")) {
            assert!(got.is_empty(), "{step}: no event, got {got:02x?}");
        } else {
            let want = golden("notify", step, 5);
            assert_eq!(got.len(), 32, "{step}: one ControlsNotify, got {got:02x?}");
            let u32_at =
                |i: usize| u32::from_le_bytes([got[i], got[i + 1], got[i + 2], got[i + 3]]);
            assert_eq!(
                (got[0], got[1], got[8]),
                (85, 3, 1),
                "{step}: XkbControlsNotify"
            );
            assert_eq!(u32_at(12), hex(&want[0]), "{step}: changedControls");
            assert_eq!(u32_at(16) & 1, hex(&want[1]) & 1, "{step}: enabledControls");
            assert_eq!(u32_at(20), hex(&want[2]), "{step}: enabledControlChanges");
            let major = if want[3] == "xkb" {
                136
            } else {
                want[3].parse().unwrap()
            };
            assert_eq!(
                (got[26], got[27]),
                (major, want[4].parse().unwrap()),
                "{step}: cause"
            );
        }
        if let Some(want) = GOLDEN
            .contains(&format!("controls: {step} "))
            .then(|| golden("controls", step, 3))
        {
            let (delay, interval, enabled) = controls(&mut state, &mut backend, &mut peer);
            assert_eq!(
                (delay.to_string(), interval.to_string()),
                (want[0].clone(), want[1].clone()),
                "{step}: GetControls delay/interval"
            );
            assert_eq!(enabled & 1, hex(&want[2]) & 1, "{step}: RepeatKeys enabled");
        }
    }

    for (delay, interval) in [(0, 40), (300, 0)] {
        let step = format!("XkbSetAutoRepeatRate({delay}, {interval})");
        let want = golden("error", &step, 2);
        xkb_client_request(
            &mut state,
            &mut backend,
            5,
            136,
            7,
            &set_auto_repeat_rate(delay, interval),
        );
        let got = kbd_map_drain(&mut peer);
        assert_eq!(got.len(), 32, "{step}: one error, got {got:02x?}");
        assert_eq!((got[0], got[1]), (0, 2), "{step}: {}", want[0]);
        assert_eq!(
            u32::from_le_bytes([got[4], got[5], got[6], got[7]]),
            hex(&want[1]),
            "{step}: errorValue"
        );
        assert_eq!(
            (got[8], got[9], got[10]),
            (7, 0, 136),
            "{step}: minor, major"
        );
        assert_eq!(
            controls(&mut state, &mut backend, &mut peer),
            (660, 40, 1),
            "{step}: refused request changed nothing"
        );
    }
}
