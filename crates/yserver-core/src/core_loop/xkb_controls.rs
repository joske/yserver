//! The XKB controls the core loop owns for every backend: the RepeatKeys
//! delay and interval, the RepeatKeys enabled control (Xorg links it to the
//! core global auto-repeat) and the per-key repeat (the core keyboard
//! feedback's `autoRepeats`). GetControls reports them; SetControls and
//! ChangeKeyboardControl change them and send Xorg's `XkbControlsNotify`.

use crate::{
    backend::Backend,
    core_loop::{fanout::fanout_event_to_clients, xkb_layout::XKB_DEVICE_ID, xkb_select},
    server::ServerState,
};
use yserver_protocol::x11::{self, error};

/// `XkbRepeatKeysMask`: a control bit and a `changeCtrls` bit.
const REPEAT_KEYS_MASK: u32 = 1 << 0;
const INTERNAL_MODS_MASK: u32 = 1 << 28;
const IGNORE_LOCK_MODS_MASK: u32 = 1 << 29;
const PER_KEY_REPEAT_MASK: u32 = 1 << 30;
const CONTROLS_ENABLED_MASK: u32 = 1 << 31;
const ALL_CONTROLS_MASK: u32 = 0xf800_1fff;
const ALL_BOOLEAN_CTRLS_MASK: u32 = 0x0000_1fff;
/// `sz_xkbSetControlsReq` (100) minus the 4-byte request header.
const SET_CONTROLS_BODY_LEN: usize = 96;

/// The controls this module owns, before and after a change: what
/// `XkbComputeControlsNotify` compares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RepeatControls {
    enabled: bool,
    delay: u16,
    interval: u16,
    per_key: [u8; 32],
}

impl RepeatControls {
    pub(crate) fn of(state: &ServerState) -> Self {
        let k = &state.keyboard_control;
        Self {
            enabled: k.global_auto_repeat,
            delay: k.repeat_delay,
            interval: k.repeat_interval,
            per_key: k.auto_repeats,
        }
    }
}

/// `base` (a backend's enabled controls) with RepeatKeys from the core
/// global auto-repeat: `XkbComputeControlsNotify` keeps them equal
/// (xkb/xkbEvents.c:359-362).
pub(crate) fn enabled_controls(state: &ServerState, base: u32) -> u32 {
    base & !REPEAT_KEYS_MASK | u32::from(state.keyboard_control.global_auto_repeat)
}

/// Fill the core-owned fields into a backend's GetControls reply
/// (`xkbGetControlsReply`: repeatDelay @20, repeatInterval @22,
/// enabledCtrls @56, perKeyRepeat @60). XKB serves little-endian clients
/// only.
pub(crate) fn patch_get_controls_reply(state: &ServerState, bytes: &mut [u8]) {
    if bytes.len() < 92 {
        return;
    }
    let k = &state.keyboard_control;
    bytes[20..22].copy_from_slice(&k.repeat_delay.to_le_bytes());
    bytes[22..24].copy_from_slice(&k.repeat_interval.to_le_bytes());
    let base = u32::from_le_bytes([bytes[56], bytes[57], bytes[58], bytes[59]]);
    bytes[56..60].copy_from_slice(&enabled_controls(state, base).to_le_bytes());
    bytes[60..92].copy_from_slice(&k.auto_repeats);
}

