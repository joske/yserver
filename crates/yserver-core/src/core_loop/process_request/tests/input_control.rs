use super::*;

/// Build a ChangeKeyboardControl body: mask + one u32 per set bit.
fn kbctrl_body(mask: u32, values: &[u32]) -> Vec<u8> {
    let mut body = mask.to_le_bytes().to_vec();
    for v in values {
        body.extend_from_slice(&v.to_le_bytes());
    }
    body
}

fn kbctrl_header() -> RequestHeader {
    RequestHeader {
        opcode: 102,
        data: 0,
        length_units: 2,
    }
}

#[test]
fn change_keyboard_control_stores_bell_fields() {
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    // key-click(0x01)=30, bell-percent(0x02)=80, bell-pitch(0x04)=528,
    // bell-duration(0x08)=200
    let body = kbctrl_body(0x0f, &[30, 80, 528, 200]);
    let _ = handle_change_keyboard_control(
        &mut state,
        ClientId(1),
        SequenceNumber(1),
        kbctrl_header(),
        &body,
    );
    assert_eq!(state.keyboard_control.key_click_percent, 30);
    assert_eq!(state.keyboard_control.bell_percent, 80);
    assert_eq!(state.keyboard_control.bell_pitch, 528);
    assert_eq!(state.keyboard_control.bell_duration, 200);
}

#[test]
fn change_keyboard_control_minus_one_restores_defaults() {
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    state.keyboard_control.bell_percent = 7;
    state.keyboard_control.bell_pitch = 7;
    let neg1 = 0xffff_ffffu32;
    let body = kbctrl_body(0x0f, &[neg1, neg1, neg1, neg1]);
    let _ = handle_change_keyboard_control(
        &mut state,
        ClientId(1),
        SequenceNumber(1),
        kbctrl_header(),
        &body,
    );
    assert_eq!(state.keyboard_control.key_click_percent, 0);
    assert_eq!(state.keyboard_control.bell_percent, 50);
    assert_eq!(state.keyboard_control.bell_pitch, 400);
    assert_eq!(state.keyboard_control.bell_duration, 100);
}

#[test]
fn change_keyboard_control_led_without_mode_is_bad_match() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    // led(0x10)=3 without led-mode(0x20) → BadMatch (Xorg
    // dix/devices.c:2121).
    let body = kbctrl_body(0x10, &[3]);
    let _ = handle_change_keyboard_control(
        &mut state,
        ClientId(1),
        SequenceNumber(1),
        kbctrl_header(),
        &body,
    );
    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes[0], 0);
    assert_eq!(bytes[1], x11::error::BAD_MATCH);
}

#[test]
fn change_keyboard_control_key_without_repeat_mode_is_bad_match() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    // key(0x40)=64 without auto-repeat-mode(0x80) → BadMatch
    // (Xorg dix/devices.c:2158).
    let body = kbctrl_body(0x40, &[64]);
    let _ = handle_change_keyboard_control(
        &mut state,
        ClientId(1),
        SequenceNumber(1),
        kbctrl_header(),
        &body,
    );
    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes[0], 0);
    assert_eq!(bytes[1], x11::error::BAD_MATCH);
}

#[test]
fn change_keyboard_control_led_mask_bits() {
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    // led-mode On for leds 1, 20, 32 → bits 0, 19, 31.
    for led in [1u32, 20, 32] {
        let body = kbctrl_body(0x30, &[led, 1]);
        let _ = handle_change_keyboard_control(
            &mut state,
            ClientId(1),
            SequenceNumber(1),
            kbctrl_header(),
            &body,
        );
    }
    assert_eq!(state.keyboard_control.led_mask, 0x8008_0001);
    // led-mode Off with no led → all off.
    let body = kbctrl_body(0x20, &[0]);
    let _ = handle_change_keyboard_control(
        &mut state,
        ClientId(1),
        SequenceNumber(1),
        kbctrl_header(),
        &body,
    );
    assert_eq!(state.keyboard_control.led_mask, 0);
}

#[test]
fn change_keyboard_control_per_key_and_global_auto_repeat() {
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    // key=64 auto-repeat-mode=Off → clear bit (64>>3=8, bit 0).
    let body = kbctrl_body(0xc0, &[64, 0]);
    let _ = handle_change_keyboard_control(
        &mut state,
        ClientId(1),
        SequenceNumber(1),
        kbctrl_header(),
        &body,
    );
    assert_eq!(state.keyboard_control.auto_repeats[8] & 0x01, 0);
    // global auto-repeat Off (no key).
    let body = kbctrl_body(0x80, &[0]);
    let _ = handle_change_keyboard_control(
        &mut state,
        ClientId(1),
        SequenceNumber(1),
        kbctrl_header(),
        &body,
    );
    assert!(!state.keyboard_control.global_auto_repeat);
}

/// Xorg `DoChangeKeyboardControl` calls `XkbDisableComputedAutoRepeats`
/// for a per-key auto-repeat change: the key gets
/// `XkbExplicitAutoRepeatMask`, so a later mapping change leaves its bit
/// alone (`XkbUpdateDescActions`), while other keys' bits follow the
/// keymap and a change reports `XkbControlsNotify(PerKeyRepeat)`.
#[test]
fn per_key_auto_repeat_set_by_a_client_survives_a_mapping_change() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    crate::core_loop::xkb_select::xkb_select_events(&mut state, 1, 0x0100, 0x0008);
    // key=64 auto-repeat-mode=On (already on by default).
    let body = kbctrl_body(0xc0, &[64, 1]);
    let _ = handle_change_keyboard_control(
        &mut state,
        ClientId(1),
        SequenceNumber(1),
        kbctrl_header(),
        &body,
    );
    assert_eq!(state.keyboard_control.auto_repeats_explicit[8], 0x01);
    let change = crate::backend::KeyboardMappingChange {
        map_notify: x11::XkbMapNotify::default(),
        num_groups: 1,
        enabled_controls: 1,
        repeats: vec![(64, false), (65, false)],
        indicator_map_changed: 0,
        indicator_state: 0,
        compat_changed_groups: 0,
        compat_total_si: 0,
    };
    crate::core_loop::xkb_layout::apply_keyboard_mapping_repeats(
        &mut state,
        85,
        &change,
        (crate::core_loop::xkb_layout::X_CHANGE_KEYBOARD_MAPPING, 0),
    );
    assert_eq!(
        state.keyboard_control.auto_repeats[8] & 0x03,
        0x01,
        "64 keeps the client's bit, 65 follows the keymap"
    );
    let ev = read_all_available(&mut peer);
    assert_eq!(ev.len(), 32, "one ControlsNotify");
    assert_eq!((ev[0], ev[1]), (85, 3));
    assert_eq!(&ev[12..16], &0x4000_0000u32.to_le_bytes());

    // Nothing changes: no event.
    crate::core_loop::xkb_layout::apply_keyboard_mapping_repeats(
        &mut state,
        85,
        &change,
        (crate::core_loop::xkb_layout::X_CHANGE_KEYBOARD_MAPPING, 0),
    );
    assert!(read_all_available(&mut peer).is_empty());
}

/// SetModifierMapping through the dispatcher on a backend without an XKB
/// keymap (the fallback store); returns everything the client received.
fn set_modifier_mapping(
    state: &mut ServerState,
    peer: &mut UnixStream,
    kpm: u8,
    keys: &[u8],
) -> Vec<u8> {
    let mut backend = RecordingBackend::new();
    process_request(
        state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode: 118,
            data: kpm,
            length_units: u32::try_from(1 + keys.len() / 4).unwrap(),
        },
        keys,
        None,
    )
    .expect("SetModifierMapping dispatch");
    read_all_or_buffered(state, 1, peer)
}