/// XKB SetControls (minor 7, `body` after the request header), for the
/// controls this module owns, in `ProcXkbSetControls`' order
/// (xkb/xkb.c:767). The checks run up to the RepeatKeys one; the controls
/// yserver doesn't implement are accepted and left alone. On success sends
/// the ControlsNotify (cause `major`, `X_kbSetControls`).
pub(crate) fn set_controls(
    state: &mut ServerState,
    backend: &dyn Backend,
    major: u8,
    body: &[u8],
) -> Result<(), (u8, u32)> {
    if body.len() != SET_CONTROLS_BODY_LEN {
        return Err((error::BAD_LENGTH, 0));
    }
    let u16_at = |i: usize| u16::from_le_bytes([body[i], body[i + 1]]);
    let u32_at = |i: usize| u32::from_le_bytes([body[i], body[i + 1], body[i + 2], body[i + 3]]);
    let mask_match = |err: u32, affect: u32, value: u32| {
        if value & !affect == 0 {
            Ok(())
        } else {
            Err((
                error::BAD_MATCH,
                xkb_select::err_code2(err, value & !affect),
            ))
        }
    };
    let change = u32_at(28);
    if change & !ALL_CONTROLS_MASK != 0 {
        return Err((
            error::BAD_VALUE,
            xkb_select::err_code2(0x01, change & !ALL_CONTROLS_MASK),
        ));
    }
    if change & INTERNAL_MODS_MASK != 0 {
        mask_match(0x02, body[2].into(), body[3].into())?;
        mask_match(0x03, u16_at(6).into(), u16_at(8).into())?;
    }
    if change & IGNORE_LOCK_MODS_MASK != 0 {
        mask_match(0x04, body[4].into(), body[5].into())?;
        mask_match(0x05, u16_at(10).into(), u16_at(12).into())?;
    }
    let (affect_enabled, enabled) = (u32_at(20), u32_at(24));
    mask_match(0x06, affect_enabled, enabled)?;
    if affect_enabled & !ALL_BOOLEAN_CTRLS_MASK != 0 {
        return Err((
            error::BAD_VALUE,
            xkb_select::err_code2(0x07, affect_enabled & !ALL_BOOLEAN_CTRLS_MASK),
        ));
    }
    let (delay, interval) = (u16_at(32), u16_at(34));
    if change & REPEAT_KEYS_MASK != 0 && (delay < 1 || interval < 1) {
        let code3 = (u32::from(delay) << 16) | u32::from(interval);
        return Err((error::BAD_VALUE, xkb_select::err_code2(0x08, code3)));
    }

    let old = RepeatControls::of(state);
    let k = &mut state.keyboard_control;
    if affect_enabled & REPEAT_KEYS_MASK != 0 {
        k.global_auto_repeat = enabled & REPEAT_KEYS_MASK != 0;
    }
    if change & REPEAT_KEYS_MASK != 0 {
        (k.repeat_delay, k.repeat_interval) = (delay, interval);
    }
    if change & PER_KEY_REPEAT_MASK != 0 {
        // XkbDDXChangeControls copies per_key_repeat into the feedback.
        k.auto_repeats.copy_from_slice(&body[64..96]);
    }
    send_controls_notify(state, backend, old, (major, 7));
    Ok(())
}

/// `XkbComputeControlsNotify` + `XkbSendControlsNotify` for a change of the
/// controls this module owns since `old`: nothing when none changed, else
/// one event to every client selecting a changed control. `cause` is the
/// request's (major, minor); ChangeKeyboardControl's `XkbSetRepeatKeys`
/// leaves it 0 (measured, `kms/testdata/xorg-repeat-controls.txt`).
pub(crate) fn send_controls_notify(
    state: &mut ServerState,
    backend: &dyn Backend,
    old: RepeatControls,
    cause: (u8, u8),
) {
    let new = RepeatControls::of(state);
    let mut changed = 0;
    if old.enabled != new.enabled {
        changed |= CONTROLS_ENABLED_MASK;
    }
    if (old.delay, old.interval) != (new.delay, new.interval) {
        changed |= REPEAT_KEYS_MASK;
    }
    if old.per_key != new.per_key {
        changed |= PER_KEY_REPEAT_MASK;
    }
    if changed == 0 {
        return;
    }
    let (num_groups, base) = backend.xkb_controls_base().unwrap_or((1, 0));
    let notify = x11::XkbControlsNotify {
        device_id: XKB_DEVICE_ID,
        num_groups,
        changed_controls: changed,
        enabled_controls: enabled_controls(state, base),
        enabled_control_changes: if old.enabled == new.enabled {
            0
        } else {
            REPEAT_KEYS_MASK
        },
        keycode: 0,
        event_type: 0,
        request_major: cause.0,
        request_minor: cause.1,
    };
    let xkb_event_base = backend.xkb_info().map_or(0, |(_maj, ev, _err)| ev);
    let recipients = xkb_select::interest_recipients(state, |i| i.controls & changed != 0);
    let _dropped = fanout_event_to_clients(state, &recipients, |buf, seq, order| {
        let _ = x11::write_xkb_controls_notify(buf, order, seq, xkb_event_base, notify);
    });
}