/// Xorg `build_modmap_from_modkeymap`: a keycode listed twice is
/// BadValue with value 0 (golden `duplicate-keycode`), before the range
/// check; `check_modmap_change`: a keycode below 8 is BadValue with that
/// keycode, the lowest one (golden `keycode-out-of-range`). Nothing is
/// stored or notified.
#[test]
fn set_modifier_mapping_refusals_as_xorg() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let r = set_modifier_mapping(&mut state, &mut peer, 1, &[50, 0, 37, 0, 0, 0, 0, 50]);
    assert_eq!(r.len(), 32);
    assert_eq!((r[0], r[1]), (0, x11::error::BAD_VALUE), "{r:02x?}");
    assert_eq!(&r[4..8], &0u32.to_le_bytes(), "duplicate: value 0");

    let r = set_modifier_mapping(&mut state, &mut peer, 1, &[50, 5, 37, 3, 0, 0, 0, 0]);
    assert_eq!(r.len(), 32);
    assert_eq!((r[0], r[1]), (0, x11::error::BAD_VALUE));
    assert_eq!(&r[4..8], &3u32.to_le_bytes(), "the lowest keycode below 8");
    assert_eq!(state.modifier_mapping_override, None);
}

/// Xorg `check_modmap_change`: MappingBusy (reply status 1, nothing
/// applied, no MappingNotify) when a key of the new map is down or a key
/// of the old one is — golden `busy-held-key-changes`,
/// `busy-held-modifier-unchanged`, `busy-held-key-becomes-modifier`; a
/// held key that is in neither doesn't block (`held-nonmodifier-
/// unaffected`). Xorg's loop over the old modifiers stops before the
/// last keycode, so a held keycode 255 in the old map doesn't block.
#[test]
fn set_modifier_mapping_busy_as_xorg() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let old = [50u8, 0, 66, 0, 37, 0, 0, 255];
    let r = set_modifier_mapping(&mut state, &mut peer, 1, &old);
    assert_eq!(r.len(), 64, "MappingNotify then the reply: {r:02x?}");
    assert_eq!(r[0] & 0x7f, 34);
    assert_eq!((r[32], r[33]), (1, 0), "Success");

    let press = |state: &mut ServerState, kc: u8, down: bool| {
        if down {
            state.keys_down[usize::from(kc >> 3)] |= 1 << (kc & 7);
        } else {
            state.keys_down[usize::from(kc >> 3)] &= !(1 << (kc & 7));
        }
    };
    let new = [62u8, 0, 66, 0, 37, 0, 0, 0];
    // 50 held: an old modifier, not in the new map.
    press(&mut state, 50, true);
    let r = set_modifier_mapping(&mut state, &mut peer, 1, &new);
    assert_eq!(r.len(), 32, "only the reply: {r:02x?}");
    assert_eq!((r[0], r[1]), (1, 1), "MappingBusy");
    press(&mut state, 50, false);
    // 62 held: a new modifier.
    press(&mut state, 62, true);
    let r = set_modifier_mapping(&mut state, &mut peer, 1, &new);
    assert_eq!((r.len(), r[1]), (32, 1), "MappingBusy");
    press(&mut state, 62, false);
    assert_eq!(state.modifier_mapping_override, Some((1, old.to_vec())));
    // 38 held: neither.
    press(&mut state, 38, true);
    let r = set_modifier_mapping(&mut state, &mut peer, 1, &new);
    assert_eq!((r.len(), r[33]), (64, 0), "applied");
    press(&mut state, 38, false);

    // 255 held and in the old map only: not checked by Xorg.
    let with_255 = [62u8, 0, 66, 0, 37, 0, 0, 255];
    let _ = set_modifier_mapping(&mut state, &mut peer, 1, &with_255);
    press(&mut state, 255, true);
    let r = set_modifier_mapping(&mut state, &mut peer, 1, &new);
    assert_eq!((r.len(), r[33]), (64, 0), "held 255 doesn't block");
    assert_eq!(state.modifier_mapping_override, Some((1, new.to_vec())));
}

#[test]
fn change_keyboard_control_bad_value_does_not_commit() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    // valid bell-percent(0x02)=80 staged, then key-click... order is
    // mask-bit order: key-click(0x01)=101 (out of range) errors and
    // nothing commits.
    let body = kbctrl_body(0x03, &[101, 80]);
    let _ = handle_change_keyboard_control(
        &mut state,
        ClientId(1),
        SequenceNumber(1),
        kbctrl_header(),
        &body,
    );
    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes[0], 0);
    assert_eq!(bytes[1], x11::error::BAD_VALUE);
    assert_eq!(
        u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
        101
    );
    assert_eq!(state.keyboard_control.bell_percent, 50, "no partial commit");
}

#[test]
fn get_keyboard_control_reflects_state() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    state.keyboard_control.key_click_percent = 21;
    state.keyboard_control.bell_percent = 12;
    state.keyboard_control.bell_pitch = 402;
    state.keyboard_control.bell_duration = 222;
    state.keyboard_control.global_auto_repeat = false;
    state.keyboard_control.led_mask = 0x8008_000f;
    let _ = handle_get_keyboard_control(&mut state, ClientId(1), SequenceNumber(1));
    let bytes = read_all_available(&mut peer);
    // reply(0) global_auto_repeat(1) seq(2-3) len(4-7) led_mask(8-11)
    // key_click(12) bell_pct(13) bell_pitch(14-15) bell_duration(16-17)
    assert_eq!(bytes[0], 1);
    assert_eq!(bytes[1], 0, "global_auto_repeat off");
    assert_eq!(
        u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
        0x8008_000f
    );
    assert_eq!(bytes[12], 21);
    assert_eq!(bytes[13], 12);
    assert_eq!(u16::from_le_bytes([bytes[14], bytes[15]]), 402);
    assert_eq!(u16::from_le_bytes([bytes[16], bytes[17]]), 222);
    assert_eq!(&bytes[20..52], &crate::server::DEFAULT_AUTO_REPEATS);
}

#[test]
fn bell_out_of_range_percent_is_bad_value() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let header = RequestHeader {
        opcode: 104,
        data: 101u8, // percent=101 (and -101 = 155u8 also invalid)
        length_units: 1,
    };
    let _ = handle_bell(&mut state, ClientId(1), SequenceNumber(1), header);
    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes[0], 0);
    assert_eq!(bytes[1], x11::error::BAD_VALUE);
}

#[test]
fn change_pointer_control_stores_and_restores() {
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let header = RequestHeader {
        opcode: 105,
        data: 0,
        length_units: 3,
    };
    // accel 12/11, threshold 43, do_accel=1 do_thresh=1
    let body = [12, 0, 11, 0, 43, 0, 1, 1];
    let _ =
        handle_change_pointer_control(&mut state, ClientId(1), SequenceNumber(1), header, &body);
    assert_eq!(state.pointer_control.accel_numerator, 12);
    assert_eq!(state.pointer_control.accel_denominator, 11);
    assert_eq!(state.pointer_control.threshold, 43);
    // -1 everywhere restores defaults 2/1, threshold 4.
    let body = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 1, 1];
    let _ =
        handle_change_pointer_control(&mut state, ClientId(1), SequenceNumber(1), header, &body);
    assert_eq!(state.pointer_control.accel_numerator, 2);
    assert_eq!(state.pointer_control.accel_denominator, 1);
    assert_eq!(state.pointer_control.threshold, 4);
}

#[test]
fn change_pointer_control_zero_denominator_is_bad_value() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let header = RequestHeader {
        opcode: 105,
        data: 0,
        length_units: 3,
    };
    // denominator 0 → BadValue (Xorg dix/devices.c:2362 `<= 0`).
    let body = [2, 0, 0, 0, 4, 0, 1, 0];
    let _ =
        handle_change_pointer_control(&mut state, ClientId(1), SequenceNumber(1), header, &body);
    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes[0], 0);
    assert_eq!(bytes[1], x11::error::BAD_VALUE);
    assert_eq!(
        state.pointer_control.accel_numerator, 2,
        "no partial commit"
    );
}

#[test]
fn get_pointer_control_reflects_state() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    state.pointer_control.accel_numerator = 34;
    state.pointer_control.accel_denominator = 35;
    state.pointer_control.threshold = 36;
    let _ = handle_get_pointer_control(&mut state, ClientId(1), SequenceNumber(1));
    let bytes = read_all_available(&mut peer);
    // reply(0) pad(1) seq(2-3) len(4-7) accel_num(8-9) accel_den(10-11)
    // threshold(12-13)
    assert_eq!(u16::from_le_bytes([bytes[8], bytes[9]]), 34);
    assert_eq!(u16::from_le_bytes([bytes[10], bytes[11]]), 35);
    assert_eq!(u16::from_le_bytes([bytes[12], bytes[13]]), 36);
}
